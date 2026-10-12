//! Waiting guidance as a change-diffed World State section.
//!
//! Goal renders the §9 waiting-capability sentences for the current sampling
//! step through this section. The harness persists each step's snapshot and
//! renders a fragment only when it differs, so identical capabilities add no
//! new context across sampling steps or turn boundaries, while a capability
//! change (or the goal stopping) surfaces on the very next request. Threads
//! that never activate a goal contribute no section at all.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WaitingCapabilities;
use codex_extension_api::WorldStateSectionContribution;
use serde_json::Value;

use crate::steering::WAITING_EXEC_NOTIFY;
use crate::steering::WAITING_NO_EXEC_NOTIFY;
use crate::steering::render_native_wait_tool_name;

/// Stable section id, persisted in rollouts.
pub(crate) const WORLD_STATE_ID: &str = "goal_waiting_guidance";

const START_MARKER: &str = "<goal_waiting_guidance>";
const END_MARKER: &str = "</goal_waiting_guidance>";

/// One-shot correction rendered on the active→inactive transition.
///
/// Static and bounded; pinned byte-exact in `ext/goal/tests`.
pub(crate) const WAITING_GUIDANCE_RETRACTION: &str =
    "Waiting guidance given earlier in this thread no longer applies; the goal is not active.";

/// Builds this step's waiting-guidance sections: zero or one.
///
/// While the goal is active (`goal_active`, the P2 `GoalActivity` scope:
/// Active or BudgetLimited), contributes the capability snapshot for this
/// step's [`WaitingCapabilities`]. When the goal is not active, contributes a
/// `false` snapshot exactly when a previous entry exists, so the harness
/// renders one retraction; otherwise contributes nothing, keeping a zero
/// footprint for threads that never had an active goal.
pub(crate) fn goal_waiting_guidance_sections(
    goal_active: bool,
    capabilities: &WaitingCapabilities,
    previous_world_state: Option<&serde_json::Map<String, Value>>,
) -> Vec<WorldStateSectionContribution> {
    let previously_recorded =
        previous_world_state.is_some_and(|state| state.contains_key(WORLD_STATE_ID));
    if !goal_active && !previously_recorded {
        return Vec::new();
    }
    vec![if goal_active {
        active_section(capabilities)
    } else {
        inactive_section()
    }]
}

/// Comparison snapshot for one sampling step: the five capability flags plus
/// the capped native wait display name.
///
/// The name is the same string the fragment shows, so names differing only
/// beyond the cap compare equal and cannot churn the section. An absent name
/// is omitted rather than null: core strips null object fields before it
/// persists a snapshot, so a null field would never equal the restored
/// previous snapshot and would re-render the fragment on every step.
fn waiting_guidance_snapshot(capabilities: &WaitingCapabilities) -> Value {
    let mut snapshot = serde_json::json!({
        "exec_notify": capabilities.exec_notify,
        "native_mail_wait": capabilities.native_mail_wait,
        "sleep": capabilities.sleep,
        "stdin": capabilities.stdin,
    });
    if let Some(native_wait) = capabilities.native_wait.as_ref() {
        snapshot["native_wait"] = Value::String(render_native_wait_tool_name(native_wait));
    }
    snapshot
}

fn active_section(capabilities: &WaitingCapabilities) -> WorldStateSectionContribution {
    let snapshot = waiting_guidance_snapshot(capabilities);
    // The world-state envelope replaces the turn-context slot and content
    // kind; only the §9 sentence text is reused.
    let body = crate::steering::waiting_capability_fragments(capabilities)
        .first()
        .map(|fragment| fragment.text().to_string())
        .unwrap_or_default();
    WorldStateSectionContribution::new(WORLD_STATE_ID, snapshot.clone(), move |previous| {
        let changed = match previous {
            PreviousWorldStateSection::Known(previous) => previous != &snapshot,
            // Activation, or a re-render after compaction evicted the
            // fragment while the snapshot survived (core maps that to Absent
            // via the retained matcher below).
            PreviousWorldStateSection::Absent => true,
            // Unreachable: no legacy matcher is registered. Fail visible,
            // because an untyped history hit cannot prove the five flags and
            // the tool name still match.
            PreviousWorldStateSection::Unknown => true,
        };
        changed.then(|| {
            RenderedWorldStateFragment::new("developer", (START_MARKER, END_MARKER), body.clone())
        })
    })
    .with_retained_fragment_matcher(is_active_waiting_guidance_fragment)
}

fn inactive_section() -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(WORLD_STATE_ID, Value::Bool(false), |previous| {
        let retract = match previous {
            // Already retracted: stay silent.
            PreviousWorldStateSection::Known(Value::Bool(false)) => false,
            // A recorded active snapshot means stale guidance may still be
            // model-visible: correct it exactly once.
            PreviousWorldStateSection::Known(_) => false,
            // No recorded entry (defensive: the contributor skips the section
            // in this case): nothing to correct.
            PreviousWorldStateSection::Absent => false,
            // Unreachable without a legacy matcher; mirror the template by
            // correcting possibly-stale history.
            PreviousWorldStateSection::Unknown => true,
        };
        retract.then(|| {
            RenderedWorldStateFragment::new(
                "developer",
                (START_MARKER, END_MARKER),
                WAITING_GUIDANCE_RETRACTION,
            )
        })
    })
}

/// Recognizes this section's active fragment in retained model history.
///
/// Requires the section markers plus one of the two mutually exclusive exec
/// sentences, so the marker-sharing retraction never counts as retained
/// guidance (which would suppress the re-render after compaction).
fn is_active_waiting_guidance_fragment(role: &str, text: &str) -> bool {
    role == "developer"
        && text.trim_start().starts_with(START_MARKER)
        && text.trim_end().ends_with(END_MARKER)
        && (text.contains(WAITING_EXEC_NOTIFY) || text.contains(WAITING_NO_EXEC_NOTIFY))
}
