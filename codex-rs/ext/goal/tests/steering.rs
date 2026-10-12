//! Verifies conditional checklist guidance preserves goal instructions and task data.
#![allow(dead_code)]

#[path = "../src/steering.rs"]
mod steering;

use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::ContentItemKind;
use codex_extension_api::PromptSlot;
use codex_extension_api::ToolName;
use codex_extension_api::WaitingCapabilities;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_utils_template::Template;
use pretty_assertions::assert_eq;

#[test]
fn enabled_checklist_preserves_the_original_continuation_prompt() {
    let goal = test_goal("Finish the feature.");
    let original = Template::parse(include_str!("../templates/goals/continuation.md"))
        .expect("original continuation template")
        .render([
            ("objective", goal.objective.as_str()),
            ("tokens_used", "100"),
            ("token_budget", "10000"),
            ("remaining_tokens", "9900"),
        ])
        .expect("render original continuation prompt");
    let expected: ResponseItem = ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        original,
    ));
    assert_eq!(
        steering::continuation_steering_item(&goal, /*update_plan_enabled*/ true),
        expected,
    );
}

#[test]
fn disabled_checklist_preserves_goal_text_that_mentions_the_tool() {
    let objective = "Inspect update_plan.\n\n## Plan tool\nThis is user task data.";
    let item = steering::continuation_steering_item(
        &test_goal(objective),
        /*update_plan_enabled*/ false,
    );
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected goal continuation message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected goal continuation text");
    };
    assert!(text.contains(objective));
    assert!(!text.contains("If update_plan is available"));
    assert!(text.contains("Completion audit:"));
}

fn test_goal(objective: &str) -> ThreadGoal {
    ThreadGoal {
        thread_id: ThreadId::new(),
        objective: objective.to_string(),
        status: ThreadGoalStatus::Active,
        token_budget: Some(10_000),
        tokens_used: 100,
        time_used_seconds: 0,
        created_at: 0,
        updated_at: 0,
    }
}

/// Byte-exact §9 verified-wait paragraph (without the "- " bullet prefix).
const SECTION_NINE_CONTRACT_PARAGRAPH: &str = "A verified wait uses a concrete work handle or a runtime-acknowledged completion subscription. A fresh liveness poll is not required on every turn. Do independent work first. When only waiting remains, use the waiting capabilities supplied by the runtime for this sampling step. An acknowledged subscription permits ending the turn while the runtime waits; a session ID alone does not. Otherwise prefer a single long, input-interruptible wait to repeated short checks. The runtime controls continuation delays and check-in limits; do not simulate them with short polling loops. Conversation, intent, old output, and lock or state files alone are not current liveness evidence. A missing handle, observation timeout, failed lookup, or transient read error does not prove completion: distinguish a terminal result from lost observation before restarting work. Waiting alone is not a blocker, and it is not goal completion.";

fn continuation_text(objective: &str, update_plan_enabled: bool) -> String {
    let item = steering::continuation_steering_item(&test_goal(objective), update_plan_enabled);
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected goal continuation message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected goal continuation text");
    };
    text.clone()
}

#[test]
fn continuation_prompt_carries_section_nine_verified_wait_contract() {
    // AC1: both template variants (with and without update_plan) render the
    // exact §9 paragraph as a bullet and none of the removed text.
    for update_plan_enabled in [true, false] {
        let text = continuation_text("Finish the feature.", update_plan_enabled);
        let expected_bullet = format!("- {SECTION_NINE_CONTRACT_PARAGRAPH}");
        assert!(
            text.contains(expected_bullet.as_str()),
            "update_plan_enabled={update_plan_enabled}: missing §9 paragraph in:\n{text}",
        );
        assert!(
            !text.contains("confirmed live now"),
            "update_plan_enabled={update_plan_enabled}: removed text present in:\n{text}",
        );
        assert!(
            !text.contains("never restart solely because observation expired"),
            "update_plan_enabled={update_plan_enabled}: removed text present in:\n{text}",
        );
    }
}

#[test]
fn waiting_guidance_constants_match_section_nine_texts() {
    // AC2/AC8: pins the renderer constants to the byte-exact §9 sentences so
    // a rewording mutant fails here rather than silently drifting the prompt.
    assert_eq!(
        steering::WAITING_EXEC_NOTIFY,
        "exec_command can acknowledge notify_on_exit subscriptions. For finite background work, request a notification; after the tool acknowledges it, do independent work and then end the turn. Its completion will be delivered by the runtime. Use write_stdin only for needed intermediate output or interaction. Use exec_notification to read retained terminal output or release a mistaken subscription without terminating the process."
    );
    assert_eq!(
        steering::WAITING_NO_EXEC_NOTIFY,
        "This session cannot promise an exec completion wake. A yielded exec session by itself is not a reason to expect automatic resumption."
    );
    assert_eq!(
        steering::WAITING_SLEEP,
        "clock.sleep is available as a direct tool. Use a substantial input-interruptible wait when a notification subscription is unavailable. Do not place clock.sleep inside a code-mode exec cell."
    );
    assert_eq!(
        steering::WAITING_STDIN,
        "An empty write_stdin with a long yield_time_ms can wait for one known live exec session. Read again early only when intermediate output is needed to decide the next action."
    );
    assert_eq!(
        steering::WAITING_NATIVE_WAIT,
        "The native agent wait tool is available for Codex-managed children. Pass multiple child identifiers where that tool supports them to wait for whichever changes first. It cannot observe arbitrary shell or remote jobs."
    );
    assert_eq!(
        steering::WAITING_NATIVE_MAIL_WAIT,
        "When this active goal yields with owned native child work still running, the runtime can wait for child mailbox results. Do not repeatedly inspect child logs merely to keep the goal alive."
    );
}

#[test]
fn waiting_guidance_renders_exact_sentences_in_fixed_order() {
    // AC2: one developer-capabilities fragment; the exec sentence first, then
    // sleep, stdin, native wait (with the snapshot's actual tool name), and
    // native mail wait. The deferred no-wait/end-turn paragraph is absent.
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        exec_notify: true,
        native_mail_wait: true,
    });
    assert_eq!(fragments.len(), 1);
    let fragment = &fragments[0];
    assert_eq!(fragment.slot(), PromptSlot::DeveloperCapabilities);
    assert_eq!(
        fragment.content_kind(),
        &ContentItemKind("goal.waiting_capabilities".to_string()),
    );
    let native_line = format!(
        "{} (Tool name: multi_agent_v1.wait_agent.)",
        steering::WAITING_NATIVE_WAIT
    );
    let expected = [
        steering::WAITING_EXEC_NOTIFY,
        steering::WAITING_SLEEP,
        steering::WAITING_STDIN,
        native_line.as_str(),
        steering::WAITING_NATIVE_MAIL_WAIT,
    ]
    .join("\n");
    assert_eq!(fragment.text(), expected);
    assert!(
        !fragment.text().contains("No blocking or notification wait"),
        "deferred no-wait paragraph must not render: {}",
        fragment.text(),
    );
}

#[test]
fn waiting_guidance_selects_sentences_per_flag() {
    // AC2: exactly one exec sentence always; every other sentence iff its
    // flag, with the snapshot's actual (not hard-coded) native wait name.
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities::none());
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].text(), steering::WAITING_NO_EXEC_NOTIFY);

    for (capabilities, present, absent) in [
        (
            WaitingCapabilities {
                sleep: true,
                ..WaitingCapabilities::none()
            },
            vec![steering::WAITING_SLEEP, steering::WAITING_NO_EXEC_NOTIFY],
            vec![
                steering::WAITING_EXEC_NOTIFY,
                steering::WAITING_STDIN,
                steering::WAITING_NATIVE_WAIT,
                steering::WAITING_NATIVE_MAIL_WAIT,
            ],
        ),
        (
            WaitingCapabilities {
                stdin: true,
                native_mail_wait: true,
                ..WaitingCapabilities::none()
            },
            vec![
                steering::WAITING_STDIN,
                steering::WAITING_NATIVE_MAIL_WAIT,
                steering::WAITING_NO_EXEC_NOTIFY,
            ],
            vec![
                steering::WAITING_EXEC_NOTIFY,
                steering::WAITING_SLEEP,
                steering::WAITING_NATIVE_WAIT,
            ],
        ),
        (
            WaitingCapabilities {
                exec_notify: true,
                native_wait: Some(ToolName::namespaced("custom_wait_ns", "wait_agent")),
                ..WaitingCapabilities::none()
            },
            vec![steering::WAITING_EXEC_NOTIFY, steering::WAITING_NATIVE_WAIT],
            vec![
                steering::WAITING_NO_EXEC_NOTIFY,
                steering::WAITING_SLEEP,
                steering::WAITING_STDIN,
                steering::WAITING_NATIVE_MAIL_WAIT,
            ],
        ),
    ] {
        let fragments = steering::waiting_capability_fragments(&capabilities);
        assert_eq!(fragments.len(), 1);
        let text = fragments[0].text().to_string();
        for sentence in present {
            assert!(text.contains(sentence), "missing {sentence} in:\n{text}");
        }
        for sentence in absent {
            assert!(
                !text.contains(sentence),
                "unexpected {sentence} in:\n{text}"
            );
        }
    }

    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        native_wait: Some(ToolName::namespaced("custom_wait_ns", "wait_agent")),
        ..WaitingCapabilities::none()
    });
    assert!(
        fragments[0].text().contains("custom_wait_ns.wait_agent"),
        "native wait must render the snapshot's actual name: {}",
        fragments[0].text(),
    );
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        native_wait: Some(ToolName::plain("wait_agent")),
        ..WaitingCapabilities::none()
    });
    assert!(
        fragments[0].text().contains("(Tool name: wait_agent.)"),
        "plain native wait name must render bare: {}",
        fragments[0].text(),
    );
}

// AC4 bounds: 3 KiB and 1,000 tokens for the maximal fragment.
const MAX_WAITING_GUIDANCE_BYTES: usize = 3072;
const MAX_WAITING_GUIDANCE_TOKENS: usize = 1000;

/// Repo token estimate: `codex_utils_string::approx_tokens_from_byte_count`
/// is `ceil(bytes / 4)` (`APPROX_BYTES_PER_TOKEN = 4`). Computed inline so
/// this crate gains no new dev-dependency (and no `Cargo.lock` churn).
fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[test]
fn waiting_guidance_fragment_is_bounded_over_every_flag_combination() {
    // AC4: every combination of the four boolean flags crossed with native
    // wait absent, short-named, overlong ASCII, and overlong wide-character
    // names stays within both bounds.
    let overlong = format!("{}.{}", "n".repeat(100), "w".repeat(100));
    assert!(overlong.len() > steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS);
    // Four-byte characters maximize the byte length under the character cap.
    let overlong_wide = "𝄞".repeat(steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS + 40);
    let native_names = [
        None,
        Some(ToolName::plain("wait_agent")),
        Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        Some(ToolName::plain(overlong)),
        Some(ToolName::plain(overlong_wide)),
    ];
    let mut max_bytes = 0;
    let mut max_tokens = 0;
    for bits in 0..16u8 {
        for native_wait in &native_names {
            let capabilities = WaitingCapabilities {
                sleep: bits & 0b0001 != 0,
                stdin: bits & 0b0010 != 0,
                native_wait: native_wait.clone(),
                exec_notify: bits & 0b0100 != 0,
                native_mail_wait: bits & 0b1000 != 0,
            };
            let fragments = steering::waiting_capability_fragments(&capabilities);
            assert_eq!(fragments.len(), 1);
            let text = fragments[0].text();
            max_bytes = max_bytes.max(text.len());
            max_tokens = max_tokens.max(approx_tokens(text));
            assert!(
                text.len() <= MAX_WAITING_GUIDANCE_BYTES,
                "fragment exceeds 3 KiB for {capabilities:?}: {} bytes",
                text.len(),
            );
            assert!(
                approx_tokens(text) <= MAX_WAITING_GUIDANCE_TOKENS,
                "fragment exceeds 1K tokens for {capabilities:?}: {} tokens",
                approx_tokens(text),
            );
        }
    }
    assert!(
        max_bytes > 0 && max_tokens > 0,
        "bound test must observe a non-empty maximal fragment"
    );
}

#[test]
fn waiting_guidance_native_wait_tool_name_is_capped() {
    // AC4/AC8: the only variable datum is hard-capped; overlong names
    // truncate at a character boundary with a trailing ellipsis, short names
    // render untouched.
    let cap = steering::MAX_NATIVE_WAIT_TOOL_NAME_CHARS;
    let long_name = "w".repeat(cap + 72);
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        native_wait: Some(ToolName::plain(long_name)),
        ..WaitingCapabilities::none()
    });
    let expected = format!("{}…", "w".repeat(cap));
    let expected_line = format!("(Tool name: {expected}.)");
    assert!(
        fragments[0].text().contains(expected_line.as_str()),
        "overlong name must truncate to {cap} chars plus ellipsis: {}",
        fragments[0].text(),
    );

    let wide_name: String = "é".repeat(cap + 10);
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        native_wait: Some(ToolName::plain(wide_name)),
        ..WaitingCapabilities::none()
    });
    let expected = format!("{}…", "é".repeat(cap));
    let expected_line = format!("(Tool name: {expected}.)");
    assert!(
        fragments[0].text().contains(expected_line.as_str()),
        "multibyte truncation must respect character boundaries: {}",
        fragments[0].text(),
    );

    let exact_name = "w".repeat(cap);
    let fragments = steering::waiting_capability_fragments(&WaitingCapabilities {
        native_wait: Some(ToolName::plain(exact_name.clone())),
        ..WaitingCapabilities::none()
    });
    let expected_line = format!("(Tool name: {exact_name}.)");
    assert!(
        fragments[0].text().contains(expected_line.as_str()),
        "at-cap name must render untruncated: {}",
        fragments[0].text(),
    );
}
