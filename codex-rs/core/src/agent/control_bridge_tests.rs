//! Status-matrix tests drive the production watcher (V1) and terminal router (V2).
use super::*;
use crate::agent::api::AgentTurnOutcome;
use crate::session::TurnInput;
use codex_extension_items::sleep::SleepItem;
use codex_rollout_trace::ThreadTraceContext;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(MultiAgentVersion::V1, AgentStatus::Completed(Some("answer".into())); "v1_completed")]
#[test_case(MultiAgentVersion::V1, AgentStatus::Errored("failure".into()); "v1_error")]
#[test_case(MultiAgentVersion::V1, AgentStatus::Shutdown; "v1_closed")]
#[test_case(MultiAgentVersion::V1, AgentStatus::Interrupted; "v1_interrupted")]
#[test_case(MultiAgentVersion::V2, AgentStatus::Completed(Some("answer".into())); "v2_completed")]
#[test_case(MultiAgentVersion::V2, AgentStatus::Errored("failure".into()); "v2_error")]
#[test_case(MultiAgentVersion::V2, AgentStatus::Shutdown; "v2_closed")]
#[test_case(MultiAgentVersion::V2, AgentStatus::Interrupted; "v2_interrupted")]
#[tokio::test]
async fn native_status_mailbox_fragment(version: MultiAgentVersion, status: AgentStatus) {
    let harness = AgentControlHarness::new().await;
    let (parent_id, parent) = harness.start_thread().await;
    let (child_id, child) = harness.start_thread().await;
    // Keep the scheduler from issuing a model request: this matrix inspects
    // queue data. Integration tests separately assert actual request counts.
    *parent.session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    parent.thread_extension_data().insert(SleepItem {
        id: codex_extension_items::sleep::goal_wait_sleep_id(1),
        duration_ms: 0,
    });
    let child_path = AgentPath::root()
        .join("worker")
        .expect("bridge fixture operation should succeed");
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: (version == MultiAgentVersion::V2).then(|| child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });
    match version {
        MultiAgentVersion::Disabled => unreachable!("native version required"),
        MultiAgentVersion::V1 => {
            harness.control.maybe_start_completion_watcher(
                child_id,
                Some(source),
                child_id.to_string(),
                /*child_agent_path*/ None,
            );
            let turn = child.session.new_default_turn().await;
            let event = match &status {
                AgentStatus::Completed(message) => EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn.sub_id.clone(),
                    started_at: None,
                    last_agent_message: message.clone(),
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                }),
                AgentStatus::Errored(message) => EventMsg::Error(ErrorEvent {
                    message: message.clone(),
                    codex_error_info: None,
                    misalignment: None,
                }),
                AgentStatus::Shutdown => EventMsg::ShutdownComplete,
                AgentStatus::Interrupted => EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some(turn.sub_id.clone()),
                    reason: TurnAbortReason::Interrupted,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                }),
                AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::NotFound => {
                    unreachable!()
                }
            };
            child.session.send_event(&turn, event).await;
        }
        MultiAgentVersion::V2 => {
            harness
                .control
                .notify_parent_of_terminal_turn(
                    AgentTurnOutcome {
                        thread_id: child_id,
                        turn_id: "terminal".into(),
                        source,
                        parent_turn_id: None,
                        initiating_agent_path: None,
                        status: status.clone(),
                    },
                    &ThreadTraceContext::disabled(),
                )
                .await;
        }
    }
    assert!(wait_for_subagent_notification(&parent).await);
    assert!(!has_subagent_notification(
        parent.session.clone_history().await.raw_items()
    ));
    assert!(
        !parent
            .session
            .input_queue
            .has_trigger_turn_mailbox_items()
            .await
    );
    let (items, _) = parent.session.input_queue.drain_mailbox_input_items().await;
    match version {
        MultiAgentVersion::Disabled => unreachable!("native version required"),
        MultiAgentVersion::V1 => {
            assert_eq!(
                items,
                vec![TurnInput::ResponseItem(
                    ContextualUserFragment::into(SubagentNotification::new(
                        child_id.to_string(),
                        status
                    ))
                    .into(),
                )]
            );
            assert!(
                harness
                    .manager
                    .captured_ops()
                    .iter()
                    .all(|(_, op)| !matches!(op, Op::InterAgentCommunication { .. }))
            );
        }
        MultiAgentVersion::V2 => {
            let message = if matches!(status, AgentStatus::Interrupted) {
                crate::session_prefix::format_inter_agent_interrupted_message(
                    &AgentPath::root(),
                    &child_path,
                )
            } else {
                crate::session_prefix::format_inter_agent_completion_message(
                    AgentPath::root(),
                    child_path.clone(),
                    &status,
                )
                .expect("bridge fixture operation should succeed")
            };
            assert_eq!(
                items,
                vec![TurnInput::InterAgentCommunication(
                    InterAgentCommunication::new(
                        child_path,
                        AgentPath::root(),
                        Vec::new(),
                        message,
                        /*trigger_turn*/ false,
                    )
                )]
            );
        }
    }
    assert!(!parent.session.input_queue.has_pending_mailbox_items().await);
}

#[test_case(AgentStatus::Completed(Some("\u{0001}".repeat(10000))); "completed_escaped")]
#[test_case(AgentStatus::Errored("🦀".repeat(10000)); "error_unicode")]
#[test_case(AgentStatus::Errored("\\\"".repeat(10000)); "error_escaped")]
fn native_v1_fragment_bounds_payload(status: AgentStatus) {
    let rendered = SubagentNotification::new("x".repeat(10000), status).render();
    assert!(rendered.len() < 3072, "{} bytes", rendered.len());
    assert!(rendered.contains("[truncated]"));
    let body = rendered
        .strip_prefix("<subagent_notification>")
        .expect("bridge fixture operation should succeed")
        .strip_suffix("</subagent_notification>")
        .expect("bridge fixture operation should succeed")
        .trim();
    let parsed: serde_json::Value =
        serde_json::from_str(body).expect("bridge fixture operation should succeed");
    assert_eq!(parsed["agent_path"], "x".repeat(64));
}
