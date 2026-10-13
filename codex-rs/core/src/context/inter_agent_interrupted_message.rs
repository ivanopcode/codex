use codex_protocol::AgentPath;
use codex_utils_string::take_bytes_at_char_boundary;

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Maximum rendered bytes kept from each agent path before truncation.
///
/// `AgentPath` bounds segment and total length where paths are created; this
/// truncation stays as defense in depth, so the rendered notice keeps its
/// fixed upper bound in [`INTERRUPTED_MESSAGE_MAX_BYTES`] however the input
/// paths were produced.
const INTERRUPTED_PATH_MAX_BYTES: usize = 64;

/// Marker appended when a path exceeds [`INTERRUPTED_PATH_MAX_BYTES`].
const TRUNCATED_PATH_MARKER: &str = "…[truncated]";

/// Fixed notice prose around the two interpolated paths.
const BODY_PREFIX: &str = "Message Type: INTERRUPTED\nTask name: ";
const BODY_MIDDLE: &str = "\nSender: ";
const BODY_SUFFIX: &str = "\nPayload:\nAgent interrupted. Its turn ended without a final answer; resend work if still needed.";

/// Hard upper bound on the rendered notice body in bytes: fixed prose plus
/// two truncated paths of at most [`INTERRUPTED_PATH_MAX_BYTES`] bytes each,
/// with the truncation marker appended when truncated.
const INTERRUPTED_MESSAGE_MAX_BYTES: usize = BODY_PREFIX.len()
    + BODY_MIDDLE.len()
    + BODY_SUFFIX.len()
    + 2 * (INTERRUPTED_PATH_MAX_BYTES + TRUNCATED_PATH_MARKER.len());

/// Hard cap for the whole plaintext interrupted item: author and recipient
/// display strings plus all content text. For byte-level BPE tokenizers,
/// tokens <= UTF-8 bytes, so this also bounds the item to at most 1000 tokens.
/// Routing identities remain unchanged; only the body's display copies use
/// the smaller path cap. The envelope uses `AgentPath`'s existing display cap
/// and the same truncation marker.
const INTERRUPTED_ITEM_MAX_BYTES: usize = 1000;
const _: () = assert!(
    INTERRUPTED_MESSAGE_MAX_BYTES
        + 2 * (AgentPath::MODEL_DISPLAY_MAX_BYTES + TRUNCATED_PATH_MARKER.len())
        <= INTERRUPTED_ITEM_MAX_BYTES
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentInterruptedMessage {
    task_name: AgentPath,
    sender: AgentPath,
}

impl InterAgentInterruptedMessage {
    pub(crate) fn new(task_name: AgentPath, sender: AgentPath) -> Self {
        Self { task_name, sender }
    }
}

/// Renders an agent path for model-visible text, truncating it to
/// [`INTERRUPTED_PATH_MAX_BYTES`] bytes with a marker when over the cap.
/// Paths at or under the cap render byte-identical.
fn truncate_path(path: &AgentPath) -> String {
    const INTERRUPTED_PATH_MAX_BYTES: usize = 256;
    let rendered = path.as_str();
    if rendered.len() <= INTERRUPTED_PATH_MAX_BYTES {
        return rendered.to_string();
    }
    format!(
        "{}{TRUNCATED_PATH_MARKER}",
        take_bytes_at_char_boundary(rendered, INTERRUPTED_PATH_MAX_BYTES)
    )
}

impl ContextualUserFragment for InterAgentInterruptedMessage {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.inter_agent_interrupted_message".to_string())
    }

    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        let task_name = truncate_path(&self.task_name);
        let sender = truncate_path(&self.sender);
        let body = format!("{BODY_PREFIX}{task_name}{BODY_MIDDLE}{sender}{BODY_SUFFIX}");
        debug_assert!(body.len() <= INTERRUPTED_MESSAGE_MAX_BYTES);
        body
    }
}

#[cfg(test)]
#[path = "inter_agent_interrupted_message_tests.rs"]
mod tests;
