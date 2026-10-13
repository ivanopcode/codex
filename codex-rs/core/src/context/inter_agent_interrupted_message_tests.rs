use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;

use super::INTERRUPTED_ITEM_MAX_BYTES;
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
fn inter_agent_interrupted_dense_item_fits_byte_bound() {
    let legacy_parent = AgentPath::try_from(format!("/root/{}", "a1".repeat(150)))
        .expect("legacy identity remains valid");
    let new_parent = AgentPath::root()
        .join_new(&"a1".repeat(64))
        .expect("first segment at cap")
        .join_new(&"b2".repeat(64))
        .expect("second segment at cap");
    for parent in [legacy_parent, new_parent] {
        let child = parent.join_new("worker").expect("worker identity");
        // Same formatter and plaintext envelope as terminal-turn delivery.
        let communication = InterAgentCommunication::new(
            child.clone(),
            parent.clone(),
            Vec::new(),
            crate::session_prefix::format_inter_agent_interrupted_message(&parent, &child),
            /*trigger_turn*/ false,
        );
        let ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } = communication.to_model_input_item()
        else {
            panic!("interrupted communication must render an agent message");
        };
        let text_bytes: usize = content
            .iter()
            .map(|part| match part {
                AgentMessageInputContent::InputText { text } => text.len(),
                AgentMessageInputContent::EncryptedContent { .. } => {
                    panic!("interrupted delivery must be plaintext")
                }
            })
            .sum();
        let item_bytes = author.len() + recipient.len() + text_bytes;
        assert!(
            item_bytes <= INTERRUPTED_ITEM_MAX_BYTES,
            "interrupted item exceeds byte/token bound: {item_bytes}"
        );
        assert_eq!(
            item_bytes, 838,
            "dense fixtures attain the worst-case bound"
        );
        assert_eq!(
            (communication.author, communication.recipient),
            (child, parent)
        );
    }
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
