//! Derives the sampling-step [`WaitingCapabilities`] from the final tool router.
//!
//! The snapshot is computed once per sampling step from the finalized
//! [`ToolRouter`]: exclusions and name collisions are already resolved, so a
//! tool that is absent or shadowed in the plan yields `false` here. Every
//! tool flag requires direct model visibility; deferred tools do not count
//! until discovery promotes them, and nested-only code-mode tools do not
//! count either. Host and policy markers come from the thread store only:
//! acknowledged completion subscriptions (receipts, mailbox entries, pending
//! work) are tool-result data and never capability flags.

use crate::tools::router::ToolRouter;
use codex_extension_api::AsyncNotificationSupport;
use codex_extension_api::ExtensionData;
use codex_extension_api::GoalBackgroundWaitAdmission;
use codex_extension_api::WaitingCapabilities;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

const CLOCK_NAMESPACE: &str = "clock";
const SLEEP_TOOL_NAME: &str = "sleep";
const WRITE_STDIN_TOOL_NAME: &str = "write_stdin";
const EXEC_COMMAND_TOOL_NAME: &str = "exec_command";
const EXEC_NOTIFICATION_TOOL_NAME: &str = "exec_notification";
const WAIT_AGENT_TOOL_NAME: &str = "wait_agent";
const NOTIFY_ON_EXIT_PROPERTY: &str = "notify_on_exit";

/// Derives the waiting snapshot for one sampling step.
///
/// `router` must be the final captured router for the step; `thread_store`
/// supplies the host-support and installed-policy markers. Nothing else —
/// in particular no receipt, mailbox, or pending-work state — influences
/// the result.
pub(crate) fn derive_waiting_capabilities(
    router: &ToolRouter,
    thread_store: &ExtensionData,
) -> WaitingCapabilities {
    let specs = router.model_visible_specs();
    let sleep = is_directly_visible(
        &specs,
        &ToolName::namespaced(CLOCK_NAMESPACE, SLEEP_TOOL_NAME),
    );
    let stdin = is_directly_visible(&specs, &ToolName::plain(WRITE_STDIN_TOOL_NAME));
    let native_wait = find_direct_wait_agent(&specs);
    let host_available = AsyncNotificationSupport::read_from(thread_store).is_available();
    let exec_notify = host_available
        && exec_command_advertises_notify_on_exit(&specs)
        && is_directly_visible(&specs, &ToolName::plain(EXEC_NOTIFICATION_TOOL_NAME));
    let native_mail_wait =
        host_available && thread_store.get::<GoalBackgroundWaitAdmission>().is_some();
    WaitingCapabilities {
        sleep,
        stdin,
        native_wait,
        exec_notify,
        native_mail_wait,
    }
}

/// Whether `name` is directly model-visible in the finalized plan.
///
/// This mirrors the direct branch of `ToolRouter::exposes_tool` while
/// deliberately excluding nested code-mode tools and deferred tools: neither
/// is callable as a direct model tool in this sampling step.
fn is_directly_visible(specs: &[ToolSpec], name: &ToolName) -> bool {
    let name = name.clone().with_default_namespace();
    specs.iter().any(|spec| match spec {
        ToolSpec::Function(_) | ToolSpec::Freeform(_) => {
            name.is_default_namespace() && spec.name() == name.name
        }
        ToolSpec::Namespace(namespace) => {
            name.namespace.as_deref() == Some(namespace.name.as_str())
                && namespace.tools.iter().any(|tool| match tool {
                    ResponsesApiNamespaceTool::Function(tool) => tool.name == name.name,
                    ResponsesApiNamespaceTool::Custom(tool) => tool.name == name.name,
                })
        }
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => false,
    })
}

/// Finds the directly-visible native agent wait tool, if any.
///
/// Both the V1 (`multi_agent_v1.wait_agent`) and V2 (configured namespace or
/// plain `wait_agent`) registrations share the `wait_agent` tool name, so the
/// scan matches the name in any namespace and reports the actual [`ToolName`]
/// found. Only one multi-agent version registers wait tools per step; if
/// several ever appear, the first in model-visible order wins.
fn find_direct_wait_agent(specs: &[ToolSpec]) -> Option<ToolName> {
    specs.iter().find_map(|spec| match spec {
        ToolSpec::Function(tool) => {
            (tool.name == WAIT_AGENT_TOOL_NAME).then(|| ToolName::plain(tool.name.clone()))
        }
        ToolSpec::Freeform(tool) => {
            (tool.name == WAIT_AGENT_TOOL_NAME).then(|| ToolName::plain(tool.name.clone()))
        }
        ToolSpec::Namespace(namespace) => namespace
            .tools
            .iter()
            .any(|tool| match tool {
                ResponsesApiNamespaceTool::Function(tool) => tool.name == WAIT_AGENT_TOOL_NAME,
                ResponsesApiNamespaceTool::Custom(tool) => tool.name == WAIT_AGENT_TOOL_NAME,
            })
            .then(|| ToolName::namespaced(namespace.name.clone(), WAIT_AGENT_TOOL_NAME)),
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => None,
    })
}

/// Whether the advertised `exec_command` schema exposes the `notify_on_exit`
/// opt-in the model can set.
///
/// An excluded `exec_command` has no advertised schema and yields `false`.
/// Incapable hosts omit the property even when the tool itself is visible.
fn exec_command_advertises_notify_on_exit(specs: &[ToolSpec]) -> bool {
    specs
        .iter()
        .find(|spec| spec.name() == EXEC_COMMAND_TOOL_NAME)
        .is_some_and(|spec| match spec {
            ToolSpec::Function(spec) => spec
                .parameters
                .properties
                .as_ref()
                .is_some_and(|properties| properties.contains_key(NOTIFY_ON_EXIT_PROPERTY)),
            ToolSpec::Namespace(_)
            | ToolSpec::ToolSearch { .. }
            | ToolSpec::WebSearch { .. }
            | ToolSpec::Freeform(_) => false,
        })
}

#[cfg(test)]
#[path = "waiting_capabilities_tests.rs"]
mod tests;
