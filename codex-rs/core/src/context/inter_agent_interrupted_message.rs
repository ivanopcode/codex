use codex_protocol::AgentPath;
use codex_utils_string::take_bytes_at_char_boundary;

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Maximum rendered bytes kept from each agent path before truncation.
///
/// Agent paths are ASCII-only but unbounded in length: the V2 spawn entry
/// passes `task_name` through `AgentPath::join` with no length check, so both
/// interpolated paths are truncated to keep the model-visible notice within
/// [`INTERRUPTED_MESSAGE_MAX_BYTES`].
const INTERRUPTED_PATH_MAX_BYTES: usize = 256;

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
