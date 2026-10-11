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
use codex_extension_api::ThreadIdleCause;
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
use core_test_support::responses;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
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

/// The first continuation after the goal set starts immediately (entry rule:
/// no automatic predecessor), then the second quiet continuation waits the
/// n=2 delay (60s) through the production timer/runtime/Core path as paused
/// time advances alone. Production entry points:
/// `GoalService::apply_runtime_effects` -> `GoalRuntimeHandle::continue_if_idle`
/// (entry-rule `ProceedNormal`, immediate start) for the first turn, then
/// `CodexThread::emit_thread_idle_lifecycle_if_idle` ->
/// `GoalExtension::on_thread_idle` -> `GoalRuntimeHandle::continue_if_idle`
/// (pacing `Wait` arm, `spawn_check_in_timer`) -> `CheckInTimer` fire ->
/// `claim_due_deadline` -> `continue_if_idle` -> Core `start_turn_if_idle`
/// (pacing-ticket admission) -> automatic turn accounting ->
/// `GoalExtension::on_turn_stop` (`note_turn_completed` grows the streak) for
/// the second.
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
    // floor arming).
    tokio::time::pause();
    test.codex
        .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
        .await;
    assert_eq!(mock.requests().len(), 1);
    let (second_deadline, _) = runtime
        .background_wait_state()
        .armed_deadline()
        .expect("second pacing wait should arm");
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
