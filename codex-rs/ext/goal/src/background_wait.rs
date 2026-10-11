//! Goal-owned background waiting policy, inactive by default.
//!
//! Gates ONLY automatic goal continuation on known pending exec-completion
//! work. User input, follow-up input, and queued or trigger mail never consult
//! this module; trigger-mail priority stays authoritative and no global idle
//! suppression exists here.
//!
//! The goal continuation path calls
//! [`BackgroundWaitState::evaluate_continuation`] with the E1 snapshot. When
//! known pending work exists and no check-in is due, evaluation returns
//! [`BackgroundWaitEvaluation::Wait`] and the caller drops the goal semaphore
//! before returning without starting a turn. Never hold the semaphore during
//! a delay; this module is synchronous and holds its own lock only briefly.
//!
//! Fallback check-ins fire at absolute offsets of 30, 60, and 120 minutes
//! from the check-in epoch, at most three per human input. Each ticket
//! bypasses only this work gate, exactly once. Status, Plan, shutdown,
//! capacity, input, and newer-turn checks still apply in Core. After the third
//! check-in the caller emits [`CHECK_INS_STOPPED_WARNING`] exactly once; the
//! goal stays active and event-wakeable, never paused, blocked, or complete.
//!
//! The check-in epoch is anchored at the last human input, where the allowance
//! renews. Admitted check-in turns and other automatic turns never move it;
//! only human input, goal mutation, clear, stop, and release reset the epoch
//! and the used count together. Turn start and steering invalidate tickets
//! without renewing the epoch.
//!
//! Every `Wait` evaluation with a deadline registers that deadline as the
//! pending scheduled re-entry. The runtime spawns one abortable timer per
//! registration; when it fires, the timer claims the registration through
//! [`BackgroundWaitState::claim_due_deadline`] and re-enters the normal goal
//! continuation path. Every invalidation cancels the registration (and aborts
//! the timer through the hook installed by the runtime); a newer deadline
//! replaces it. A completion (observed as empty pending work on reassessment)
//! pre-empts timers without counting as human input. Releasing a mis-armed
//! subscription lets a later reassessment proceed without killing the process.
//!
//! A failed snapshot read is never treated as empty: evaluation returns
//! [`BackgroundWaitEvaluation::WaitOnReadFailure`] and no automatic
//! continuation proceeds past the gate. Callers use fake `now` values in
//! tests, never real sleeps.
//!
//! Low-activity pacing (see `continuation_schedule`) shares this state's lock,
//! invalidation generation, pending-deadline slot, and admission checker.
//! When the snapshot is empty and pacing is enabled, evaluation paces the
//! continuation on the P4b-1 observation streak instead of proceeding
//! immediately; known pending work still uses this gate's waits instead.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use codex_extension_api::GoalAdmissionDecision;
use codex_extension_api::GoalBackgroundWaitAdmission;
use codex_extension_api::PendingWorkReadError;
use codex_extension_api::PendingWorkSnapshot;

use crate::continuation_schedule::PacingTicket;
use crate::continuation_schedule::pacing_delay;

/// Absolute check-in deadlines from the current wait start.
pub const CHECK_IN_DELAYS: [Duration; 3] = [
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(120 * 60),
];

/// Maximum check-ins per human input epoch.
pub const MAX_CHECK_INS_PER_HUMAN_INPUT: u8 = 3;

/// Warning emitted exactly once after the third check-in is exhausted.
pub const CHECK_INS_STOPPED_WARNING: &str =
    "Automatic check-ins stopped; waiting for subscribed work or your next message.";

/// Whether the goal is eligible for background waiting.
///
/// Only an active goal engages the gate. Inactive and budget-limited goals
/// proceed without recording, as do unopted or server-only processes, which
/// the E1 snapshot already reports as empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalWaitStatus {
    /// The goal can wait on subscribed work.
    Active,
    /// Inactive or budget-limited: the gate does not engage.
    InactiveOrBudgetLimited,
}

/// Evaluation of one goal continuation attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundWaitEvaluation {
    /// Gate not engaged: disabled or ineligible status. No recording.
    ProceedWithoutGate,
    /// No pending work: proceed, recording the revision for the admission
    /// recheck that catches receipt transitions before turn start.
    ProceedNormal { expected_revision: u64 },
    /// Pending work exists but a check-in is due: proceed once with a
    /// single-use ticket that bypasses only this work gate.
    ProceedWithTicket {
        expected_revision: u64,
        ticket: CheckInTicket,
    },
    /// No pending work and the pacing delay elapsed: proceed once with a
    /// single-use pacing ticket. The ticket binds goal identity, revision,
    /// predecessor turn, and generation/streak; admission rechecks all of
    /// them together with pending-mail state.
    ProceedWithPacingTicket {
        expected_revision: u64,
        ticket: PacingTicket,
    },
    /// Known pending work and no check-in due: wait for events. The caller
    /// drops the goal semaphore and returns without starting. `next_check_in`
    /// is the absolute deadline when allowance remains, or `None` once the
    /// cap is exhausted. `emit_warning` is true exactly once, on the first
    /// blocked attempt after the third check-in.
    Wait {
        next_check_in: Option<Duration>,
        emit_warning: bool,
    },
    /// Snapshot read failed: safe behavior is to wait, never to treat the
    /// failure as empty. No automatic continuation proceeds past the gate.
    WaitOnReadFailure { error: PendingWorkReadError },
}

/// Single-use check-in ticket bypassing only the work gate, exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckInTicket {
    ticket_id: u64,
    generation: u64,
    issued_at: Duration,
}

impl CheckInTicket {
    /// Unique ticket identity within this state.
    pub fn ticket_id(&self) -> u64 {
        self.ticket_id
    }

    /// State generation at issuance; invalidated generations reject.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Fake-time instant of issuance.
    pub fn issued_at(&self) -> Duration {
        self.issued_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AdmissionAttempt {
    goal_id: String,
    expected_revision: u64,
    expected_generation: u64,
    ticket: Option<CheckInTicket>,
    pacing_ticket: Option<PacingTicket>,
}

/// Registered scheduled re-entry for one fallback check-in deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArmedDeadline {
    deadline: Duration,
    generation: u64,
}

/// Synchronous hook fired on every invalidation so the runtime can abort its
/// pending check-in timer. Opaque in `Debug`, like the admission closure.
#[derive(Clone)]
struct InvalidationHook(Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for InvalidationHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InvalidationHook(<opaque>)")
    }
}

#[derive(Debug)]
struct WaitState {
    enabled: bool,
    active_goal_id: Option<String>,
    generation: u64,
    check_ins_used: u8,
    wait_started_at: Option<Duration>,
    warning_emitted: bool,
    outstanding_ticket: Option<CheckInTicket>,
    last_consumed_ticket_id: Option<u64>,
    next_ticket_id: u64,
    admission_attempt: Option<AdmissionAttempt>,
    pending_deadline: Option<ArmedDeadline>,
    invalidation_hook: Option<InvalidationHook>,
    pacing_enabled: bool,
    pacing_streak: u32,
    pacing_goal_id: Option<String>,
    pacing_predecessor: Option<String>,
    pacing_wait_started_at: Option<Duration>,
    pacing_outstanding: Option<PacingTicket>,
    pacing_last_consumed_ticket_id: Option<u64>,
}

impl WaitState {
    fn new() -> Self {
        Self {
            enabled: false,
            active_goal_id: None,
            generation: 0,
            check_ins_used: 0,
            wait_started_at: None,
            warning_emitted: false,
            outstanding_ticket: None,
            last_consumed_ticket_id: None,
            next_ticket_id: 1,
            admission_attempt: None,
            pending_deadline: None,
            invalidation_hook: None,
            pacing_enabled: false,
            pacing_streak: 0,
            pacing_goal_id: None,
            pacing_predecessor: None,
            pacing_wait_started_at: None,
            pacing_outstanding: None,
            pacing_last_consumed_ticket_id: None,
        }
    }

    fn invalidate_tickets(&mut self) {
        self.generation += 1;
        self.outstanding_ticket = None;
        self.pacing_outstanding = None;
        self.admission_attempt = None;
        self.pending_deadline = None;
        self.pacing_wait_started_at = None;
        self.fire_invalidation_hook();
    }

    fn fire_invalidation_hook(&self) {
        if let Some(hook) = &self.invalidation_hook {
            hook.0();
        }
    }
}

/// Goal-owned background waiting policy.
///
/// Synchronous and lock-brief: no method awaits, so evaluation can never hold
/// the goal semaphore across a delay. Disabled by default until activation.
#[derive(Debug)]
pub struct BackgroundWaitState {
    inner: Mutex<WaitState>,
}

impl BackgroundWaitState {
    /// Creates disabled policy state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(WaitState::new()),
        }
    }

    /// Enables the policy for subsequent evaluations.
    pub fn enable(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enabled = true;
    }

    /// Disables the policy and resets transient wait state for a fresh start.
    pub fn disable(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.enabled = false;
        state.active_goal_id = None;
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.outstanding_ticket = None;
        state.last_consumed_ticket_id = None;
        state.admission_attempt = None;
        state.pending_deadline = None;
        state.fire_invalidation_hook();
    }

    /// Reports whether the policy engages.
    pub fn is_enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enabled
    }

    /// Builds the Core admission checker capturing this state.
    pub fn admission_checker(self: &std::sync::Arc<Self>) -> GoalBackgroundWaitAdmission {
        let state = std::sync::Arc::clone(self);
        GoalBackgroundWaitAdmission::new(move |outcome| state.check_admission(outcome))
    }

    /// Evaluates one goal continuation attempt at fake-time `now`.
    ///
    /// Records the admission attempt for [`BackgroundWaitState::check_admission`]
    /// on every `Proceed` variant. On `Wait` variants the caller drops the
    /// goal semaphore and returns without starting.
    ///
    /// A `Wait` with a deadline registers it as the pending scheduled
    /// re-entry, replacing any older registration; the runtime spawns the
    /// matching abortable timer. Every other outcome consumes or clears the
    /// registration, except a read failure, which keeps a previously armed
    /// deadline so its already-spawned timer still re-enters.
    pub fn evaluate_continuation(
        &self,
        goal_id: &str,
        status: GoalWaitStatus,
        snapshot: Result<PendingWorkSnapshot, PendingWorkReadError>,
        now: Duration,
    ) -> BackgroundWaitEvaluation {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.evaluate_locked(&mut state, goal_id, status, snapshot, now)
    }

    fn evaluate_locked(
        &self,
        state: &mut WaitState,
        goal_id: &str,
        status: GoalWaitStatus,
        snapshot: Result<PendingWorkSnapshot, PendingWorkReadError>,
        now: Duration,
    ) -> BackgroundWaitEvaluation {
        if status == GoalWaitStatus::InactiveOrBudgetLimited {
            state.wait_started_at = None;
            state.pacing_wait_started_at = None;
            state.pending_deadline = None;
            return BackgroundWaitEvaluation::ProceedWithoutGate;
        }
        if !state.enabled && !state.pacing_enabled {
            state.pending_deadline = None;
            return BackgroundWaitEvaluation::ProceedWithoutGate;
        }
        if state.active_goal_id.as_deref() != Some(goal_id) {
            state.active_goal_id = Some(goal_id.to_string());
            state.check_ins_used = 0;
            state.wait_started_at = None;
            state.warning_emitted = false;
            state.outstanding_ticket = None;
            state.last_consumed_ticket_id = None;
            state.admission_attempt = None;
        }
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return BackgroundWaitEvaluation::WaitOnReadFailure { error };
            }
        };
        if snapshot.is_empty() {
            state.wait_started_at = None;
            if !state.pacing_enabled {
                state.pending_deadline = None;
                state.admission_attempt = Some(AdmissionAttempt {
                    goal_id: goal_id.to_string(),
                    expected_revision: snapshot.revision(),
                    expected_generation: state.generation,
                    ticket: None,
                    pacing_ticket: None,
                });
                return BackgroundWaitEvaluation::ProceedNormal {
                    expected_revision: snapshot.revision(),
                };
            }
            return self.evaluate_pacing_locked(state, goal_id, &snapshot, now);
        }
        if !state.enabled {
            // Background gate inactive but pacing active: pacing only paces
            // empty snapshots, so pending work proceeds immediately with no
            // gate. Any pacing wait is superseded (its timer claim will fail
            // quietly; the next wait replaces the timer).
            state.pacing_wait_started_at = None;
            state.pending_deadline = None;
            state.admission_attempt = None;
            return BackgroundWaitEvaluation::ProceedWithoutGate;
        }
        if state.check_ins_used >= MAX_CHECK_INS_PER_HUMAN_INPUT {
            let emit_warning = !state.warning_emitted;
            state.warning_emitted = true;
            state.pending_deadline = None;
            return BackgroundWaitEvaluation::Wait {
                next_check_in: None,
                emit_warning,
            };
        }
        let wait_started_at = *state.wait_started_at.get_or_insert(now);
        let delay = CHECK_IN_DELAYS
            .get(usize::from(state.check_ins_used))
            .copied()
            .unwrap_or(Duration::MAX);
        let deadline = wait_started_at.checked_add(delay).unwrap_or(Duration::MAX);
        if now < deadline {
            state.pending_deadline = Some(ArmedDeadline {
                deadline,
                generation: state.generation,
            });
            return BackgroundWaitEvaluation::Wait {
                next_check_in: Some(deadline),
                emit_warning: false,
            };
        }
        state.pending_deadline = None;
        let ticket = CheckInTicket {
            ticket_id: state.next_ticket_id,
            generation: state.generation,
            issued_at: now,
        };
        state.next_ticket_id += 1;
        state.outstanding_ticket = Some(ticket.clone());
        state.admission_attempt = Some(AdmissionAttempt {
            goal_id: goal_id.to_string(),
            expected_revision: snapshot.revision(),
            expected_generation: state.generation,
            ticket: Some(ticket.clone()),
            pacing_ticket: None,
        });
        BackgroundWaitEvaluation::ProceedWithTicket {
            expected_revision: snapshot.revision(),
            ticket,
        }
    }

    /// Evaluates low-activity pacing for an empty snapshot at fake-time `now`.
    ///
    /// The streak counts consecutive completed goal continuations with zero
    /// recorded state-changing outcomes; any observation resets it to the
    /// floor (see [`BackgroundWaitState::note_turn_completed`]). The first
    /// evaluation of a wait episode anchors `pacing_wait_started_at` and
    /// returns `Wait`; once `now` reaches the anchored deadline, evaluation
    /// issues a single-use [`PacingTicket`] and returns
    /// [`BackgroundWaitEvaluation::ProceedWithPacingTicket`]. The deadline
    /// never recomputes mid-episode, so re-evaluation before the deadline
    /// re-arms the same registration instead of extending the wait.
    fn evaluate_pacing_locked(
        &self,
        state: &mut WaitState,
        goal_id: &str,
        snapshot: &PendingWorkSnapshot,
        now: Duration,
    ) -> BackgroundWaitEvaluation {
        if state.pacing_goal_id.as_deref() != Some(goal_id) {
            state.pacing_goal_id = Some(goal_id.to_string());
            state.pacing_streak = 0;
            state.pacing_predecessor = None;
            state.pacing_wait_started_at = None;
            state.pacing_outstanding = None;
            state.pacing_last_consumed_ticket_id = None;
        }
        let streak_n = state.pacing_streak.saturating_add(1);
        let delay = pacing_delay(streak_n);
        let wait_started_at = *state.pacing_wait_started_at.get_or_insert(now);
        let deadline = wait_started_at.checked_add(delay).unwrap_or(Duration::MAX);
        if now < deadline {
            state.pending_deadline = Some(ArmedDeadline {
                deadline,
                generation: state.generation,
            });
            return BackgroundWaitEvaluation::Wait {
                next_check_in: Some(deadline),
                emit_warning: false,
            };
        }
        state.pending_deadline = None;
        let ticket = PacingTicket::new(
            state.next_ticket_id,
            state.generation,
            goal_id.to_string(),
            snapshot.revision(),
            state.pacing_predecessor.clone(),
            streak_n,
            now,
        );
        state.next_ticket_id += 1;
        state.pacing_outstanding = Some(ticket.clone());
        state.admission_attempt = Some(AdmissionAttempt {
            goal_id: goal_id.to_string(),
            expected_revision: snapshot.revision(),
            expected_generation: state.generation,
            ticket: None,
            pacing_ticket: Some(ticket.clone()),
        });
        BackgroundWaitEvaluation::ProceedWithPacingTicket {
            expected_revision: snapshot.revision(),
            ticket,
        }
    }

    /// Claims the registered check-in deadline for one fired timer.
    ///
    /// Production timers call this after sleeping: on `true` the registration
    /// is consumed and the caller re-enters goal continuation; on `false` the
    /// timer is stale (superseded, invalidated, disabled, or early) and the
    /// caller returns without doing anything. Consuming the claim never starts
    /// a turn by itself; the re-entry evaluates and admits normally.
    pub fn claim_due_deadline(&self, deadline: Duration, generation: u64, now: Duration) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.enabled && !state.pacing_enabled {
            return false;
        }
        let Some(armed) = state.pending_deadline else {
            return false;
        };
        if armed.deadline != deadline
            || armed.generation != generation
            || armed.generation != state.generation
        {
            return false;
        }
        if now < armed.deadline {
            return false;
        }
        state.pending_deadline = None;
        true
    }

    /// Installs the hook fired on every invalidation.
    ///
    /// The runtime installs its timer abort here so steering, turn start, goal
    /// mutation, clear, stop, release, human input, and resume all cancel the
    /// pending scheduled re-entry without further call-site wiring.
    pub fn set_invalidation_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .invalidation_hook = Some(InvalidationHook(Arc::new(hook)));
    }

    /// Rechecks pending state and revision for one recorded attempt.
    ///
    /// Consumes the recorded attempt exactly once. Returns `Wait` when no
    /// attempt was recorded, when the generation or revision changed, when a
    /// ticket is missing, stale, or reused, when a pacing ticket's goal,
    /// predecessor, or pending-mail state changed, and on every read failure.
    /// A stale ticket never starts a turn.
    pub fn check_admission(
        &self,
        outcome: Result<PendingWorkSnapshot, PendingWorkReadError>,
    ) -> GoalAdmissionDecision {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(attempt) = state.admission_attempt.take() else {
            return GoalAdmissionDecision::Wait;
        };
        if attempt.expected_generation != state.generation {
            return GoalAdmissionDecision::Wait;
        }
        let snapshot = match outcome {
            Ok(snapshot) => snapshot,
            Err(_) => return GoalAdmissionDecision::Wait,
        };
        if let Some(ticket) = attempt.pacing_ticket.as_ref() {
            return Self::check_pacing_admission_locked(&mut state, &attempt, ticket, &snapshot);
        }
        match attempt.ticket {
            Some(ticket) => {
                let outstanding_matches = state
                    .outstanding_ticket
                    .as_ref()
                    .is_some_and(|outstanding| outstanding.ticket_id == ticket.ticket_id)
                    && ticket.generation == state.generation
                    && state.last_consumed_ticket_id != Some(ticket.ticket_id);
                if !outstanding_matches {
                    return GoalAdmissionDecision::Wait;
                }
                if snapshot.revision() != attempt.expected_revision {
                    state.outstanding_ticket = None;
                    return GoalAdmissionDecision::Wait;
                }
                if state.check_ins_used >= MAX_CHECK_INS_PER_HUMAN_INPUT {
                    return GoalAdmissionDecision::Wait;
                }
                state.last_consumed_ticket_id = Some(ticket.ticket_id);
                state.outstanding_ticket = None;
                state.check_ins_used += 1;
                GoalAdmissionDecision::Allow
            }
            None => {
                if snapshot.revision() != attempt.expected_revision {
                    return GoalAdmissionDecision::Wait;
                }
                if !snapshot.is_empty() {
                    return GoalAdmissionDecision::Wait;
                }
                GoalAdmissionDecision::Allow
            }
        }
    }

    /// Rechecks one pacing ticket against predecessor and pending-mail state.
    ///
    /// Consumes the ticket exactly once. Any mismatch (generation, revision,
    /// goal identity, predecessor turn, outstanding handle, reuse, or fresh
    /// pending mail) returns `Wait` without starting a turn.
    fn check_pacing_admission_locked(
        state: &mut WaitState,
        attempt: &AdmissionAttempt,
        ticket: &PacingTicket,
        snapshot: &PendingWorkSnapshot,
    ) -> GoalAdmissionDecision {
        let outstanding_matches = state
            .pacing_outstanding
            .as_ref()
            .is_some_and(|outstanding| outstanding.ticket_id() == ticket.ticket_id())
            && ticket.generation() == state.generation
            && state.pacing_last_consumed_ticket_id != Some(ticket.ticket_id());
        if !outstanding_matches {
            return GoalAdmissionDecision::Wait;
        }
        if ticket.goal_id() != attempt.goal_id
            || state.pacing_goal_id.as_deref() != Some(ticket.goal_id())
        {
            state.pacing_outstanding = None;
            return GoalAdmissionDecision::Wait;
        }
        if snapshot.revision() != attempt.expected_revision
            || snapshot.revision() != ticket.expected_revision()
        {
            state.pacing_outstanding = None;
            return GoalAdmissionDecision::Wait;
        }
        if state.pacing_predecessor.as_deref() != ticket.predecessor_turn() {
            state.pacing_outstanding = None;
            return GoalAdmissionDecision::Wait;
        }
        if !snapshot.is_empty() {
            state.pacing_outstanding = None;
            return GoalAdmissionDecision::Wait;
        }
        state.pacing_last_consumed_ticket_id = Some(ticket.ticket_id());
        state.pacing_outstanding = None;
        GoalAdmissionDecision::Allow
    }

    /// Renews the check-in allowance on human input.
    ///
    /// Also resets the pacing streak to the floor: the next automatic
    /// continuation after human input waits only the floor delay.
    pub fn note_human_input(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.pacing_streak = 0;
    }

    /// Invalidates tickets on turn start without renewing the allowance.
    ///
    /// The check-in epoch is anchored at the last human input: admitted
    /// check-in turns and other automatic turns never move it.
    pub fn note_turn_start(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .invalidate_tickets();
    }

    /// Invalidates tickets on steering.
    pub fn note_steering(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .invalidate_tickets();
    }

    /// Resets the check-in epoch and allowance on goal mutation.
    ///
    /// Also resets pacing to the floor for the changed goal.
    pub fn note_goal_mutation(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.pacing_streak = 0;
        state.pacing_predecessor = None;
    }

    /// Clears wait state on goal clear for a fresh next goal.
    pub fn note_clear(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.active_goal_id = None;
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.last_consumed_ticket_id = None;
        state.pacing_streak = 0;
        state.pacing_goal_id = None;
        state.pacing_predecessor = None;
        state.pacing_last_consumed_ticket_id = None;
    }

    /// Clears wait state on thread stop.
    pub fn note_stop(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.active_goal_id = None;
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.last_consumed_ticket_id = None;
        state.pacing_streak = 0;
        state.pacing_goal_id = None;
        state.pacing_predecessor = None;
        state.pacing_last_consumed_ticket_id = None;
    }

    /// Resets the check-in epoch and allowance on subscription release.
    pub fn note_release(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
    }

    /// Discards stale waits on resume; live-generation-only state restarts.
    ///
    /// Pacing restarts at the floor: no streak, subscription, or deadline is
    /// restored.
    pub fn note_resume(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.invalidate_tickets();
        state.active_goal_id = None;
        state.check_ins_used = 0;
        state.wait_started_at = None;
        state.warning_emitted = false;
        state.last_consumed_ticket_id = None;
        state.pacing_streak = 0;
        state.pacing_goal_id = None;
        state.pacing_predecessor = None;
        state.pacing_last_consumed_ticket_id = None;
    }

    /// Records one completed turn for pacing.
    ///
    /// Always binds `turn_id` as the predecessor for the next pacing ticket.
    /// When `is_goal_continuation` is true, a turn with zero recorded
    /// state-changing outcomes grows the low-activity streak by one; any
    /// observed state change resets it to the floor. Human and other
    /// non-continuation turns never touch the streak (human input resets it
    /// separately via [`BackgroundWaitState::note_human_input`]).
    ///
    /// Call after the three-empty-continuations blocker check and before the
    /// turn's accounting entry is finished, while the observation count is
    /// still readable.
    pub fn note_turn_completed(
        &self,
        turn_id: &str,
        is_goal_continuation: bool,
        had_state_change: bool,
    ) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.pacing_predecessor = Some(turn_id.to_string());
        if !is_goal_continuation {
            return;
        }
        if had_state_change {
            state.pacing_streak = 0;
        } else {
            state.pacing_streak = 0;
        }
    }

    /// Enables low-activity pacing for subsequent evaluations.
    pub fn enable_pacing(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pacing_enabled = true;
    }

    /// Disables pacing and clears its transient wait state.
    ///
    /// Cancels the shared pending slot (a background-wait registration
    /// re-arms on its next evaluation; its timer claim fails quietly until
    /// then) and aborts the timer through the invalidation hook.
    pub fn disable_pacing(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.pacing_enabled = false;
        state.pacing_wait_started_at = None;
        state.pacing_outstanding = None;
        state.admission_attempt = None;
        state.pending_deadline = None;
        state.fire_invalidation_hook();
    }

    /// Reports whether low-activity pacing engages.
    pub fn is_pacing_enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pacing_enabled
    }

    /// Consecutive completed goal continuations with no observed state change.
    pub fn pacing_streak(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pacing_streak
    }

    /// Predecessor turn bound by the next pacing ticket, if any completed yet.
    pub fn pacing_predecessor(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pacing_predecessor
            .clone()
    }

    /// Currently outstanding pacing ticket, if one was issued and not consumed.
    pub fn pacing_outstanding_ticket(&self) -> Option<PacingTicket> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pacing_outstanding
            .clone()
    }

    /// Check-ins consumed in the current human-input epoch.
    pub fn check_ins_used(&self) -> u8 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check_ins_used
    }

    /// Reports whether the stopped-check-ins warning was emitted.
    pub fn warning_emitted(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .warning_emitted
    }

    /// Start of the current wait episode, if waiting.
    pub fn wait_started_at(&self) -> Option<Duration> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .wait_started_at
    }

    /// Registered scheduled re-entry as `(deadline, generation)`, if armed.
    pub fn armed_deadline(&self) -> Option<(Duration, u64)> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_deadline
            .map(|armed| (armed.deadline, armed.generation))
    }

    /// Currently outstanding ticket, if one was issued and not yet consumed.
    pub fn outstanding_ticket(&self) -> Option<CheckInTicket> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .outstanding_ticket
            .clone()
    }

    /// Current invalidation generation.
    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .generation
    }
}

impl Default for BackgroundWaitState {
    fn default() -> Self {
        Self::new()
    }
}
