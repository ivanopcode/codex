use codex_protocol::AgentPath;
use pretty_assertions::assert_eq;

use super::INTERRUPTED_MESSAGE_MAX_BYTES;
use super::INTERRUPTED_PATH_MAX_BYTES;
use super::InterAgentInterruptedMessage;
use crate::context::ContextualUserFragment;

#[test]
fn interrupted_fragment_renders_exact_notice_body() {
    let fragment = InterAgentInterruptedMessage::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
    );
    assert_eq!(
        fragment.render(),
        "Message Type: INTERRUPTED\nTask name: /root\nSender: /root/worker\nPayload:\nAgent interrupted. Its turn ended without a final answer; resend work if still needed."
    );
}

#[test]
fn interrupted_fragment_truncates_overlong_paths_to_hard_cap() {
    // Valid at the source (segments <= 128 bytes, path <= 1024 bytes),
    // yet longer than the fragment's per-path cap.
    let task_rendered = format!("/root/{}", vec!["t".repeat(100); 9].join("/"));
    let sender_rendered = format!("/root/{}", vec!["s".repeat(100); 9].join("/"));
    let fragment = InterAgentInterruptedMessage::new(
        AgentPath::try_from(task_rendered.as_str()).expect("valid agent path"),
        AgentPath::try_from(sender_rendered.as_str()).expect("valid agent path"),
    );
    let rendered = fragment.render();
    let expected = format!(
        "Message Type: INTERRUPTED\nTask name: {}…[truncated]\nSender: {}…[truncated]\nPayload:\nAgent interrupted. Its turn ended without a final answer; resend work if still needed.",
        &task_rendered[..INTERRUPTED_PATH_MAX_BYTES],
        &sender_rendered[..INTERRUPTED_PATH_MAX_BYTES],
    );
    assert_eq!(rendered, expected);
    assert!(
        rendered.len() <= INTERRUPTED_MESSAGE_MAX_BYTES,
        "rendered notice exceeds hard cap: {} > {INTERRUPTED_MESSAGE_MAX_BYTES}",
        rendered.len()
    );
}
