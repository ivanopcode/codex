//! App-server vertical test: low-activity pacing across two poll-only continuations.
//!
//! Drives the production JSON-RPC surface (`thread/start`, `thread/goal/set`)
//! with real tool dispatch against a scripted mock model server. The scripted
//! model performs two consecutive read-only (`get_goal`) poll turns with no
//! state-changing outcome. Pacing must space them: the first continuation
//! waits the 30s floor (n=1) and the second waits 60s (n=2), proving two
//! read-only polls grow the streak instead of evading it. Request bodies must
//! carry no scheduling text (AC7).
//!
//! Production entry points exercised:
//!
//! - `GoalExtension::on_thread_idle` -> `GoalRuntimeHandle::continue_if_idle`
//!   (pacing `Wait` arms the shared single-slot timer).
//! - `CheckInTimer` fire -> `claim_due_deadline` -> `continue_if_idle` ->
//!   Core `start_turn_if_idle` (pacing-ticket admission).
//! - `GoalExtension::on_turn_stop` -> `note_turn_completed` (streak grows on
//!   zero P4b-1 observations).
//!
//! Timing is real (the production `SystemCheckInClock` drives pacing), so
//! this test takes about 100s: ~30s + turn1 + ~60s + turn2 + an 8s
//! no-early-turn3 window. Lower-bound assertions prove the delays held; the
//! notification timeouts are the upper bound (a stuck runtime fails waiting
//! for the next `turn/started`).

use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use pretty_assertions::assert_eq;
use pretty_assertions::assert_ne;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const PACING_START_TIMEOUT: Duration = Duration::from_secs(150);
const TURN_TIMEOUT: Duration = Duration::from_secs(60);
const NO_EARLY_TURN3_WINDOW: Duration = Duration::from_secs(8);

fn get_goal_call_sse(response_id: &str, call_id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(response_id),
        responses::ev_function_call(call_id, "get_goal", "{}"),
        responses::ev_completed(response_id),
    ])
}

fn final_message_sse(response_id: &str, message_id: &str, text: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(response_id),
        responses::ev_assistant_message(message_id, text),
        responses::ev_completed(response_id),
    ])
}

struct PacingHarness {
    mock: ResponseMock,
    app: TestAppServer,
    thread_id: String,
    _home: TempDir,
    _server: MockServer,
}

async fn start_pacing_harness(scripts: Vec<String>) -> Result<(PacingHarness, Instant)> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(&server, scripts).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let request = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    let thread_id = thread.id.clone();
    let request = app
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": thread_id, "objective": "finish the background work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    let goal_set_at = Instant::now();
    Ok((
        PacingHarness {
            mock,
            app,
            thread_id,
            _home: home,
            _server: server,
        },
        goal_set_at,
    ))
}

async fn await_turn_started(app: &mut TestAppServer) -> Result<TurnStartedNotification> {
    timeout(PACING_START_TIMEOUT, app.read_notification("turn/started")).await?
}

async fn await_turn_completed(app: &mut TestAppServer) -> Result<TurnCompletedNotification> {
    timeout(TURN_TIMEOUT, app.read_notification("turn/completed")).await?
}

/// AC1/AC2: two consecutive poll-only continuations are spaced 30s then 60s.
#[tokio::test]
async fn goal_pacing_two_polls_grow_streak_with_30s_then_60s_delays() -> Result<()> {
    let scripts = vec![
        get_goal_call_sse("pace-r1", "pace-poll-1"),
        final_message_sse("pace-r2", "pace-m2", "waiting on background work"),
        get_goal_call_sse("pace-r3", "pace-poll-2"),
        final_message_sse("pace-r4", "pace-m4", "still waiting on background work"),
    ];
    let (mut harness, goal_set_at) = start_pacing_harness(scripts).await?;

    // First continuation waits the n=1 floor (~30s) after the goal set.
    let first_started = await_turn_started(&mut harness.app).await?;
    let first_delay = goal_set_at.elapsed();
    assert!(
        first_delay >= Duration::from_secs(28),
        "first poll continuation should wait the ~30s floor, waited {first_delay:?}"
    );
    let first_completed = await_turn_completed(&mut harness.app).await?;
    assert_eq!(first_completed.turn.id, first_started.turn.id);
    assert_eq!(TurnStatus::Completed, first_completed.turn.status);
    assert_eq!(None, first_completed.turn.error);
    let first_end_at = Instant::now();

    // Two model requests for the first poll turn (tool call + final message).
    let requests = harness.mock.requests();
    assert_eq!(requests.len(), 2);
    requests[1]
        .function_call_output_text("pace-poll-1")
        .context("get_goal tool result should be attached")?;

    // Second continuation waits the n=2 delay (~60s): the streak grew.
    let second_started = await_turn_started(&mut harness.app).await?;
    assert_ne!(second_started.turn.id, first_started.turn.id);
    let second_delay = first_end_at.elapsed();
    assert!(
        second_delay >= Duration::from_secs(58),
        "second poll continuation should wait the ~60s n=2 delay, waited {second_delay:?}"
    );
    assert!(
        second_delay > first_delay,
        "second delay ({second_delay:?}) should exceed the first ({first_delay:?})"
    );
    let second_completed = await_turn_completed(&mut harness.app).await?;
    assert_eq!(second_completed.turn.id, second_started.turn.id);
    assert_eq!(TurnStatus::Completed, second_completed.turn.status);
    assert_eq!(None, second_completed.turn.error);

    // Four model requests total for two poll turns; nothing model-visible
    // about scheduling in any request body.
    let requests = harness.mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        for marker in ["pacing", "PacingTicket", "continuation_schedule", "streak"] {
            assert!(
                !request.body_contains_text(marker),
                "pacing must leave no model-visible trace, found {marker:?}"
            );
        }
        assert!(
            !request.body_contains_text("source=\"exec_completion\""),
            "poll turns carry no completion fragment"
        );
    }

    // No early third turn: the n=3 delay (~120s) still holds 8s after the
    // second poll turn ended.
    let early_third = timeout(
        NO_EARLY_TURN3_WINDOW,
        harness
            .app
            .read_notification::<TurnStartedNotification>("turn/started"),
    )
    .await;
    assert!(
        early_third.is_err(),
        "third continuation must not start inside the n=3 delay window"
    );
    assert_eq!(
        ThreadGoalStatus::Active,
        goal_status(&mut harness.app, &harness.thread_id).await?
    );
    assert_eq!(
        harness.mock.requests().len(),
        4,
        "no further model requests while the n=3 delay holds"
    );
    Ok(())
}

async fn goal_status(app: &mut TestAppServer, thread_id: &str) -> Result<ThreadGoalStatus> {
    let request = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId": thread_id})))
        .await?;
    let result: ThreadGoalGetResponse = timeout(TURN_TIMEOUT, app.read_response(request)).await??;
    result
        .goal
        .context("goal should exist")
        .map(|goal| goal.status)
}
