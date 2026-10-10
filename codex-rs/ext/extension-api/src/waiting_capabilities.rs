//! Sampling-step snapshot of the waiting tools the model can use right now.
//!
//! Core derives one [`WaitingCapabilities`] per sampling step from the
//! finalized tool router plus thread-scoped host/policy markers, and shares
//! that same instance with every turn-context contributor at both context
//! paths (initial context and steady-state diffs). Contributors render
//! waiting guidance from this snapshot instead of re-reading configuration
//! before sampling.
//!
//! Every tool flag means "directly model-visible in the finalized plan":
//! exclusions and name collisions are already resolved, and deferred or
//! nested-only tools do not count. Acknowledged completion subscriptions are
//! tool-result data, never capability flags: arming a receipt must not flip
//! any flag here.
//!
//! The snapshot is deliberately not serializable and is never persisted to
//! history or rollout; it lives only for its sampling step.

use crate::ToolName;

/// Waiting tools callable as direct model tools in one sampling step.
///
/// Computed once per step from the final captured router; both context paths
/// receive the same instance so values cannot diverge within the step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitingCapabilities {
    /// `clock.sleep` is directly model-visible in the finalized plan.
    pub sleep: bool,
    /// `write_stdin` is directly model-visible in the finalized plan.
    pub stdin: bool,
    /// A native agent wait tool (`wait_agent` in any namespace, V1 or V2) is
    /// directly model-visible, carrying its actual [`ToolName`].
    pub native_wait: Option<ToolName>,
    /// The host can promise an exec completion wake for this step: host
    /// support is available, `exec_command` advertises the `notify_on_exit`
    /// opt-in, and both the `write_stdin` and `exec_notification` controls
    /// are directly model-visible.
    pub exec_notify: bool,
    /// The host can wait for child mailbox results for this step: host
    /// support is available and a goal wait policy is installed.
    pub native_mail_wait: bool,
}

impl WaitingCapabilities {
    /// Snapshot with every capability disabled.
    pub fn none() -> Self {
        Self {
            sleep: false,
            stdin: false,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        }
    }
}
