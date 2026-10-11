//! Low-activity pacing tests with fake time.
//!
//! Drives [`BackgroundWaitState`] with pacing enabled through its production
//! entry points: [`BackgroundWaitState::evaluate_continuation`] (goal
//! continuation path), [`BackgroundWaitState::check_admission`] (Core
//! admission recheck via the published checker),
//! [`BackgroundWaitState::claim_due_deadline`] (scheduled timer re-entry),
//! and [`BackgroundWaitState::note_turn_completed`] (P4b-1 observation
//! streak). No real sleeps; all deadlines use explicit fake `Duration`
//! values. Delays are asserted against the AC-specified schedule
//! (`min(30s * 2^(n-1), 30min)`), not against implementation constants.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_extension_api::GoalAdmissionDecision;
use codex_extension_api::PendingWorkReceipt;
use codex_extension_api::PendingWorkSnapshot;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolName;
use codex_goal_extension::BackgroundWaitEvaluation;
use codex_goal_extension::BackgroundWaitState;
use codex_goal_extension::GoalWaitStatus;
use codex_goal_extension::pacing_delay;
use codex_goal_extension::pacing_disabled_by_env;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;

#[path = "../src/accounting.rs"]
#[allow(dead_code)]
mod accounting;

use accounting::GoalAccountingState;

fn pacing_state() -> Arc<BackgroundWaitState> {
    let state = Arc::new(BackgroundWaitState::new());
    state.enable_pacing();
    state
}

fn combined_state() -> Arc<BackgroundWaitState> {
    let state = Arc::new(BackgroundWaitState::new());
    state.enable();
    state.enable_pacing();
    state
}

fn pending_snapshot(revision: u64) -> PendingWorkSnapshot {
    PendingWorkSnapshot::new(
        vec![PendingWorkReceipt::new("armed-1")],
        Vec::new(),
        Vec::new(),
        revision,
    )
}

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn mins(value: u64) -> Duration {
    Duration::from_secs(value * 60)
}

/// Models one production idle callback that must keep pacing-waiting: asserts
/// a ticket-free `Wait` with the expected deadline. Every pacing ticket in
/// these tests flows through `claim_due_deadline` first.
fn require_pacing_wait(
    state: &BackgroundWaitState,
    at: Duration,
    revision: u64,
    expected_next: Duration,
) {
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(revision)),
        at,
    );
    assert_eq!(
        evaluation,
        BackgroundWaitEvaluation::Wait {
            next_check_in: Some(expected_next),
            emit_warning: false,
        }
    );
}

/// Models one production timer re-entry for pacing: the fired timer claims
/// the armed registration, then `continue_if_idle` re-evaluates, and Core
/// admission consumes the issued pacing ticket. Returns the ticket streak `n`.
fn require_claimed_pacing_ticket(
    state: &BackgroundWaitState,
    deadline: Duration,
    at: Duration,
    revision: u64,
) -> u32 {
    assert!(
        state.claim_due_deadline(deadline, state.generation(), at),
        "pacing timer for {deadline:?} should fire at {at:?}"
    );
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(revision)),
        at,
    );
    let streak = match evaluation {
        BackgroundWaitEvaluation::ProceedWithPacingTicket { ticket, .. } => ticket.streak(),
        other => panic!("expected pacing ticket at {at:?}, got {other:?}"),
    };
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(revision))),
        GoalAdmissionDecision::Allow
    );
    streak
}

/// Models one production turn lifecycle for pacing: the admitted turn starts
/// (invalidating the ticket generation) and then completes with the given
/// observation.
fn complete_turn(
    state: &BackgroundWaitState,
    turn_id: &str,
    is_goal_continuation: bool,
    had_state_change: bool,
) {
    state.note_turn_start();
    state.note_turn_completed(turn_id, is_goal_continuation, had_state_change);
}

#[test]
fn pacing_delay_n1_n2_n3_and_cap() {
    assert_eq!(pacing_delay(0), secs(30));
    assert_eq!(pacing_delay(1), secs(30));
    assert_eq!(pacing_delay(2), secs(60));
    assert_eq!(pacing_delay(3), secs(120));
    assert_eq!(pacing_delay(4), secs(240));
    assert_eq!(pacing_delay(5), secs(480));
    assert_eq!(pacing_delay(6), secs(960));
    assert_eq!(pacing_delay(7), mins(30));
    assert_eq!(pacing_delay(8), mins(30));
    assert_eq!(pacing_delay(10), mins(30));
    assert_eq!(pacing_delay(100), mins(30));
    assert_eq!(pacing_delay(u32::MAX), mins(30));
}

#[test]
fn pacing_first_evaluation_waits_floor_then_proceeds_with_ticket() {
    let state = pacing_state();
    let start = secs(1000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    assert_eq!(state.pacing_streak(), 0);

    let streak = require_claimed_pacing_ticket(&state, start + secs(30), start + secs(30), 1);
    assert_eq!(streak, 1);
    assert_eq!(state.pacing_outstanding_ticket(), None);
}

#[test]
fn pacing_two_polls_grow_streak() {
    let state = pacing_state();
    let start = secs(5000);

    // First quiet continuation waits the floor (n=1).
    require_pacing_wait(&state, start, 1, start + secs(30));
    let streak = require_claimed_pacing_ticket(&state, start + secs(30), start + secs(30), 1);
    assert_eq!(streak, 1);
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 1);
    assert_eq!(state.pacing_predecessor().as_deref(), Some("turn-1"));

    // Second quiet continuation waits n=2 (60s), not the floor: two
    // read-only polls grow the streak instead of evading it.
    let second_start = start + secs(31);
    require_pacing_wait(&state, second_start, 1, second_start + secs(60));
    let streak =
        require_claimed_pacing_ticket(&state, second_start + secs(60), second_start + secs(60), 1);
    assert_eq!(streak, 2);
    complete_turn(
        &state, "turn-2", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 2);

    // Third quiet continuation waits n=3 (120s).
    let third_start = second_start + secs(61);
    require_pacing_wait(&state, third_start, 1, third_start + secs(120));
}

#[test]
fn pacing_state_change_resets_streak_to_floor() {
    let state = pacing_state();
    let start = secs(7000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    require_claimed_pacing_ticket(&state, start + secs(30), start + secs(30), 1);
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    require_pacing_wait(&state, start + secs(31), 1, start + secs(31) + secs(60));
    require_claimed_pacing_ticket(
        &state,
        start + secs(31) + secs(60),
        start + secs(31) + secs(60),
        1,
    );
    complete_turn(
        &state, "turn-2", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 2);

    // An observed state change resets to the floor even after a streak.
    complete_turn(
        &state, "turn-3", /*is_goal_continuation*/ true, /*had_state_change*/ true,
    );
    assert_eq!(state.pacing_streak(), 0);
    let fourth_start = start + secs(200);
    require_pacing_wait(&state, fourth_start, 1, fourth_start + secs(30));
}

#[test]
fn pacing_non_continuation_turn_does_not_touch_streak() {
    let state = pacing_state();
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 1);

    // Human and other non-continuation turns bind the predecessor but never
    // grow or reset the streak on their own (human input resets separately
    // via note_human_input).
    state.note_turn_completed(
        "human-1", /*is_goal_continuation*/ false, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 1);
    assert_eq!(state.pacing_predecessor().as_deref(), Some("human-1"));
    state.note_turn_completed(
        "human-2", /*is_goal_continuation*/ false, /*had_state_change*/ true,
    );
    assert_eq!(state.pacing_streak(), 1);
    assert_eq!(state.pacing_predecessor().as_deref(), Some("human-2"));
}

#[test]
fn pacing_human_input_resets_streak_to_floor() {
    let state = pacing_state();
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    complete_turn(
        &state, "turn-2", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 2);
    state.note_human_input();
    assert_eq!(state.pacing_streak(), 0);
    require_pacing_wait(&state, secs(9000), 1, secs(9030));
}

#[test]
fn pacing_goal_change_resets_streak() {
    let state = pacing_state();
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 1);
    let evaluation = state.evaluate_continuation(
        "goal-2",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(1)),
        secs(100),
    );
    assert_eq!(
        evaluation,
        BackgroundWaitEvaluation::Wait {
            next_check_in: Some(secs(130)),
            emit_warning: false,
        }
    );
    assert_eq!(state.pacing_streak(), 0);
}

#[test]
fn pacing_new_schedule_replaces_old_without_accumulation() {
    let state = pacing_state();
    let start = secs(2000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    assert_eq!(
        state.armed_deadline(),
        Some((start + secs(30), state.generation()))
    );

    // Re-evaluation before the deadline re-arms the same registration; the
    // wait never extends and no second registration accumulates.
    require_pacing_wait(&state, start + secs(5), 1, start + secs(30));
    assert_eq!(
        state.armed_deadline(),
        Some((start + secs(30), state.generation()))
    );

    // Invalidation cancels the old schedule; the next evaluation starts a
    // fresh episode instead of resuming the old deadline.
    state.note_turn_start();
    assert_eq!(state.armed_deadline(), None);
    require_pacing_wait(&state, start + secs(5), 1, start + secs(35));
    assert_eq!(
        state.armed_deadline(),
        Some((start + secs(35), state.generation()))
    );
}

#[test]
fn pacing_event_wake_preempts_timer() {
    let state = pacing_state();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    state.set_invalidation_hook({
        let hook_calls = Arc::clone(&hook_calls);
        move || {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        }
    });
    let start = secs(3000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    let (deadline, generation) = state.armed_deadline().expect("armed pacing deadline");

    // An event wake (mail, child completion, exec notification) starts a turn,
    // whose turn-start hook pre-empts the pacing timer exactly like a
    // background-wait wake does.
    state.note_turn_start();
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.armed_deadline(), None);
    assert!(
        !state.claim_due_deadline(deadline, generation, deadline),
        "pre-empted pacing timer must not fire"
    );
}

#[test]
fn pacing_invalidation_cancels_armed_timer() {
    for invalidate in [
        BackgroundWaitState::note_turn_start,
        BackgroundWaitState::note_steering,
        BackgroundWaitState::note_goal_mutation,
        BackgroundWaitState::note_release,
        BackgroundWaitState::note_human_input,
        BackgroundWaitState::note_clear,
        BackgroundWaitState::note_stop,
        BackgroundWaitState::note_resume,
    ] {
        let state = pacing_state();
        let hook_calls = Arc::new(AtomicUsize::new(0));
        state.set_invalidation_hook({
            let hook_calls = Arc::clone(&hook_calls);
            move || {
                hook_calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        let start = secs(4000);
        require_pacing_wait(&state, start, 1, start + secs(30));
        let (deadline, generation) = state.armed_deadline().expect("armed pacing deadline");
        invalidate(&state);
        assert_eq!(state.armed_deadline(), None);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert!(
            !state.claim_due_deadline(deadline, generation, deadline),
            "cancelled pacing timer must not fire"
        );
    }
}

#[test]
fn pacing_ticket_binds_goal_revision_predecessor_generation() {
    let state = pacing_state();
    complete_turn(
        &state, "turn-9", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    let generation = state.generation();
    let start = secs(6000);
    require_pacing_wait(&state, start, 7, start + secs(60));
    assert!(
        state.claim_due_deadline(start + secs(60), generation, start + secs(60)),
        "pacing timer should fire at its deadline"
    );
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(7)),
        start + secs(60),
    );
    let ticket = match evaluation {
        BackgroundWaitEvaluation::ProceedWithPacingTicket {
            expected_revision: 7,
            ticket,
        } => ticket,
        other => panic!("expected pacing ticket, got {other:?}"),
    };
    assert_eq!(ticket.goal_id(), "goal-1");
    assert_eq!(ticket.expected_revision(), 7);
    assert_eq!(ticket.predecessor_turn(), Some("turn-9"));
    assert_eq!(ticket.generation(), generation);
    assert_eq!(ticket.streak(), 2);
    assert_eq!(ticket.issued_at(), start + secs(60));
    assert_eq!(state.pacing_outstanding_ticket().as_ref(), Some(&ticket));
}

#[test]
fn pacing_fast_intervening_human_turn_invalidates() {
    let state = pacing_state();
    let start = secs(8000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    let (deadline, generation) = state.armed_deadline().expect("armed pacing deadline");

    // A human turn that starts AND ends before the old deadline still
    // invalidates the wait: the turn-start note bumps the generation.
    state.note_human_input();
    state.note_turn_start();
    state.note_turn_completed(
        "human-1", /*is_goal_continuation*/ false, /*had_state_change*/ false,
    );
    assert_eq!(state.armed_deadline(), None);
    assert!(
        !state.claim_due_deadline(deadline, generation, deadline),
        "wait superseded by a fast human turn must not fire"
    );

    // Same for an issued ticket: it never admits after the intervening turn.
    require_pacing_wait(&state, start + secs(1), 1, start + secs(31));
    let (deadline, generation) = state.armed_deadline().expect("armed pacing deadline");
    assert!(state.claim_due_deadline(deadline, generation, deadline));
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(1)),
        deadline,
    );
    assert!(matches!(
        evaluation,
        BackgroundWaitEvaluation::ProceedWithPacingTicket { .. }
    ));
    state.note_human_input();
    state.note_turn_start();
    state.note_turn_completed(
        "human-2", /*is_goal_continuation*/ false, /*had_state_change*/ false,
    );
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(1))),
        GoalAdmissionDecision::Wait
    );
}

#[test]
fn pacing_stale_ticket_rejected_on_revision_change() {
    let state = pacing_state();
    let start = secs(10000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    require_claimed_pacing_ticket(&state, start + secs(30), start + secs(30), 1);

    // Fresh pending-mail state (a revision bump with no generation change)
    // must reject the ticket: trigger mail wins over pacing.
    let state = pacing_state();
    require_pacing_wait(&state, start, 1, start + secs(30));
    assert!(state.claim_due_deadline(start + secs(30), state.generation(), start + secs(30)));
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(1)),
        start + secs(30),
    );
    assert!(matches!(
        evaluation,
        BackgroundWaitEvaluation::ProceedWithPacingTicket { .. }
    ));
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(2))),
        GoalAdmissionDecision::Wait
    );
    assert_eq!(state.pacing_outstanding_ticket(), None);
}

#[test]
fn pacing_stale_ticket_rejected_on_predecessor_change() {
    let state = pacing_state();
    let start = secs(11000);
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    require_pacing_wait(&state, start, 1, start + secs(60));
    assert!(state.claim_due_deadline(start + secs(60), state.generation(), start + secs(60)));
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(1)),
        start + secs(60),
    );
    let ticket = match evaluation {
        BackgroundWaitEvaluation::ProceedWithPacingTicket { ticket, .. } => ticket,
        other => panic!("expected pacing ticket, got {other:?}"),
    };
    assert_eq!(ticket.predecessor_turn(), Some("turn-1"));

    // A predecessor change without a generation bump (defense in depth:
    // production turn starts always bump first) still rejects.
    state.note_turn_completed(
        "turn-2", /*is_goal_continuation*/ false, /*had_state_change*/ false,
    );
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(1))),
        GoalAdmissionDecision::Wait
    );
}

#[test]
fn pacing_ticket_reused_is_rejected() {
    let state = pacing_state();
    let start = secs(12000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    assert!(state.claim_due_deadline(start + secs(30), state.generation(), start + secs(30)));
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(PendingWorkSnapshot::empty(1)),
        start + secs(30),
    );
    assert!(matches!(
        evaluation,
        BackgroundWaitEvaluation::ProceedWithPacingTicket { .. }
    ));
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(1))),
        GoalAdmissionDecision::Allow
    );
    // Single-use: the consumed attempt never admits twice.
    assert_eq!(
        state.check_admission(Ok(PendingWorkSnapshot::empty(1))),
        GoalAdmissionDecision::Wait
    );
}

#[test]
fn pacing_claim_rejects_early_superseded_and_wrong_generation() {
    let state = pacing_state();
    let start = secs(13000);
    require_pacing_wait(&state, start, 1, start + secs(30));
    let (deadline, generation) = state.armed_deadline().expect("armed pacing deadline");
    assert!(
        !state.claim_due_deadline(deadline, generation, start + secs(29)),
        "pacing timer must not fire before its deadline"
    );
    assert_eq!(state.armed_deadline(), Some((deadline, generation)));

    state.note_turn_start();
    require_pacing_wait(&state, start + secs(5), 1, start + secs(35));
    assert!(
        !state.claim_due_deadline(deadline, generation, start + secs(30)),
        "superseded pacing registration must not fire"
    );
    let (new_deadline, new_generation) = state.armed_deadline().expect("armed pacing deadline");
    assert!(
        !state.claim_due_deadline(new_deadline, generation, new_deadline),
        "wrong-generation claim must not fire"
    );
    assert!(state.claim_due_deadline(new_deadline, new_generation, new_deadline));
}

#[test]
fn pacing_inactive_goal_never_paced() {
    let state = pacing_state();
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::InactiveOrBudgetLimited,
        Ok(PendingWorkSnapshot::empty(1)),
        secs(14000),
    );
    assert_eq!(evaluation, BackgroundWaitEvaluation::ProceedWithoutGate);
    assert_eq!(state.armed_deadline(), None);

    // Going inactive after arming cancels the pacing registration.
    let state = pacing_state();
    require_pacing_wait(&state, secs(14000), 1, secs(14030));
    assert!(state.armed_deadline().is_some());
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::InactiveOrBudgetLimited,
        Ok(PendingWorkSnapshot::empty(1)),
        secs(14000),
    );
    assert_eq!(evaluation, BackgroundWaitEvaluation::ProceedWithoutGate);
    assert_eq!(state.armed_deadline(), None);
}

#[test]
fn pacing_known_work_uses_background_wait_instead() {
    let state = combined_state();
    let start = secs(15000);
    let evaluation = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(pending_snapshot(1)),
        start,
    );
    // Known subscribed work waits on the background gate's 30-minute
    // check-in, not on the 30-second pacing floor.
    assert_eq!(
        evaluation,
        BackgroundWaitEvaluation::Wait {
            next_check_in: Some(start + mins(30)),
            emit_warning: false,
        }
    );
    assert_eq!(state.pacing_streak(), 0);
}

#[test]
fn pacing_emits_no_warnings_or_prompt_text() {
    let state = pacing_state();
    let start = secs(16000);
    // Across a full wait/fire/admit cycle pacing never emits the
    // stopped-check-ins warning (that warning belongs to the background
    // gate's exhausted cap, never to pacing).
    for at in [start, start + secs(10), start + secs(29)] {
        let evaluation = state.evaluate_continuation(
            "goal-1",
            GoalWaitStatus::Active,
            Ok(PendingWorkSnapshot::empty(1)),
            at,
        );
        assert_eq!(
            evaluation,
            BackgroundWaitEvaluation::Wait {
                next_check_in: Some(start + secs(30)),
                emit_warning: false,
            }
        );
    }
    assert!(!state.warning_emitted());
    require_claimed_pacing_ticket(&state, start + secs(30), start + secs(30), 1);
    assert!(!state.warning_emitted());
}

#[test]
fn pacing_resume_restarts_at_floor_and_resets_quotas() {
    let state = combined_state();
    let start = secs(17000);
    // Grow both quotas: one background check-in and a pacing streak of two
    // with an armed pacing wait.
    let _ = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(pending_snapshot(1)),
        start,
    );
    let _ = state.evaluate_continuation(
        "goal-1",
        GoalWaitStatus::Active,
        Ok(pending_snapshot(1)),
        start + mins(30),
    );
    assert_eq!(
        state.check_admission(Ok(pending_snapshot(1))),
        GoalAdmissionDecision::Allow
    );
    assert_eq!(state.check_ins_used(), 1);
    complete_turn(
        &state, "turn-1", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    complete_turn(
        &state, "turn-2", /*is_goal_continuation*/ true, /*had_state_change*/ false,
    );
    assert_eq!(state.pacing_streak(), 2);
    require_pacing_wait(&state, start + mins(31), 1, start + mins(31) + secs(120));
    assert!(state.armed_deadline().is_some());

    // Resume reconciles the goal and restarts transient pacing at the floor,
    // never restoring stale streaks, predecessors, or subscriptions; the
    // restart also resets transient background quotas (disclosed bound).
    state.note_resume();
    assert_eq!(state.pacing_streak(), 0);
    assert_eq!(state.pacing_predecessor(), None);
    assert_eq!(state.pacing_outstanding_ticket(), None);
    assert_eq!(state.armed_deadline(), None);
    assert_eq!(state.check_ins_used(), 0);
    require_pacing_wait(&state, start + mins(32), 1, start + mins(32) + secs(30));
}

#[test]
fn pacing_waiting_turn_is_paced_not_empty() {
    let accounting = GoalAccountingState::default();
    accounting.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    accounting.mark_turn_goal_active("turn-1", "goal-1");
    // Poll-only activity: a successful tool plus a nonempty final message,
    // but zero state-changing outcomes.
    accounting.record_tool_outcome(
        "turn-1",
        &ToolName::plain("exec"),
        ToolCallOutcome::Completed { success: true },
    );
    let waiting_final = TurnItem::AgentMessage(AgentMessageItem {
        id: "msg-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: "waiting on the background job".to_string(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    });
    accounting.record_item("turn-1", &waiting_final);
    accounting.mark_goal_continuation("turn-1".to_string());
    assert_eq!(None, accounting.empty_response_goal("turn-1"));
    assert!(!accounting.has_observed_state_change("turn-1"));

    // The nonempty waiting turn grows the pacing streak instead of counting
    // toward the empty blocker.
    let state = pacing_state();
    state.note_turn_completed(
        "turn-1",
        accounting.is_goal_continuation("turn-1"),
        accounting.has_observed_state_change("turn-1"),
    );
    assert_eq!(state.pacing_streak(), 1);
    require_pacing_wait(&state, secs(18000), 1, secs(18060));
}

#[test]
fn pacing_disabled_by_default_with_env_kill_switch() {
    let state = BackgroundWaitState::new();
    assert!(!state.is_pacing_enabled());
    assert!(!pacing_disabled_by_env());

    state.enable_pacing();
    assert!(state.is_pacing_enabled());
    state.disable_pacing();
    assert!(!state.is_pacing_enabled());
}
