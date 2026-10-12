//! Unit tests for the goal waiting-guidance World State section.
//!
//! Pins the zero-footprint rule, the render-diff arms (including the
//! unreachable `Unknown` arms), the byte-exact retraction, the retained
//! matcher that distinguishes active guidance from the retraction, and the
//! bounded comparison snapshot.
#![allow(dead_code)]

#[path = "../src/steering.rs"]
mod steering;
#[path = "../src/waiting_world_state.rs"]
mod waiting_world_state;

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::ToolName;
use codex_extension_api::WaitingCapabilities;
use codex_extension_api::WorldStateSectionContribution;
use pretty_assertions::assert_eq;
use pretty_assertions::assert_ne;
use serde_json::Map;
use serde_json::Value;

fn active_capabilities() -> WaitingCapabilities {
    WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        exec_notify: false,
        native_mail_wait: false,
    }
}

fn sections(
    goal_active: bool,
    capabilities: &WaitingCapabilities,
    previous: Option<&Map<String, Value>>,
) -> Vec<WorldStateSectionContribution> {
    waiting_world_state::goal_waiting_guidance_sections(goal_active, capabilities, previous)
}

fn single_section(
    goal_active: bool,
    capabilities: &WaitingCapabilities,
    previous: Option<&Map<String, Value>>,
) -> WorldStateSectionContribution {
    let sections = sections(goal_active, capabilities, previous);
    assert_eq!(sections.len(), 1);
    let Some(section) = sections.into_iter().next() else {
        panic!("expected exactly one waiting-guidance section");
    };
    section
}

fn rendered_text(
    section: &WorldStateSectionContribution,
    previous: PreviousWorldStateSection,
) -> String {
    let Some(fragment) = section.render_diff(previous) else {
        panic!("expected a rendered waiting-guidance fragment");
    };
    assert_eq!(fragment.role(), "developer");
    let (start, end) = fragment.markers();
    let body = fragment.body();
    format!("{start}{body}{end}")
}

#[test]
fn waiting_guidance_world_state_retraction_is_pinned_byte_exact() {
    assert_eq!(
        waiting_world_state::WAITING_GUIDANCE_RETRACTION,
        "Waiting guidance given earlier in this thread no longer applies; the goal is not active."
    );
}

#[test]
fn waiting_guidance_world_state_zero_footprint_without_previous_entry() {
    // Inactive goal, no recorded entry: no section at all, so threads that
    // never had an active goal keep a zero footprint (no snapshot entry, no
    // model-visible change).
    let capabilities = WaitingCapabilities::none();
    assert!(sections(false, &capabilities, None).is_empty());
    assert!(sections(false, &capabilities, Some(&Map::new())).is_empty());
    let mut unrelated = Map::new();
    unrelated.insert("other_section".to_string(), Value::Bool(true));
    assert!(sections(false, &capabilities, Some(&unrelated)).is_empty());
    // An active goal always contributes, even without a previous entry.
    assert_eq!(sections(true, &capabilities, None).len(), 1);
}

#[test]
fn waiting_guidance_world_state_active_renders_full_fragment_on_absent() {
    let capabilities = active_capabilities();
    let section = single_section(true, &capabilities, None);
    assert_eq!(section.id(), waiting_world_state::WORLD_STATE_ID);
    let snapshot = section.snapshot().clone();
    assert!(snapshot.is_object());
    let text = rendered_text(&section, PreviousWorldStateSection::Absent);
    let expected_body = steering::waiting_capability_fragments(&capabilities)
        .first()
        .map(|fragment| fragment.text().to_string())
        .unwrap_or_default();
    assert!(text.contains(expected_body.as_str()));
    assert!(text.contains("multi_agent_v1.wait_agent"));
    assert!(section.has_retained_fragment_matcher());
    assert!(section.matches_retained_fragment("developer", text.as_str()));
}

#[test]
fn waiting_guidance_world_state_unchanged_snapshot_renders_nothing() {
    let capabilities = active_capabilities();
    let section = single_section(true, &capabilities, None);
    let snapshot = section.snapshot().clone();
    assert!(
        section
            .render_diff(PreviousWorldStateSection::Known(&snapshot))
            .is_none()
    );
}

#[test]
fn waiting_guidance_world_state_changed_snapshot_renders_again() {
    let capabilities = active_capabilities();
    let section = single_section(true, &capabilities, None);
    // A different active snapshot re-renders the full current fragment.
    let other = waiting_world_state::goal_waiting_guidance_sections(
        true,
        &WaitingCapabilities::none(),
        None,
    );
    assert_eq!(other.len(), 1);
    let other_snapshot = other[0].snapshot().clone();
    assert_ne!(other_snapshot, *section.snapshot());
    let text = rendered_text(&section, PreviousWorldStateSection::Known(&other_snapshot));
    assert!(text.contains("multi_agent_v1.wait_agent"));
    // Reactivation after a retraction renders as well.
    let reactivated = rendered_text(
        &section,
        PreviousWorldStateSection::Known(&Value::Bool(false)),
    );
    assert!(reactivated.contains("multi_agent_v1.wait_agent"));
}

#[test]
fn waiting_guidance_world_state_unknown_arms_fail_visible() {
    // No legacy matcher is registered, so `Unknown` is unreachable in
    // production; both arms are pinned to fail visible. An untyped history
    // hit cannot prove the five flags and the tool name still match, and a
    // possibly-stale history under an inactive goal must be corrected.
    let capabilities = active_capabilities();
    let active = single_section(true, &capabilities, None);
    let text = rendered_text(&active, PreviousWorldStateSection::Unknown);
    assert!(text.contains("multi_agent_v1.wait_agent"));

    let mut previous = Map::new();
    previous.insert(
        waiting_world_state::WORLD_STATE_ID.to_string(),
        active.snapshot().clone(),
    );
    let inactive = single_section(false, &WaitingCapabilities::none(), Some(&previous));
    let retraction = rendered_text(&inactive, PreviousWorldStateSection::Unknown);
    let expected = format!(
        "<goal_waiting_guidance>{}</goal_waiting_guidance>",
        waiting_world_state::WAITING_GUIDANCE_RETRACTION
    );
    assert_eq!(retraction, expected);
}

#[test]
fn waiting_guidance_world_state_retracts_on_active_to_inactive() {
    let capabilities = active_capabilities();
    let active = single_section(true, &capabilities, None);
    let active_snapshot = active.snapshot().clone();
    let mut previous = Map::new();
    previous.insert(
        waiting_world_state::WORLD_STATE_ID.to_string(),
        active_snapshot.clone(),
    );
    let inactive = single_section(false, &WaitingCapabilities::none(), Some(&previous));
    assert_eq!(inactive.id(), waiting_world_state::WORLD_STATE_ID);
    assert_eq!(*inactive.snapshot(), Value::Bool(false));
    assert!(!inactive.has_retained_fragment_matcher());

    // The recorded active snapshot means stale guidance may still be
    // model-visible: exactly one retraction, byte-exact.
    let Some(fragment) = inactive.render_diff(PreviousWorldStateSection::Known(&active_snapshot))
    else {
        panic!("expected exactly one retraction");
    };
    assert_eq!(fragment.role(), "developer");
    assert_eq!(
        fragment.body(),
        waiting_world_state::WAITING_GUIDANCE_RETRACTION
    );

    // Already retracted, or defensively no recorded entry: stay silent.
    assert!(
        inactive
            .render_diff(PreviousWorldStateSection::Known(&Value::Bool(false)))
            .is_none()
    );
    assert!(
        inactive
            .render_diff(PreviousWorldStateSection::Absent)
            .is_none()
    );
}

#[test]
fn waiting_guidance_world_state_retained_matcher_distinguishes_active_from_retraction() {
    let active = single_section(true, &active_capabilities(), None);
    let active_text = rendered_text(&active, PreviousWorldStateSection::Absent);
    assert!(active.matches_retained_fragment("developer", active_text.as_str()));

    // The positive-exec variant counts as retained guidance too.
    let positive = single_section(
        true,
        &WaitingCapabilities {
            exec_notify: true,
            ..WaitingCapabilities::none()
        },
        None,
    );
    let positive_text = rendered_text(&positive, PreviousWorldStateSection::Absent);
    assert!(active.matches_retained_fragment("developer", positive_text.as_str()));

    // The marker-sharing retraction is never retained guidance: matching it
    // would suppress the re-render after compaction evicted the real thing.
    let mut previous = Map::new();
    previous.insert(
        waiting_world_state::WORLD_STATE_ID.to_string(),
        active.snapshot().clone(),
    );
    let inactive = single_section(false, &WaitingCapabilities::none(), Some(&previous));
    let retraction = rendered_text(
        &inactive,
        PreviousWorldStateSection::Known(active.snapshot()),
    );
    assert!(!active.matches_retained_fragment("developer", retraction.as_str()));

    // Wrong role, unmarked text, and bare markers never match.
    assert!(!active.matches_retained_fragment("user", active_text.as_str()));
    assert!(!active.matches_retained_fragment("developer", "waiting guidance"));
    assert!(!active.matches_retained_fragment(
        "developer",
        "<goal_waiting_guidance></goal_waiting_guidance>"
    ));
}

#[test]
fn waiting_guidance_world_state_snapshot_is_bounded_and_capped() {
    // The snapshot carries the five flags plus the capped display name: the
    // same string the fragment shows.
    let overlong = "w".repeat(steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS + 40);
    let capabilities = WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::plain(overlong)),
        exec_notify: true,
        native_mail_wait: true,
    };
    let section = single_section(true, &capabilities, None);
    let snapshot = section.snapshot();
    let expected_name = format!("{}…", "w".repeat(steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS));
    assert_eq!(
        snapshot,
        &serde_json::json!({
            "exec_notify": true,
            "native_mail_wait": true,
            "native_wait": expected_name,
            "sleep": true,
            "stdin": true,
        })
    );
    let serialized = serde_json::to_string(snapshot).unwrap_or_default();
    assert!(
        serialized.len() <= 512,
        "snapshot must stay small in rollouts: {serialized}"
    );

    // An absent native wait is omitted, never null: core strips null object
    // fields before persisting, so a null would never compare equal again.
    let plain = single_section(true, &WaitingCapabilities::none(), None);
    assert_eq!(
        plain.snapshot(),
        &serde_json::json!({
            "exec_notify": false,
            "native_mail_wait": false,
            "sleep": false,
            "stdin": false,
        })
    );
}

#[test]
fn waiting_guidance_world_state_snapshot_ignores_beyond_cap_name_differences() {
    // Names differing only beyond the cap render identically, so their
    // snapshots compare equal and cannot churn the section.
    let cap = steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS;
    let left = single_section(
        true,
        &WaitingCapabilities {
            native_wait: Some(ToolName::plain(format!("{}-left", "w".repeat(cap)))),
            ..WaitingCapabilities::none()
        },
        None,
    );
    let right = single_section(
        true,
        &WaitingCapabilities {
            native_wait: Some(ToolName::plain(format!("{}-right", "w".repeat(cap)))),
            ..WaitingCapabilities::none()
        },
        None,
    );
    assert_eq!(left.snapshot(), right.snapshot());
    assert!(
        left.render_diff(PreviousWorldStateSection::Known(right.snapshot()))
            .is_none()
    );
}
