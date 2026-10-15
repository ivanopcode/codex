use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AgentStatus;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentNotification {
    pub(crate) agent_reference: String,
    pub(crate) status: AgentStatus,
}

impl SubagentNotification {
    pub(crate) fn new(agent_reference: impl Into<String>, status: AgentStatus) -> Self {
        // JSON escapes each scalar to at most six bytes (non-ASCII scalars
        // remain UTF-8). These limits keep the entire fragment below 3 KiB.
        let errored = matches!(status, AgentStatus::Errored(_));
        let status = match status {
            AgentStatus::Completed(Some(text)) | AgentStatus::Errored(text) => {
                let mut bounded: String = text.chars().take(if errored { usize::MAX } else { 384 }).collect();
                if bounded.len() < text.len() {
                    bounded.push_str("\n[truncated]");
                }
                // Preserve the terminal classification while bounding its payload.
                if errored {
                    AgentStatus::Errored(bounded)
                } else {
                    AgentStatus::Completed(Some(bounded))
                }
            }
            status @ (AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Completed(None)
            | AgentStatus::Interrupted
            | AgentStatus::Shutdown
            | AgentStatus::NotFound) => status,
        };
        Self {
            agent_reference: agent_reference.into().chars().take(64).collect(),
            status,
        }
    }
}

impl ContextualUserFragment for SubagentNotification {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.subagent_notification".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagent_notification>", "</subagent_notification>")
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "agent_path": &self.agent_reference,
                "status": &self.status,
            })
        )
    }
}
