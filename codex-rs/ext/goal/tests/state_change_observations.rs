//! State-change observations: per-goal-turn counts of committed effects.
//!
//! Each reset source (applied file change, actual plan transition, committed
//! goal mutation, accepted spawn/follow-up) increments the count at its
//! committing handler or event. ToolFinish success, failed tools, identical
//! plans, shell success, wrapper success, and model text never increment.
//! Arbitrary shell side effects are unknown and never inferred.

#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/accounting.rs"]
mod accounting;

use accounting::GoalAccountingState;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolName;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;

const TURN: &str = "turn-1";
const GOAL: &str = "goal-1";

fn start_goal_turn(state: &GoalAccountingState, turn_id: &str) {
    state.start_turn(turn_id, ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active(turn_id, GOAL);
}

fn plan_step(step: &str, status: StepStatus) -> PlanItemArg {
    PlanItemArg {
        step: step.to_string(),
        status,
    }
}

fn file_change_item(status: Option<PatchApplyStatus>) -> TurnItem {
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("src/main.rs"),
        FileChange::Update {
            unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
            move_path: None,
        },
    );
    TurnItem::FileChange(FileChangeItem {
        id: "patch-1".to_string(),
        changes,
        status,
        auto_approved: None,
        stdout: None,
        stderr: None,
    })
}

fn collab_item(tool: CollabAgentTool, status: CollabAgentToolCallStatus) -> TurnItem {
    TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: "collab-1".to_string(),
        tool,
        status,
        sender_thread_id: ThreadId::from_u128(1),
        receiver_thread_ids: vec![ThreadId::from_u128(2)],
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
    })
}

fn subagent_item(kind: SubAgentActivityKind) -> TurnItem {
    TurnItem::SubAgentActivity(SubAgentActivityItem {
        id: "sub-1".to_string(),
        kind,
        agent_thread_id: ThreadId::from_u128(2),
        agent_path: AgentPath::root(),
    })
}

fn shell_success_item() -> TurnItem {
    TurnItem::CommandExecution(CommandExecutionItem {
        sandbox_type: None,
        model_context: None,
        id: "exec-1".to_string(),
        plugin_id: None,
        script_path: None,
        process_id: None,
        command: vec!["ps".to_string()],
        cwd: "file:///tmp".parse().expect("cwd uri"),
        parsed_cmd: Vec::new(),
        source: ExecCommandSource::Agent,
        interaction_input: None,
        status: CommandExecutionStatus::Completed,
        stdout: Some("PID TTY".to_string()),
        stderr: None,
        aggregated_output: None,
        exit_code: Some(0),
        duration: None,
        formatted_output: None,
    })
}

fn progress_claim_item(text: &str) -> TurnItem {
    TurnItem::AgentMessage(AgentMessageItem {
        id: "msg-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: text.to_string(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    })
}

#[test]
fn applied_file_change_sets_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_state_change_for_item(TURN, &file_change_item(Some(PatchApplyStatus::Completed)));
    assert_eq!(1, state.state_change_count(TURN));
    assert!(state.has_observed_state_change(TURN));
}

#[test]
fn failed_or_declined_file_change_does_not_set() {
    for status in [
        None,
        Some(PatchApplyStatus::Failed),
        Some(PatchApplyStatus::Declined),
    ] {
        let state = GoalAccountingState::default();
        start_goal_turn(&state, TURN);
        let label = format!("{status:?}");
        state.record_state_change_for_item(TURN, &file_change_item(status));
        assert_eq!(0, state.state_change_count(TURN), "status: {label}");
        assert!(!state.has_observed_state_change(TURN));
    }
}

#[test]
fn first_plan_counts_as_transition() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    let plan = vec![plan_step("Build", StepStatus::InProgress)];
    assert!(state.record_plan_transition(TURN, &plan));
    assert_eq!(1, state.state_change_count(TURN));
}

#[test]
fn identical_plan_resubmission_does_not_set() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    let plan = vec![
        plan_step("Build", StepStatus::InProgress),
        plan_step("Test", StepStatus::Pending),
    ];
    assert!(state.record_plan_transition(TURN, &plan));
    assert!(!state.record_plan_transition(TURN, &plan));
    assert_eq!(1, state.state_change_count(TURN));
}

#[test]
fn plan_status_change_counts_as_transition() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    let before = vec![plan_step("Build", StepStatus::InProgress)];
    let after = vec![plan_step("Build", StepStatus::Completed)];
    assert!(state.record_plan_transition(TURN, &before));
    assert!(state.record_plan_transition(TURN, &after));
    assert_eq!(2, state.state_change_count(TURN));
}

#[test]
fn plan_step_text_change_counts_as_transition() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    assert!(state.record_plan_transition(TURN, &[plan_step("Build", StepStatus::Pending)]));
    assert!(state.record_plan_transition(TURN, &[plan_step("Build release", StepStatus::Pending)]));
    assert_eq!(2, state.state_change_count(TURN));
}

#[test]
fn goal_mutation_sets_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_goal_mutation(TURN);
    assert_eq!(1, state.state_change_count(TURN));
    assert!(state.has_observed_state_change(TURN));
}

#[test]
fn goal_mutation_for_unknown_turn_is_ignored() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_goal_mutation("no-such-turn");
    assert_eq!(0, state.state_change_count("no-such-turn"));
    assert_eq!(0, state.state_change_count(TURN));
}

#[test]
fn spawn_agent_completed_sets_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_state_change_for_item(
        TURN,
        &collab_item(
            CollabAgentTool::SpawnAgent,
            CollabAgentToolCallStatus::Completed,
        ),
    );
    assert_eq!(1, state.state_change_count(TURN));
}

#[test]
fn send_input_completed_sets_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_state_change_for_item(
        TURN,
        &collab_item(
            CollabAgentTool::SendInput,
            CollabAgentToolCallStatus::Completed,
        ),
    );
    assert_eq!(1, state.state_change_count(TURN));
}

#[test]
fn subagent_started_and_interacted_set_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_state_change_for_item(TURN, &subagent_item(SubAgentActivityKind::Started));
    assert_eq!(1, state.state_change_count(TURN));
    state.record_state_change_for_item(TURN, &subagent_item(SubAgentActivityKind::Interacted));
    assert_eq!(2, state.state_change_count(TURN));
}

#[test]
fn spawn_rejections_and_non_spawn_tools_do_not_set() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    for item in [
        collab_item(
            CollabAgentTool::SpawnAgent,
            CollabAgentToolCallStatus::Failed,
        ),
        collab_item(
            CollabAgentTool::SpawnAgent,
            CollabAgentToolCallStatus::InProgress,
        ),
        collab_item(CollabAgentTool::Wait, CollabAgentToolCallStatus::Completed),
        collab_item(
            CollabAgentTool::CloseAgent,
            CollabAgentToolCallStatus::Completed,
        ),
        collab_item(
            CollabAgentTool::ResumeAgent,
            CollabAgentToolCallStatus::Completed,
        ),
        collab_item(
            CollabAgentTool::ListAgents,
            CollabAgentToolCallStatus::Completed,
        ),
        collab_item(
            CollabAgentTool::InterruptAgent,
            CollabAgentToolCallStatus::Completed,
        ),
        subagent_item(SubAgentActivityKind::Interrupted),
        subagent_item(SubAgentActivityKind::Completed),
    ] {
        state.record_state_change_for_item(TURN, &item);
    }
    assert_eq!(0, state.state_change_count(TURN));
}

#[test]
fn failed_tool_does_not_set_observation() {
    for outcome in [
        ToolCallOutcome::Completed { success: false },
        ToolCallOutcome::Completed { success: true },
        ToolCallOutcome::Failed {
            handler_executed: true,
        },
        ToolCallOutcome::Failed {
            handler_executed: false,
        },
        ToolCallOutcome::Blocked,
        ToolCallOutcome::Aborted,
    ] {
        let state = GoalAccountingState::default();
        start_goal_turn(&state, TURN);
        state.record_tool_outcome(TURN, &ToolName::plain("exec"), outcome);
        assert_eq!(
            0,
            state.state_change_count(TURN),
            "outcome: {outcome:?} must not set observation"
        );
    }
}

#[test]
fn shell_success_does_not_set_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_tool_outcome(
        TURN,
        &ToolName::plain("shell"),
        ToolCallOutcome::Completed { success: true },
    );
    state.record_state_change_for_item(TURN, &shell_success_item());
    assert_eq!(0, state.state_change_count(TURN));
    assert!(!state.has_observed_state_change(TURN));
}

#[test]
fn code_mode_leaf_counts_once_and_wrapper_adds_none() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    state.record_state_change_for_item(TURN, &file_change_item(Some(PatchApplyStatus::Completed)));
    assert_eq!(1, state.state_change_count(TURN));
    state.record_tool_outcome(
        TURN,
        &ToolName::plain("exec"),
        ToolCallOutcome::Completed { success: true },
    );
    state.record_state_change_for_item(TURN, &shell_success_item());
    assert_eq!(1, state.state_change_count(TURN));

    let wrapper_only = GoalAccountingState::default();
    start_goal_turn(&wrapper_only, TURN);
    wrapper_only.record_tool_outcome(
        TURN,
        &ToolName::plain("exec"),
        ToolCallOutcome::Completed { success: true },
    );
    wrapper_only.record_state_change_for_item(TURN, &shell_success_item());
    assert_eq!(0, wrapper_only.state_change_count(TURN));
}

#[test]
fn model_text_claiming_progress_does_not_set() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    for text in [
        "Made great progress on the goal.",
        "STATE_CHANGE: applied the fix.",
        "Plan updated and goal complete.",
    ] {
        let item = progress_claim_item(text);
        state.record_item(TURN, &item);
        state.record_state_change_for_item(TURN, &item);
    }
    assert_eq!(0, state.state_change_count(TURN));
}

#[test]
fn no_model_writable_bypass_sets_observation() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, TURN);
    let plan = vec![plan_step(
        "STATE_CHANGE bypass attempt",
        StepStatus::Pending,
    )];
    assert!(state.record_plan_transition(TURN, &plan));
    assert!(!state.record_plan_transition(TURN, &plan));
    state.record_tool_outcome(
        TURN,
        &ToolName::plain("state_change"),
        ToolCallOutcome::Completed { success: true },
    );
    let claim = progress_claim_item("STATE-CHANGING outcome observed");
    state.record_item(TURN, &claim);
    state.record_state_change_for_item(TURN, &claim);
    assert_eq!(1, state.state_change_count(TURN));
}

#[test]
fn empty_blocker_still_fires_first_with_no_observation() {
    let empty_final = TurnItem::AgentMessage(AgentMessageItem {
        id: "empty".to_string(),
        content: vec![AgentMessageContent::Text {
            text: " \n".to_string(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    });
    let state = GoalAccountingState::default();
    for turn in 1..=3 {
        let id = turn.to_string();
        state.start_turn(&id, ModeKind::Default, &TokenUsage::default());
        state.mark_turn_goal_active(&id, GOAL);
        state.record_item(&id, &empty_final);
        state.mark_goal_continuation(id.clone());
        assert_eq!(0, state.state_change_count(&id));
        assert_eq!(
            (turn == 3).then(|| GOAL.to_string()),
            state.empty_response_goal(&id),
            "turn: {turn}"
        );
        state.finish_turn(&id);
    }
}

#[test]
fn observation_does_not_disturb_empty_blocker() {
    let empty_final = TurnItem::AgentMessage(AgentMessageItem {
        id: "empty".to_string(),
        content: vec![AgentMessageContent::Text {
            text: " \n".to_string(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    });
    let state = GoalAccountingState::default();
    for turn in 1..=3 {
        let id = turn.to_string();
        state.start_turn(&id, ModeKind::Default, &TokenUsage::default());
        state.mark_turn_goal_active(&id, GOAL);
        state.record_item(&id, &empty_final);
        state.mark_goal_continuation(id.clone());
        state.record_applied_file_change(&id);
        assert_eq!(1, state.state_change_count(&id));
        assert_eq!(
            (turn == 3).then(|| GOAL.to_string()),
            state.empty_response_goal(&id),
            "turn: {turn}"
        );
        state.finish_turn(&id);
    }
}

#[test]
fn observation_is_transient_per_turn() {
    let state = GoalAccountingState::default();
    start_goal_turn(&state, "turn-1");
    state.record_applied_file_change("turn-1");
    assert_eq!(1, state.state_change_count("turn-1"));
    state.finish_turn("turn-1");
    assert_eq!(0, state.state_change_count("turn-1"));
    start_goal_turn(&state, "turn-2");
    assert_eq!(0, state.state_change_count("turn-2"));
    assert!(!state.has_observed_state_change("turn-2"));
}

#[test]
fn fresh_state_carries_no_observation_or_plan_memory() {
    let state = GoalAccountingState::default();
    assert_eq!(0, state.state_change_count(TURN));
    assert!(!state.has_observed_state_change(TURN));
    start_goal_turn(&state, TURN);
    assert!(state.record_plan_transition(TURN, &[plan_step("First", StepStatus::Pending)]));
    assert_eq!(1, state.state_change_count(TURN));
}
