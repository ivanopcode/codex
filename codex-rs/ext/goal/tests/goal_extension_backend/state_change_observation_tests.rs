use codex_extension_api::ExtensionData;
use codex_extension_api::PlanCommitInput;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

use super::GoalExtensionHarness;
use super::seed_thread_metadata;
use super::test_runtime;
use super::test_thread_id;
use super::tool_by_name;
use super::tool_call;

const TURN: &str = "turn-1";

async fn item_completed(harness: &GoalExtensionHarness, turn_id: &str, item: &TurnItem) {
    let turn_store = ExtensionData::new(turn_id);
    for contributor in harness.registry.turn_lifecycle_contributors() {
        contributor
            .on_item_completed(&harness.thread_store, &turn_store, item)
            .await;
    }
}

async fn plan_committed(harness: &GoalExtensionHarness, turn_id: &str, plan: &UpdatePlanArgs) {
    let turn_store = ExtensionData::new(turn_id);
    for contributor in harness.registry.tool_lifecycle_contributors() {
        contributor
            .on_plan_committed(PlanCommitInput {
                thread_store: &harness.thread_store,
                turn_store: &turn_store,
                turn_id,
                call_id: "plan-call",
                plan,
            })
            .await;
    }
}

fn file_change(status: Option<PatchApplyStatus>) -> TurnItem {
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

fn update_plan_arg(steps: Vec<(&str, StepStatus)>) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: Some("test plan".to_string()),
        plan: steps
            .into_iter()
            .map(|(step, status)| PlanItemArg {
                step: step.to_string(),
                status,
            })
            .collect(),
    }
}

#[tokio::test]
async fn backend_file_change_completed_sets_observation() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let harness = GoalExtensionHarness::new(runtime, thread_id).await?;
    harness.start_turn(TURN, &TokenUsage::default()).await;

    item_completed(
        &harness,
        TURN,
        &file_change(Some(PatchApplyStatus::Completed)),
    )
    .await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));
    assert!(harness.runtime_handle().has_observed_state_change(TURN));

    item_completed(&harness, TURN, &file_change(Some(PatchApplyStatus::Failed))).await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));
    Ok(())
}

#[tokio::test]
async fn backend_plan_transition_sets_and_identical_does_not() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let harness = GoalExtensionHarness::new(runtime, thread_id).await?;
    harness.start_turn(TURN, &TokenUsage::default()).await;

    let first = update_plan_arg(vec![("Build", StepStatus::InProgress)]);
    plan_committed(&harness, TURN, &first).await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    plan_committed(&harness, TURN, &first).await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    let second = update_plan_arg(vec![("Build", StepStatus::Completed)]);
    plan_committed(&harness, TURN, &second).await;
    assert_eq!(2, harness.runtime_handle().state_change_count(TURN));
    Ok(())
}

#[tokio::test]
async fn backend_goal_create_and_update_set_but_failed_create_does_not() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let harness = GoalExtensionHarness::new(runtime, thread_id).await?;
    harness.start_turn(TURN, &TokenUsage::default()).await;
    let tools = harness.tools();
    let create_tool = tool_by_name(&tools, "create_goal");

    create_tool
        .handle(tool_call(
            "create_goal",
            "call-create-1",
            json!({ "objective": "ship observations" }),
        ))
        .await?;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    let duplicate = create_tool
        .handle(tool_call(
            "create_goal",
            "call-create-2",
            json!({ "objective": "second goal" }),
        ))
        .await;
    assert!(duplicate.is_err());
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    let update_tool = tool_by_name(&tools, "update_goal");
    update_tool
        .handle(tool_call(
            "update_goal",
            "call-update-1",
            json!({ "status": "complete" }),
        ))
        .await?;
    assert_eq!(2, harness.runtime_handle().state_change_count(TURN));
    Ok(())
}

#[tokio::test]
async fn backend_goal_update_paused_and_blocked_set_observation() -> anyhow::Result<()> {
    for status in ["paused", "blocked"] {
        let runtime = test_runtime().await?;
        let thread_id = test_thread_id()?;
        seed_thread_metadata(runtime.as_ref(), thread_id).await?;
        let harness = GoalExtensionHarness::new(runtime, thread_id).await?;
        harness.start_turn(TURN, &TokenUsage::default()).await;
        let tools = harness.tools();
        let create_tool = tool_by_name(&tools, "create_goal");
        create_tool
            .handle(tool_call(
                "create_goal",
                "call-create",
                json!({ "objective": "pause or block me" }),
            ))
            .await?;
        assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

        let update_tool = tool_by_name(&tools, "update_goal");
        update_tool
            .handle(tool_call(
                "update_goal",
                "call-update",
                json!({ "status": status }),
            ))
            .await?;
        assert_eq!(
            2,
            harness.runtime_handle().state_change_count(TURN),
            "status: {status}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn backend_external_goal_set_and_clear_record_observation() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let harness = GoalExtensionHarness::new(runtime.clone(), thread_id).await?;
    harness.start_turn(TURN, &TokenUsage::default()).await;

    let outcome = harness
        .goal_service
        .set_thread_goal(
            runtime.as_ref(),
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("externally set goal"),
                status: Some(ThreadGoalStatus::Active),
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
        )
        .await?;
    outcome.apply_runtime_effects(&harness.goal_service).await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    let cleared = harness
        .goal_service
        .clear_thread_goal(runtime.as_ref(), thread_id)
        .await?;
    assert!(cleared);
    assert_eq!(2, harness.runtime_handle().state_change_count(TURN));
    Ok(())
}

#[tokio::test]
async fn backend_spawn_followup_sets_but_tool_finish_success_does_not() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let harness = GoalExtensionHarness::new(runtime, thread_id).await?;
    harness.start_turn(TURN, &TokenUsage::default()).await;

    harness
        .notify_tool_finish(TURN, "call-shell", "shell")
        .await;
    assert_eq!(0, harness.runtime_handle().state_change_count(TURN));

    let spawn = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: "spawn-1".to_string(),
        tool: CollabAgentTool::SpawnAgent,
        status: CollabAgentToolCallStatus::Completed,
        sender_thread_id: ThreadId::from_u128(1),
        receiver_thread_ids: vec![ThreadId::from_u128(2)],
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
    });
    item_completed(&harness, TURN, &spawn).await;
    assert_eq!(1, harness.runtime_handle().state_change_count(TURN));

    let interacted = TurnItem::SubAgentActivity(SubAgentActivityItem {
        id: "sub-1".to_string(),
        kind: SubAgentActivityKind::Interacted,
        agent_thread_id: ThreadId::from_u128(2),
        agent_path: AgentPath::root(),
    });
    item_completed(&harness, TURN, &interacted).await;
    assert_eq!(2, harness.runtime_handle().state_change_count(TURN));
    Ok(())
}

#[tokio::test]
async fn backend_fresh_runtime_carries_no_observation() -> anyhow::Result<()> {
    let runtime = test_runtime().await?;
    let thread_id = test_thread_id()?;
    seed_thread_metadata(runtime.as_ref(), thread_id).await?;
    let first = GoalExtensionHarness::new(runtime.clone(), thread_id).await?;
    first.start_turn(TURN, &TokenUsage::default()).await;
    item_completed(
        &first,
        TURN,
        &file_change(Some(PatchApplyStatus::Completed)),
    )
    .await;
    assert_eq!(1, first.runtime_handle().state_change_count(TURN));

    let second = GoalExtensionHarness::new(runtime, thread_id).await?;
    second.start_turn(TURN, &TokenUsage::default()).await;
    assert_eq!(0, second.runtime_handle().state_change_count(TURN));
    assert!(!second.runtime_handle().has_observed_state_change(TURN));
    Ok(())
}
