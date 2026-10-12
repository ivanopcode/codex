//! Goal low-activity pacing through the production runtime under paused time.
//!
//! Builds a real session with the goal extension and pacing enabled, holds no
//! pending work (empty snapshots), and advances paused Tokio time alone. The
//! first continuation after the goal set has no automatic predecessor, so the
//! entry rule starts it immediately; the second quiet continuation then waits
//! the n=2 delay (60s) through the production `CheckInTimer` ->
//! `GoalRuntimeHandle` (`continue_if_idle` pacing `Wait` arm) ->
//! `claim_due_deadline` -> `continue_if_idle` -> Core `start_turn_if_idle`
//! (pacing-ticket admission) re-entry. No manual per-turn
//! evaluate/claim/admit calls; no real sleeps.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_core::config::Config;
use codex_extension_api::AsyncNotificationSupport;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_goal_extension::CheckInClock;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalRuntimeHandle;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_goal_extension::install_with_backend_and_clock;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

struct PausedClock {
    base: Duration,
    start: tokio::time::Instant,
}

impl PausedClock {
    fn new(base: Duration) -> Self {
        Self {
            base,
            start: tokio::time::Instant::now(),
        }
    }
}

impl CheckInClock for PausedClock {
    fn now(&self) -> Duration {
        self.base + self.start.elapsed()
    }

    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let delay = deadline.saturating_sub(self.now());
        Box::pin(tokio::time::sleep(delay))
    }
}

fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    db: &Arc<codex_state::StateRuntime>,
    service: &Arc<GoalService>,
    manager: Weak<codex_core::ThreadManager>,
    clock: Arc<dyn CheckInClock>,
) {
    install_with_backend_and_clock(
        registry,
        db.clone(),
        AnalyticsEventsClient::disabled(),
        /*metrics_client*/ None,
        manager,
        service.clone(),
        |c: &Config| GoalExtensionConfig {
            enabled: c.features.enabled(Feature::Goals),
            max_goal_token_budget: c.max_goal_token_budget,
        },
        clock,
    );
}

async fn fixture(
    server: &wiremock::MockServer,
    clock: Arc<dyn CheckInClock>,
) -> Result<(TestCodex, Arc<codex_state::StateRuntime>, Arc<GoalService>)> {
    let home = Arc::new(TempDir::new()?);
    let mut config = core_test_support::load_default_config_for_test(&home).await;
    config.features.disable(Feature::CurrentTimeReminder)?;
    let db = codex_core::init_state_db(&config)
        .await
        .expect("state database");
    let service = Arc::new(GoalService::new());
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut registry,
        &db,
        &service,
        Weak::new(),
        Arc::clone(&clock),
    );
    // Registered after the goal extension so the production idle path arms
    // the pacing wait before this contributor notifies; `ThreadIdle::wait`
    // then proves the goal idle ran.
    registry.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_home(home)
        .with_extensions(Arc::new(registry.build()))
        .with_config(|c| {
            c.features
                .disable(Feature::CurrentTimeReminder)
                .expect("feature");
        })
        .build_with_auto_env(server)
        .await?;
    test.codex
        .thread_extension_data()
        .remove::<GoalRuntimeHandle>();
    test.codex
        .thread_extension_data()
        .insert(AsyncNotificationSupport::Available);
    let mut bound = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut bound,
        &db,
        &service,
        Arc::downgrade(&test.thread_manager),
        clock,
    );
    for contributor in bound.build().thread_lifecycle_contributors() {
        contributor
            .on_thread_start(ThreadStartInput {
                config: &test.config,
                session_source: &SessionSource::Cli,
                persistent_thread_state_available: true,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &ExtensionData::new(test.session_configured.session_id.to_string()),
                thread_store: test.codex.thread_extension_data(),
            })
            .await;
    }
    Ok((test, db, service))
}

/// One fully empty automatic continuation response: an empty final assistant
/// message (same shape as the pre-existing `thread_goal_empty_responses`
/// suite), which the goal accounting classifies as empty.
fn empty_final_sse(response_id: &str, message_id: &str) -> String {
    let mut empty_final = responses::ev_assistant_message(message_id, "");
    empty_final["item"]["phase"] = json!("final_answer");
    responses::sse(vec![
        responses::ev_response_created(response_id),
        empty_final,
        responses::ev_completed(response_id),
    ])
}

/// Waits until the production idle path arms the pacing wait, then returns
/// the armed `(deadline, generation)`.
///
/// `TurnComplete` is sent before active-turn cleanup, so the test must not
/// read the slot immediately: it pauses Tokio time first (arming and reading
/// then share the frozen clock), yields to let the production teardown run,
/// and polls the slot. The bound uses real time because paused Tokio
/// timeouts never fire.
async fn await_pacing_armed(runtime: &GoalRuntimeHandle) -> (Duration, u64) {
    let start = Instant::now();
    loop {
        if let Some(armed) = runtime.background_wait_state().armed_deadline() {
            return armed;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "pacing wait should arm via the production idle path after the turn"
        );
        tokio::task::yield_now().await;
    }
}

/// The first continuation after the goal set starts immediately (entry rule:
/// no automatic predecessor), then the second quiet continuation waits the
/// n=2 delay (60s) through the production timer/runtime/Core path as paused
/// time advances alone. Production entry points:
/// `GoalService::apply_runtime_effects` -> `GoalRuntimeHandle::continue_if_idle`
/// (entry-rule `ProceedNormal`, immediate start) for the first turn, then the
/// production turn teardown -> `GoalExtension::on_thread_idle` ->
/// `GoalRuntimeHandle::continue_if_idle` (pacing `Wait` arm,
/// `spawn_check_in_timer`) -> `CheckInTimer` fire -> `claim_due_deadline` ->
/// `continue_if_idle` -> Core `start_turn_if_idle` (pacing-ticket admission)
/// -> automatic turn accounting -> `GoalExtension::on_turn_stop`
/// (`note_turn_completed` grows the streak) for the second. The test waits
/// for the production idle path to arm; it never emits idle itself.
#[tokio::test(flavor = "current_thread")]
async fn goal_pacing_first_immediate_second_at_60s() -> Result<()> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse_completed("pace-1"),
            responses::sse_completed("pace-2"),
        ],
    )
    .await;
    let clock: Arc<dyn CheckInClock> = Arc::new(PausedClock::new(Duration::from_secs(600_000)));
    let (test, db, service) = fixture(&server, Arc::clone(&clock)).await?;
    let thread_id = test.session_configured.thread_id;

    let runtime = test
        .codex
        .thread_extension_data()
        .get::<GoalRuntimeHandle>()
        .expect("goal runtime should exist");
    assert!(
        runtime.background_wait_state().is_pacing_enabled(),
        "production thread start should activate pacing"
    );

    let outcome = service
        .set_thread_goal(
            &db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("paced quiet work"),
                status: None,
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
        )
        .await
        .expect("goal creation should succeed");
    let goal_set_at = Instant::now();
    outcome.apply_runtime_effects(&service).await;

    // Entry rule: the first continuation starts without a pacing delay, well
    // under the 30s floor a paced wait would impose.
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(
        goal_set_at.elapsed() < Duration::from_secs(28),
        "first continuation should complete promptly after the goal set, without a 30s pacing floor"
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(runtime.test_marked_goal_continuations().len(), 1);
    assert_eq!(runtime.background_wait_state().pacing_streak(), 1);

    // The second continuation is paced (n=2, 60s): nothing starts early, and
    // the armed deadline sits a full n=2 delay out (strictly above any n=1
    // floor arming). Pause first so arming and reading share the frozen
    // clock, then wait for the production idle path to arm.
    tokio::time::pause();
    let (second_deadline, _) = await_pacing_armed(&runtime).await;
    assert_eq!(mock.requests().len(), 1);
    let remaining = second_deadline.saturating_sub(clock.now());
    assert!(
        remaining > Duration::from_secs(45) && remaining <= Duration::from_secs(60),
        "the n=2 deadline should sit ~60s out, got {remaining:?}"
    );
    tokio::time::advance(remaining + Duration::from_secs(1)).await;
    tokio::time::resume();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(runtime.test_marked_goal_continuations().len(), 2);
    assert_eq!(runtime.background_wait_state().pacing_streak(), 2);
    Ok(())
}

/// AC5 on the production path with pacing ENABLED: three fully empty
/// automatic continuations block the goal before pacing reschedules.
///
/// Turn 1 starts immediately (entry rule), turns 2 and 3 wait their paced
/// n=2 (60s) / n=3 (120s) delays under paused time, and the third empty turn
/// trips the pre-existing three-empty blocker in `GoalExtension::on_turn_stop`
/// (same terminal `Blocked` status as the `thread_goal_empty_responses`
/// suite). After the blocking turn no pacing wait is armed, and advancing
/// past the next pacing delay starts no further turn: the model request count
/// stops at 3. Killer for the `pacing_blocker_skipped_for_paced_turns`
/// mutant, which skips the blocker for paced turns (goal stays `Active`, a
/// pacing wait re-arms, and a fourth request fires). The test waits for the
/// production idle path to arm; it never emits idle itself.
#[tokio::test(flavor = "current_thread")]
async fn goal_pacing_empty_blocker_runs_before_rescheduling() -> Result<()> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            empty_final_sse("empty-1", "empty-final-1"),
            empty_final_sse("empty-2", "empty-final-2"),
            empty_final_sse("empty-3", "empty-final-3"),
        ],
    )
    .await;
    let clock: Arc<dyn CheckInClock> = Arc::new(PausedClock::new(Duration::from_secs(600_000)));
    let (test, db, service) = fixture(&server, Arc::clone(&clock)).await?;
    let thread_id = test.session_configured.thread_id;

    let runtime = test
        .codex
        .thread_extension_data()
        .get::<GoalRuntimeHandle>()
        .expect("goal runtime should exist");
    assert!(
        runtime.background_wait_state().is_pacing_enabled(),
        "production thread start should activate pacing"
    );

    let outcome = service
        .set_thread_goal(
            &db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("empty quiet work"),
                status: None,
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
        )
        .await
        .expect("goal creation should succeed");
    outcome.apply_runtime_effects(&service).await;

    // Turn 1: immediate under the entry rule (no automatic predecessor).
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 1);

    // Turn 2: paced n=2 (60s). Pause first so arming and reading share
    // the frozen clock, then wait for the production idle path to arm.
    tokio::time::pause();
    let (second_deadline, _) = await_pacing_armed(&runtime).await;
    assert_eq!(mock.requests().len(), 1);
    let remaining = second_deadline.saturating_sub(clock.now());
    assert!(
        remaining > Duration::from_secs(45) && remaining <= Duration::from_secs(60),
        "the n=2 deadline should sit ~60s out, got {remaining:?}"
    );
    tokio::time::advance(remaining + Duration::from_secs(1)).await;
    tokio::time::resume();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 2);

    // Turn 3: paced n=3 (120s). Same pause-then-wait for the production arm.
    tokio::time::pause();
    let (third_deadline, _) = await_pacing_armed(&runtime).await;
    assert_eq!(mock.requests().len(), 2);
    let remaining = third_deadline.saturating_sub(clock.now());
    assert!(
        remaining > Duration::from_secs(100) && remaining <= Duration::from_secs(120),
        "the n=3 deadline should sit ~120s out, got {remaining:?}"
    );
    tokio::time::advance(remaining + Duration::from_secs(1)).await;
    tokio::time::resume();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 3);

    // The third fully empty turn blocks the goal before pacing reschedules:
    // same terminal status as the pre-existing empty-response suite, all
    // three turns marked as goal continuations.
    assert_eq!(
        db.thread_goals()
            .get_thread_goal(thread_id)
            .await?
            .expect("goal should exist")
            .status,
        codex_state::ThreadGoalStatus::Blocked
    );
    assert_eq!(runtime.test_marked_goal_continuations().len(), 3);

    // The production idle path for the blocking turn arms nothing (the goal
    // is no longer Active). `ThreadIdle::wait` proves that idle ran before
    // the test pauses; advancing past the next pacing delay (n=4 = 240s)
    // then starts no further turn.
    ThreadIdle::wait(&test.codex).await;
    tokio::time::pause();
    assert_eq!(runtime.background_wait_state().armed_deadline(), None);
    tokio::time::advance(Duration::from_secs(241)).await;
    tokio::time::resume();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnStarted(_))
            })
        )
        .await
        .is_err(),
        "blocked goal must not start another turn after the pacing delay elapses"
    );
    assert_eq!(mock.requests().len(), 3);
    Ok(())
}
