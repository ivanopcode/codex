//! Derives the sampling-step [`WaitingCapabilities`] from the final tool router.
//!
//! The snapshot is computed once per sampling step from the finalized
//! [`ToolRouter`]: exclusions and name collisions are already resolved, so a
//! tool that is absent or shadowed in the plan yields `false` here. A tool
//! flag is true when the tool is directly model-visible, or when it is
//! deferred in the registry and tool search has loaded it into this thread's
//! history (discovered). Undiscovered deferred tools and nested-only
//! code-mode tools do not count. Host and policy markers come from the thread
//! store only: acknowledged completion subscriptions (receipts, mailbox
//! entries, pending work) are tool-result data and never capability flags.

use std::collections::HashSet;

use crate::tools::router::ToolRouter;
use codex_extension_api::AsyncNotificationSupport;
use codex_extension_api::ExtensionData;
use codex_extension_api::GoalBackgroundWaitAdmission;
use codex_extension_api::WaitingCapabilities;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::models::ResponseItem;
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
/// supplies the host-support and installed-policy markers; `discovered`
/// carries the waiting tools tool search has loaded into this thread's
/// history (see [`discovered_waiting_tools_from_history`]). Nothing else —
/// in particular no receipt, mailbox, or pending-work state — influences
/// the result.
pub(crate) fn derive_waiting_capabilities(
    router: &ToolRouter,
    thread_store: &ExtensionData,
    discovered: &HashSet<ToolName>,
) -> WaitingCapabilities {
    let specs = router.model_visible_specs();
    let sleep = is_callable(
        router,
        &specs,
        discovered,
        &ToolName::namespaced(CLOCK_NAMESPACE, SLEEP_TOOL_NAME),
    );
    let stdin = is_callable(
        router,
        &specs,
        discovered,
        &ToolName::plain(WRITE_STDIN_TOOL_NAME),
    );
    let native_wait = find_native_wait_agent(router, &specs, discovered);
    let host_available = AsyncNotificationSupport::read_from(thread_store).is_available();
    let exec_notify = host_available
        && exec_command_advertises_notify_on_exit(router, &specs, discovered)
        && stdin
        && is_callable(
            router,
            &specs,
            discovered,
            &ToolName::plain(EXEC_NOTIFICATION_TOOL_NAME),
        );
    let _ = thread_store.get::<GoalBackgroundWaitAdmission>();
    let native_mail_wait = host_available;
    WaitingCapabilities {
        sleep,
        stdin,
        native_wait,
        exec_notify,
        native_mail_wait,
    }
}

/// Collects the waiting tools tool search has loaded into history.
///
/// Scans `ToolSearchOutput` items for loadable specs naming a waiting tool:
/// `clock.sleep`, `write_stdin`, any `wait_agent`, `exec_command`, or
/// `exec_notification`. Names are normalized with the default namespace so
/// callers can compare directly. Tools that were later excluded or lost a
/// collision are still returned here; derivation re-checks the finalized
/// registry before counting them.
pub(crate) fn discovered_waiting_tools_from_history<'a>(
    items: impl Iterator<Item = &'a ResponseItem>,
) -> HashSet<ToolName> {
    let mut discovered = HashSet::new();
    for item in items {
        let ResponseItem::ToolSearchOutput { tools, .. } = item else {
            continue;
        };
        for value in tools {
            for name in tool_names_from_loadable_value(value) {
                if is_waiting_relevant(&name) {
                    discovered.insert(name.with_default_namespace());
                }
            }
        }
    }
    discovered
}

/// Whether `name` is a waiting tool the snapshot can advertise.
fn is_waiting_relevant(name: &ToolName) -> bool {
    match name.name.as_str() {
        SLEEP_TOOL_NAME => name.namespace.as_deref() == Some(CLOCK_NAMESPACE),
        WRITE_STDIN_TOOL_NAME | EXEC_COMMAND_TOOL_NAME | EXEC_NOTIFICATION_TOOL_NAME => {
            name.is_default_namespace()
        }
        WAIT_AGENT_TOOL_NAME => true,
        _ => false,
    }
}

/// Extracts [`ToolName`]s from one serialized loadable spec value.
///
/// Mirrors the handler's normalization: a top-level function becomes a plain
/// name, and namespace children in the default `functions` namespace become
/// plain names as well. Malformed values yield nothing.
fn tool_names_from_loadable_value(value: &serde_json::Value) -> Vec<ToolName> {
    let Some(spec_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    if spec_type == "function" {
        return value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(|name| vec![ToolName::plain(name.to_string())])
            .unwrap_or_default();
    }
    if spec_type != "namespace" {
        return Vec::new();
    }
    let Some(namespace) = value.get("name").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let Some(tools) = value.get("tools").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .map(|name| {
            if namespace == DEFAULT_FUNCTION_NAMESPACE {
                ToolName::plain(name.to_string())
            } else {
                ToolName::namespaced(namespace.to_string(), name.to_string())
            }
        })
        .collect()
}

/// Whether `name` is callable as a model tool in this sampling step.
///
/// Directly-visible tools count. Deferred tools count only when tool search
/// has discovered them: the name must appear in `discovered` and the
/// finalized registry must still hold it as deferred, so exclusions and
/// collisions (which remove or replace the registry entry) keep yielding
/// false. Nested-only code-mode tools never count.
fn is_callable(
    router: &ToolRouter,
    specs: &[ToolSpec],
    discovered: &HashSet<ToolName>,
    name: &ToolName,
) -> bool {
    if is_directly_visible(specs, name) {
        return true;
    }
    discovered.contains(&name.clone().with_default_namespace())
        && router
            .tool_exposure(name)
            .is_some_and(|exposure| exposure.is_deferred())
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

/// Finds the callable native agent wait tool, if any.
///
/// Reads the configured native identity from the finalized registry (V1 or
/// the configured V2 namespace, plain when unnamespaced) and requires native
/// runtime provenance: unrelated dynamic, MCP, or extension tools named
/// `wait_agent` never match. The registered native tool must then be
/// directly visible or deferred-but-discovered, like every other flag.
fn find_native_wait_agent(
    router: &ToolRouter,
    specs: &[ToolSpec],
    discovered: &HashSet<ToolName>,
) -> Option<ToolName> {
    let name = router.native_wait_tool()?;
    is_callable(router, specs, discovered, &name).then_some(name)
}

/// Whether the callable `exec_command` schema exposes the `notify_on_exit`
/// opt-in the model can set.
///
/// Checks the directly advertised schema first; a deferred-but-discovered
/// `exec_command` falls back to its registry spec. An excluded
/// `exec_command` has no advertised schema and yields `false`. Incapable
/// hosts omit the property even when the tool itself is visible.
fn exec_command_advertises_notify_on_exit(
    router: &ToolRouter,
    specs: &[ToolSpec],
    discovered: &HashSet<ToolName>,
) -> bool {
    if let Some(spec) = specs
        .iter()
        .find(|spec| spec.name() == EXEC_COMMAND_TOOL_NAME)
    {
        return match spec {
            ToolSpec::Function(spec) => spec
                .parameters
                .properties
                .as_ref()
                .is_some_and(|properties| properties.contains_key(NOTIFY_ON_EXIT_PROPERTY)),
            ToolSpec::Namespace(_)
            | ToolSpec::ToolSearch { .. }
            | ToolSpec::WebSearch { .. }
            | ToolSpec::Freeform(_) => false,
        };
    }
    let name = ToolName::plain(EXEC_COMMAND_TOOL_NAME);
    if !discovered.contains(&name.clone().with_default_namespace()) {
        return false;
    }
    if !router
        .tool_exposure(&name)
        .is_some_and(|exposure| exposure.is_deferred())
    {
        return false;
    }
    router
        .tool_runtime(&name)
        .is_some_and(|runtime| match runtime.spec() {
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
