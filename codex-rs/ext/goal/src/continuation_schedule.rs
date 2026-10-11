//! Low-activity pacing for automatic goal continuations.
//!
//! While [`BackgroundWaitState`](crate::BackgroundWaitState) gates continuation
//! on known pending exec-completion work, this module paces the absence of
//! observed state change: consecutive automatic continuations that record zero
//! state-changing outcomes (per the P4b-1 accounting observations) wait
//! progressively longer before the next turn starts. Any observed state change
//! resets the streak to the floor.
//!
//! Delay schedule (AC1): automatic goal continuations are at least
//! [`PACING_BASE_DELAY`] apart. With a low-activity streak `n >= 1`
//! (consecutive continuations with no observed state change), the delay is
//! `min(30s * 2^(n-1), 30min)` via [`pacing_delay`].
//!
//! Scheduling reuses the single cancellable timer slot owned by
//! [`CheckInTimer`](crate::CheckInTimer) and the ticket/claim machinery of
//! `BackgroundWaitState`: pacing arms the shared `pending_deadline`, fires
//! through the same invalidation hook, and admits through the same
//! `GoalBackgroundWaitAdmission` checker. There is exactly one live timer per
//! goal thread; a new schedule cancels and replaces the old one.
//!
//! [`PacingTicket`] binds goal identity and pending-work revision, the
//! predecessor turn, and the generation/streak cause. Admission rechecks the
//! ticket together with predecessor and pending-mail state; a stale ticket
//! never starts a turn.
//!
//! All pacing state is transient: resume restarts at the floor, and nothing
//! here is model-visible (no prompt text, history, or context changes).

use std::time::Duration;

/// Minimum spacing between automatic goal continuations (the floor).
pub const PACING_BASE_DELAY: Duration = Duration::from_secs(30);

/// Maximum pacing delay (the cap).
pub const PACING_CAP_DELAY: Duration = Duration::from_secs(30 * 60);

/// Test-only kill switch: when `CODEX_GOAL_PACING_DISABLED=1`, the goal
/// extension leaves pacing disabled so pre-pacing suites keep their timing.
///
/// Production never sets this; pacing is always active there. App-server
/// suites that predate pacing set it per-server-process via
/// `with_env_overrides` to stay fast and focused on their own behavior.
pub const PACING_DISABLED_ENV_VAR: &str = "CODEX_GOAL_PACING_DISABLED";

/// Reports whether pacing is disabled via [`PACING_DISABLED_ENV_VAR`].
pub fn pacing_disabled_by_env() -> bool {
    std::env::var(PACING_DISABLED_ENV_VAR).is_ok_and(|value| value == "1")
}

/// Pacing delay for low-activity streak `n`.
///
/// `n` counts consecutive continuations with no observed state change,
/// 1-indexed: `n=1` yields the floor ([`PACING_BASE_DELAY`]), and each further
/// quiet continuation doubles, capped at [`PACING_CAP_DELAY`]. `n=0` (no
/// quiet streak, e.g. right after an observed state change) also yields the
/// floor, since every automatic continuation is spaced at least the floor.
pub fn pacing_delay(low_streak_n: u32) -> Duration {
    let shift = low_streak_n.saturating_sub(1).min(31);
    let multiplier = 1u64 << shift;
    PACING_BASE_DELAY
        .checked_mul(multiplier)
        .unwrap_or(PACING_CAP_DELAY)
        .min(PACING_CAP_DELAY)
}

/// Single-use pacing ticket for one delayed automatic continuation.
///
/// Binds the goal identity and pending-work revision observed at scheduling
/// time, the predecessor turn that completed before the wait, and the
/// invalidation generation plus the streak cause (`streak` is the 1-indexed
/// `n` from [`pacing_delay`]). Invalidated by human input or steering, goal
/// change or clear, stop, completion or release, and any new turn (each bumps
/// the shared generation, including a fast human turn that starts and ends
/// before the deadline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacingTicket {
    ticket_id: u64,
    generation: u64,
    goal_id: String,
    expected_revision: u64,
    predecessor_turn: Option<String>,
    streak: u32,
    issued_at: Duration,
}

impl PacingTicket {
    /// Creates a ticket binding one scheduled pacing wait.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ticket_id: u64,
        generation: u64,
        goal_id: String,
        expected_revision: u64,
        predecessor_turn: Option<String>,
        streak: u32,
        issued_at: Duration,
    ) -> Self {
        Self {
            ticket_id,
            generation,
            goal_id,
            expected_revision,
            predecessor_turn,
            streak,
            issued_at,
        }
    }

    /// Unique ticket identity within the owning wait state.
    pub fn ticket_id(&self) -> u64 {
        self.ticket_id
    }

    /// State generation at issuance; invalidated generations reject.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Goal identity bound at scheduling time.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// Pending-work revision bound at scheduling time.
    pub fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    /// Predecessor turn bound at scheduling time, if any turn completed yet.
    pub fn predecessor_turn(&self) -> Option<&str> {
        self.predecessor_turn.as_deref()
    }

    /// Low-activity streak `n` (1-indexed) whose delay this ticket waited.
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// Fake-time instant of issuance.
    pub fn issued_at(&self) -> Duration {
        self.issued_at
    }
}
