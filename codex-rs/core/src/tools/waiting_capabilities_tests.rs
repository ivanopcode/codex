use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::AsyncNotificationSupport;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::GoalActivity;
use codex_extension_api::GoalActivityState;
use codex_extension_api::GoalAdmissionDecision;
use codex_extension_api::GoalBackgroundWaitAdmission;
use codex_extension_api::PendingWorkProvider;
use codex_extension_api::PendingWorkReceipt;
use codex_extension_api::PendingWorkSnapshot;
use codex_extension_api::PromptFragment;
use codex_extension_api::ToolPolicy;
use codex_extension_api::TurnContextContributionInput;
use codex_extension_api::WaitingCapabilities;
use codex_features::Feature;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::derive_waiting_capabilities;
use super::discovered_waiting_tools_from_history;
use crate::config::CurrentTimeReminderConfig;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_turn_settings_for_test;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::DynamicToolHandler;
use crate::tools::handlers::SleepHandler;
use crate::tools::handlers::multi_agents::WaitAgentHandler;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_v2::WaitAgentHandler as WaitAgentHandlerV2;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use crate::tools::spec_plan::build_tool_router;

fn function_spec(name: &str) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: format!("{name} tool"),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), /*required*/ None, Some(false.into())),
        output_schema: None,
    })
}

fn namespace_spec(namespace: &str, tools: &[&str]) -> ToolSpec {
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: namespace.to_string(),
        description: format!("{namespace} tools"),
        tools: tools
            .iter()
            .map(|name| {
                ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: (*name).to_string(),
                    description: format!("{name} tool"),
                    strict: false,
                    defer_loading: None,
                    parameters: JsonSchema::object(
                        BTreeMap::new(),
                        /*required*/ None,
                        Some(false.into()),
                    ),
                    output_schema: None,
                })
            })
            .collect(),
    })
}

fn exec_command_spec(notify_on_exit: bool) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "cmd".to_string(),
        JsonSchema::string(Some("Shell command to execute.".to_string())),
    )]);
    if notify_on_exit {
        properties.insert(
            "notify_on_exit".to_string(),
            JsonSchema::boolean(Some("Request a completion wake.".to_string())),
        );
    }
    ToolSpec::Function(ResponsesApiTool {
        name: "exec_command".to_string(),
        description: "Runs a command.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["cmd".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn tool_search_spec() -> ToolSpec {
    ToolSpec::ToolSearch {
        execution: "client".to_string(),
        description: "Search tools.".to_string(),
        parameters: JsonSchema::default(),
    }
}

fn synthetic_router(
    registry: ToolRegistry,
    specs: Vec<ToolSpec>,
    code_mode_tool_names: BTreeMap<String, ToolName>,
) -> ToolRouter {
    ToolRouter::from_parts(
        registry,
        specs,
        ToolMode::Direct,
        code_mode_tool_names,
        /*tool_namespaces_info*/ None,
        &[],
    )
}

fn empty_router() -> ToolRouter {
    synthetic_router(ToolRegistry::empty_for_test(), Vec::new(), BTreeMap::new())
}

fn empty_thread_store() -> ExtensionData {
    ExtensionData::new("test-thread")
}

fn available_thread_store() -> ExtensionData {
    let store = empty_thread_store();
    store.insert(AsyncNotificationSupport::Available);
    store
}

fn install_wait_policy(store: &ExtensionData) {
    store.insert(GoalBackgroundWaitAdmission::new(|_| {
        GoalAdmissionDecision::Allow
    }));
}

fn full_waiting_specs(notify_on_exit: bool) -> Vec<ToolSpec> {
    vec![
        exec_command_spec(notify_on_exit),
        function_spec("write_stdin"),
        function_spec("exec_notification"),
        namespace_spec("clock", &["curr_time", "sleep"]),
        namespace_spec("collaboration", &["wait_agent"]),
    ]
}

fn v1_wait_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.add_with_exposure(
        WaitAgentHandler::new(WaitAgentTimeoutOptions::default()),
        ToolExposure::Direct,
    );
    registry
}

fn v2_wait_registry(namespace: Option<&str>) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    let handler = crate::tools::multi_agent_tool::multi_agent_v2_handler(
        WaitAgentHandlerV2::new(WaitAgentTimeoutOptions::default()),
        namespace,
        /*description_override*/ None,
        /*parameters_override*/ None,
    );
    registry.register_trusted_with_exposure(handler, ToolExposure::Direct);
    registry
}

fn full_waiting_router(notify_on_exit: bool) -> ToolRouter {
    synthetic_router(
        v2_wait_registry(Some("collaboration")),
        full_waiting_specs(notify_on_exit),
        BTreeMap::new(),
    )
}

#[test]
fn empty_router_yields_no_capabilities() {
    assert_eq!(
        derive_waiting_capabilities(&empty_router(), &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn direct_sleep_counts() {
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        vec![namespace_spec("clock", &["curr_time", "sleep"])],
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn nested_only_sleep_does_not_count() {
    // Kills the `sleep_counts_nested_or_deferred` mutant: nested-only tools
    // are not callable as direct model tools.
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        BTreeMap::from([(
            "clock__sleep".to_string(),
            ToolName::namespaced("clock", "sleep"),
        )]),
    );
    assert!(router.exposes_tool(&ToolName::namespaced("clock", "sleep")));
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn deferred_sleep_does_not_count_until_discovered() {
    // The deferred entry is discoverable through tool search but not callable
    // yet, so it must not flip the flag.
    let mut registry = ToolRegistry::default();
    registry.register_trusted_with_exposure(Arc::new(SleepHandler), ToolExposure::Deferred);
    let router = synthetic_router(registry, vec![tool_search_spec()], BTreeMap::new());
    assert!(router.exposes_tool(&ToolName::namespaced("clock", "sleep")));
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn discovered_deferred_sleep_counts() {
    // A-F2: a deferred tool becomes callable once tool search loads it.
    let mut registry = ToolRegistry::default();
    registry.register_trusted_with_exposure(Arc::new(SleepHandler), ToolExposure::Deferred);
    let router = synthetic_router(registry, vec![tool_search_spec()], BTreeMap::new());
    let discovered =
        HashSet::from([ToolName::namespaced("clock", "sleep").with_default_namespace()]);
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &discovered),
        WaitingCapabilities {
            sleep: true,
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn discovered_deferred_v1_wait_counts_with_native_provenance() {
    // A-F2 killer for the `native_wait_ignores_discovered` mutant: a deferred
    // V1 wait tool plus a history discovery yields `Some` with its actual
    // native name. Provenance still applies: the registry holds the native
    // runtime, not a decoy.
    let mut registry = ToolRegistry::default();
    registry.add_with_exposure(
        WaitAgentHandler::new(WaitAgentTimeoutOptions::default()),
        ToolExposure::Deferred,
    );
    let router = synthetic_router(registry, vec![tool_search_spec()], BTreeMap::new());
    let discovered = HashSet::from([
        ToolName::namespaced("multi_agent_v1", "wait_agent").with_default_namespace()
    ]);
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &discovered),
        WaitingCapabilities {
            native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn discovered_names_not_in_registry_still_yield_none() {
    // Exclusion/collision guard for A-F2: a history discovery for a tool the
    // finalized registry no longer holds (excluded or shadowed) must not
    // flip any flag.
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        vec![tool_search_spec()],
        BTreeMap::new(),
    );
    let discovered = HashSet::from([
        ToolName::namespaced("clock", "sleep").with_default_namespace(),
        ToolName::namespaced("multi_agent_v1", "wait_agent").with_default_namespace(),
        ToolName::plain("write_stdin").with_default_namespace(),
    ]);
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &discovered),
        WaitingCapabilities::none(),
    );
}

#[test]
fn history_parsing_extracts_waiting_tools_only() {
    let loadable = serde_json::json!({
        "type": "namespace",
        "name": "multi_agent_v1",
        "description": "Native agents.",
        "tools": [
            {"type": "function", "name": "wait_agent", "description": "Wait."},
            {"type": "function", "name": "spawn_agent", "description": "Spawn."},
        ],
    });
    let unrelated = serde_json::json!({
        "type": "namespace",
        "name": "calendar",
        "description": "Calendar.",
        "tools": [{"type": "function", "name": "create_event", "description": "Create."}],
    });
    let malformed = serde_json::json!({"type": "namespace"});
    let items = [
        tool_search_call("search-1"),
        tool_search_output(
            Some("search-1"),
            "client",
            vec![loadable, unrelated, malformed],
        ),
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(
        discovered_waiting_tools_from_history(items.iter()),
        HashSet::from([
            ToolName::namespaced("multi_agent_v1", "wait_agent").with_default_namespace()
        ]),
    );
}

fn wait_agent_loadable(namespace: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "namespace",
        "name": namespace,
        "description": "Native agents.",
        "tools": [
            {"type": "function", "name": "wait_agent", "description": "Wait."},
        ],
    })
}

fn tool_search_call(call_id: &str) -> ResponseItem {
    ResponseItem::ToolSearchCall {
        id: None,
        call_id: Some(call_id.to_string()),
        status: Some("completed".to_string()),
        execution: "client".to_string(),
        arguments: serde_json::json!({"query": "wait agent"}),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn tool_search_output(
    call_id: Option<&str>,
    execution: &str,
    tools: Vec<serde_json::Value>,
) -> ResponseItem {
    ResponseItem::ToolSearchOutput {
        id: None,
        call_id: call_id.map(str::to_string),
        status: "completed".to_string(),
        execution: execution.to_string(),
        tools,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn orphan_client_tool_search_output_does_not_count() {
    // Kills the `discovery_ignores_call_pairing` mutant (A2-F1/D-F1): a
    // client output with no matching `ToolSearchCall` is removed by prompt
    // normalization, so it must not count as discovered.
    let items = [tool_search_output(
        Some("orphan-search-1"),
        "client",
        vec![wait_agent_loadable("multi_agent_v1")],
    )];
    assert_eq!(
        discovered_waiting_tools_from_history(items.iter()),
        HashSet::new(),
    );
}

#[test]
fn mismatched_call_id_tool_search_output_does_not_count() {
    // A client output whose call id names a different search is still an
    // orphan: only the matching call id pairs.
    let items = [
        tool_search_call("search-other"),
        tool_search_output(
            Some("search-wait-9"),
            "client",
            vec![wait_agent_loadable("multi_agent_v1")],
        ),
    ];
    assert_eq!(
        discovered_waiting_tools_from_history(items.iter()),
        HashSet::new(),
    );
}

#[test]
fn server_tool_search_output_counts_without_call() {
    // Kills the `discovery_requires_server_call_id` mutant (A3-F1/D3-F1):
    // the normalizer retains server outputs with any call_id, including
    // `None`, so discovery must count both shapes.
    for call_id in [Some("server-search-1"), None] {
        let items = [tool_search_output(
            call_id,
            "server",
            vec![wait_agent_loadable("multi_agent_v1")],
        )];
        assert_eq!(
            discovered_waiting_tools_from_history(items.iter()),
            HashSet::from([
                ToolName::namespaced("multi_agent_v1", "wait_agent").with_default_namespace()
            ]),
        );
    }
}

#[test]
fn client_tool_search_output_without_call_id_counts_like_normalizer() {
    // The normalizer's `ToolSearchOutput` arm drops only `Some(id)` client
    // outputs without a matching call; a client output with `call_id: None`
    // is retained, so discovery must count it exactly as the normalizer
    // treats it.
    let items = [tool_search_output(
        /*call_id*/ None,
        "client",
        vec![wait_agent_loadable("multi_agent_v1")],
    )];
    assert_eq!(
        discovered_waiting_tools_from_history(items.iter()),
        HashSet::from([
            ToolName::namespaced("multi_agent_v1", "wait_agent").with_default_namespace()
        ]),
    );
}

#[test]
fn direct_write_stdin_counts() {
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        vec![function_spec("write_stdin")],
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            stdin: true,
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn nested_only_write_stdin_does_not_count() {
    // Kills the `stdin_counts_nested` mutant.
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        BTreeMap::from([("write_stdin".to_string(), ToolName::plain("write_stdin"))]),
    );
    assert!(router.exposes_tool(&ToolName::plain("write_stdin")));
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn v1_wait_agent_carries_its_namespaced_name() {
    // Kills the `native_wait_v2_only` mutant.
    let router = synthetic_router(
        v1_wait_registry(),
        vec![namespace_spec(
            "multi_agent_v1",
            &["send_input", "wait_agent", "resume_agent", "close_agent"],
        )],
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn v2_wait_agent_carries_its_configured_name() {
    // Kills the `native_wait_v1_only` mutant: the plain V2 registration
    // counts even though it has no V1 namespace.
    let router = synthetic_router(
        v2_wait_registry(/*namespace*/ None),
        vec![function_spec("wait_agent")],
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            native_wait: Some(ToolName::plain("wait_agent")),
            ..WaitingCapabilities::none()
        },
    );
}

#[test]
fn nested_only_wait_agent_does_not_count() {
    let router = synthetic_router(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        BTreeMap::from([(
            "collaboration__wait_agent".to_string(),
            ToolName::namespaced("collaboration", "wait_agent"),
        )]),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn foreign_dynamic_wait_agent_does_not_claim_native_wait() {
    // Kills the `native_wait_bare_name_match` mutant (A-F1): a directly
    // visible dynamic `foreign.wait_agent` with no native runtime registered
    // must not satisfy `native_wait`.
    let namespace = DynamicToolNamespaceSpec {
        name: "foreign".to_string(),
        description: "Foreign tools.".to_string(),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: "wait_agent".to_string(),
                description: "External callback.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            },
        )],
    };
    let DynamicToolNamespaceTool::Function(tool) = &namespace.tools[0];
    let handler = DynamicToolHandler::new_in_namespace(&namespace, tool)
        .expect("dynamic wait_agent should convert");
    let mut registry = ToolRegistry::default();
    registry.register_external(Arc::new(handler));
    let router = synthetic_router(
        registry,
        vec![namespace_spec("foreign", &["wait_agent"])],
        BTreeMap::new(),
    );
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "foreign"),
        "test setup should advertise the foreign decoy",
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &empty_thread_store(), &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

#[test]
fn exec_notify_true_with_host_opt_in_and_controls() {
    let router = full_waiting_router(/*notify_on_exit*/ true);
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: true,
            native_mail_wait: false,
        },
    );
}

#[test]
fn exec_notify_requires_available_host() {
    // Kills the `exec_notify_ignores_host` mutant: full controls on an
    // incapable host promise no wake.
    let router = full_waiting_router(/*notify_on_exit*/ true);
    let store = empty_thread_store();
    store.insert(AsyncNotificationSupport::Unavailable);
    assert_eq!(
        derive_waiting_capabilities(&router, &store, &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn exec_notify_requires_notify_opt_in() {
    // Kills the `exec_notify_ignores_opt_in` mutant.
    let router = full_waiting_router(/*notify_on_exit*/ false);
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn missing_exec_command_schema_disables_exec_notify() {
    // A missing exec schema reads as no opt-in from the final router.
    let specs: Vec<ToolSpec> = full_waiting_specs(/*notify_on_exit*/ true)
        .into_iter()
        .filter(|spec| spec.name() != "exec_command")
        .collect();
    let router = synthetic_router(
        v2_wait_registry(Some("collaboration")),
        specs,
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn exec_notify_requires_write_stdin() {
    // Kills the `exec_notify_ignores_stdin` mutant.
    let specs: Vec<ToolSpec> = full_waiting_specs(/*notify_on_exit*/ true)
        .into_iter()
        .filter(|spec| spec.name() != "write_stdin")
        .collect();
    let router = synthetic_router(
        v2_wait_registry(Some("collaboration")),
        specs,
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: false,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn exec_notify_requires_receipt_control() {
    // Kills the `exec_notify_ignores_receipt` mutant.
    let specs: Vec<ToolSpec> = full_waiting_specs(/*notify_on_exit*/ true)
        .into_iter()
        .filter(|spec| spec.name() != "exec_notification")
        .collect();
    let router = synthetic_router(
        v2_wait_registry(Some("collaboration")),
        specs,
        BTreeMap::new(),
    );
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn native_mail_wait_requires_host_and_installed_policy() {
    let router = full_waiting_router(/*notify_on_exit*/ true);
    let store = available_thread_store();
    install_wait_policy(&store);
    assert_eq!(
        derive_waiting_capabilities(&router, &store, &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: true,
            native_mail_wait: true,
        },
    );
}

#[test]
fn native_mail_wait_requires_available_host() {
    // Kills the `native_mail_wait_ignores_host` mutant.
    let router = full_waiting_router(/*notify_on_exit*/ true);
    let store = empty_thread_store();
    install_wait_policy(&store);
    assert_eq!(
        derive_waiting_capabilities(&router, &store, &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[test]
fn native_mail_wait_requires_installed_policy() {
    // Kills the `native_mail_wait_ignores_policy` mutant.
    let router = full_waiting_router(/*notify_on_exit*/ true);
    assert_eq!(
        derive_waiting_capabilities(&router, &available_thread_store(), &HashSet::new()),
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: true,
            native_mail_wait: false,
        },
    );
}

#[test]
fn pending_and_acknowledged_work_does_not_flip_flags() {
    let router = full_waiting_router(/*notify_on_exit*/ true);
    let without_provider = available_thread_store();
    let expected = derive_waiting_capabilities(&router, &without_provider, &HashSet::new());
    let with_provider = available_thread_store();
    with_provider.insert(PendingWorkProvider::new(|| {
        Ok(PendingWorkSnapshot::new(
            vec![PendingWorkReceipt::new("armed-receipt")],
            vec![PendingWorkReceipt::new("queued-receipt")],
            vec![PendingWorkReceipt::new("leased-receipt")],
            /*revision*/ 7,
        ))
    }));
    assert_eq!(
        derive_waiting_capabilities(&router, &with_provider, &HashSet::new()),
        expected,
    );
    // Receipts alone create no capability either.
    assert_eq!(
        derive_waiting_capabilities(&empty_router(), &with_provider, &HashSet::new()),
        WaitingCapabilities::none(),
    );
}

fn update_config(turn: &mut TurnContext, update: impl FnOnce(&mut crate::config::Config)) {
    let mut config = (*turn.config).clone();
    update(&mut config);
    turn.config = Arc::new(config);
}

fn set_feature(turn: &mut TurnContext, feature: Feature, enabled: bool) {
    // The toggled features never drive the multi-agent version (that follows
    // the Collab feature or an explicit override), so the version set by each
    // test stays put regardless of call order.
    let mut config = (*turn.config).clone();
    if enabled {
        config
            .features
            .enable(feature)
            .expect("test feature should be enableable in config");
    } else {
        config
            .features
            .disable(feature)
            .expect("test feature should be disableable in config");
    }
    turn.config = Arc::new(config);
}

fn goal_activity() -> GoalActivity {
    GoalActivity {
        goal_id: "goal-1".to_string(),
        revision: 1,
        state: GoalActivityState::Active,
    }
}

/// Builds a production tool router plus the snapshot derived from it.
///
/// The base disables the time reminder and multi-agent tools so each test
/// opts into exactly the surface it covers.
async fn snapshot_for(
    setup: impl FnOnce(&mut Session, &mut TurnContext),
) -> (ToolRouter, WaitingCapabilities) {
    let (mut session, mut turn) = make_session_and_context().await;
    set_feature(
        &mut turn,
        Feature::CurrentTimeReminder,
        /*enabled*/ false,
    );
    turn.multi_agent_version = MultiAgentVersion::Disabled;
    setup(&mut session, &mut turn);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = build_tool_router(
        &session,
        step_context.turn.as_ref(),
        &step_context.settings.model_info,
        &step_context.environments,
        &step_context.mcp,
        /*apps_enabled*/ false,
        &turn.extension_data,
        /*tool_suggest_candidates*/ None,
    )
    .expect("tool router should build");
    let snapshot = derive_waiting_capabilities(
        &router,
        &session.services.thread_extension_data,
        &HashSet::new(),
    );
    (router, snapshot)
}

#[tokio::test]
async fn deterministic_base_snapshot() {
    let (_router, snapshot) = snapshot_for(|_, _| {}).await;
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn snapshot_follows_sleep_switches() {
    // A hard sleep disable wins even with an active goal.
    let (_, hard_off) = snapshot_for(|session, turn| {
        set_feature(turn, Feature::SleepTool, /*enabled*/ false);
        session
            .services
            .thread_extension_data
            .insert(goal_activity());
    })
    .await;
    assert_eq!(
        hard_off,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );

    // Reminder on with sleep_tool=false (the ModelDriven default branch)
    // exposes no sleep tool.
    let (_, reminder_off) = snapshot_for(|_, turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
    })
    .await;
    assert_eq!(
        reminder_off,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );

    // Reminder sleep enabled exposes the tool.
    let (_, reminder_on) = snapshot_for(|_, turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
        update_config(turn, |config| {
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: true,
                ..CurrentTimeReminderConfig::default()
            });
        });
    })
    .await;
    assert_eq!(
        reminder_on,
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );

    // An active goal exposes sleep without clock catalog support or reminder.
    let (_, goal_active) = snapshot_for(|session, _| {
        session
            .services
            .thread_extension_data
            .insert(goal_activity());
    })
    .await;
    assert_eq!(
        goal_active,
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn snapshot_follows_catalog_clock_support() {
    // A remote catalog entry advertising clock support (replacing the bundled
    // entry, which lacks it) flows through the final router into the snapshot.
    let (_, with_clock) = snapshot_for(|_, turn| {
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info)
                .experimental_supported_tools
                .push("clock".to_string());
        });
    })
    .await;
    assert_eq!(
        with_clock,
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );

    let (_, bundled) = snapshot_for(|_, _| {}).await;
    assert_eq!(
        bundled,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn v1_wait_tool_reports_its_namespaced_name() {
    let (router, snapshot) = snapshot_for(|_, turn| {
        turn.multi_agent_version = MultiAgentVersion::V1;
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info).supports_search_tool = false;
        });
    })
    .await;
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "multi_agent_v1"),
        "test setup should expose V1 tools directly",
    );
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn v2_wait_tool_reports_its_configured_name() {
    let (router, snapshot) = snapshot_for(|_, turn| {
        turn.multi_agent_version = MultiAgentVersion::V2;
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info).multi_agent_version =
                Some(MultiAgentVersion::V2);
        });
    })
    .await;
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "collaboration"),
        "test setup should expose V2 tools directly",
    );
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: Some(ToolName::namespaced("collaboration", "wait_agent")),
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

fn dynamic_wait_agent(namespace: &str) -> DynamicToolSpec {
    DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: namespace.to_string(),
        description: format!("{namespace} tools"),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: "wait_agent".to_string(),
                description: "External callback.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            },
        )],
    })
}

#[tokio::test]
async fn foreign_dynamic_wait_agent_with_native_disabled_yields_none() {
    // Production-router A-F1: native disabled, dynamic `foreign.wait_agent`
    // directly advertised through `append_dynamic_tool_runtimes`. The bare
    // name must not satisfy `native_wait`.
    let (router, snapshot) = snapshot_for(|_, turn| {
        turn.multi_agent_version = MultiAgentVersion::Disabled;
        turn.dynamic_tools = vec![dynamic_wait_agent("foreign")];
    })
    .await;
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "foreign"),
        "test setup should advertise the foreign decoy",
    );
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn v2_wait_disabled_with_namespaced_decoy_yields_none() {
    // Production-router A-F1: V2 selected but `wait_agent_enabled=false`, so
    // no native wait runtime is registered. A dynamic namespaced `wait_agent`
    // in the would-be native namespace must not satisfy `native_wait`.
    let (router, snapshot) = snapshot_for(|_, turn| {
        turn.multi_agent_version = MultiAgentVersion::V2;
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info).multi_agent_version =
                Some(MultiAgentVersion::V2);
        });
        update_config(turn, |config| {
            config.multi_agent_v2.wait_agent_enabled = false;
        });
        turn.dynamic_tools = vec![dynamic_wait_agent("collaboration")];
    })
    .await;
    assert!(
        router.model_visible_specs().iter().any(|spec| {
            spec.name() == "collaboration"
                && match spec {
                    ToolSpec::Namespace(namespace) => namespace
                        .tools
                        .iter()
                        .any(|tool| matches!(tool, ResponsesApiNamespaceTool::Function(tool) if tool.name == "wait_agent")),
                    _ => false,
                }
        }),
        "test setup should advertise the decoy wait_agent",
    );
    assert_eq!(snapshot.native_wait, None);
}

#[tokio::test]
async fn custom_v2_namespace_reports_its_configured_name() {
    // Arbitrary configured V2 namespaces keep working: the snapshot carries
    // the runtime's actual identity, not a hardcoded namespace.
    let (router, snapshot) = snapshot_for(|_, turn| {
        turn.multi_agent_version = MultiAgentVersion::V2;
        update_turn_settings_for_test(turn, |settings| {
            Arc::make_mut(&mut settings.model_info).multi_agent_version =
                Some(MultiAgentVersion::V2);
        });
        update_config(turn, |config| {
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
    })
    .await;
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "agents"),
        "test setup should expose V2 tools under the custom namespace",
    );
    assert_eq!(
        snapshot.native_wait,
        Some(ToolName::namespaced("agents", "wait_agent")),
    );
}

#[tokio::test]
async fn discovered_v1_wait_updates_next_step_snapshot() {
    // A-F2 through production capture: V1 tools start deferred (search
    // enabled), the first step reports `None`, then a paired `tool_search`
    // call plus output loading the native wait tool makes the next step
    // report `Some`. The pair must survive prompt normalization (D-F1).
    let (session, mut turn) = make_session_and_context().await;
    set_feature(
        &mut turn,
        Feature::CurrentTimeReminder,
        /*enabled*/ false,
    );
    turn.multi_agent_version = MultiAgentVersion::V1;
    update_turn_settings_for_test(&mut turn, |settings| {
        Arc::make_mut(&mut settings.model_info).supports_search_tool = true;
    });
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let first = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("first step should capture");
    assert!(
        first
            .tool_router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name() == "tool_search"),
        "test setup should defer V1 tools behind tool_search",
    );
    assert_eq!(first.waiting_capabilities.native_wait, None);

    // Build the discovery payload through the real spec-to-loadable
    // conversion the handler uses, so the history shape stays faithful.
    let v1_spec = WaitAgentHandler::new(WaitAgentTimeoutOptions::default()).spec();
    let search_info = ToolSearchInfo::from_tool_spec(v1_spec, /*source_info*/ None)
        .expect("V1 wait spec should be searchable");
    let loadable = search_info.entry.to_loadable_spec();
    let value = serde_json::to_value(&loadable).expect("loadable spec should serialize");
    let discovery = tool_search_output(Some("search-wait-1"), "client", vec![value]);
    session
        .record_conversation_items(
            &turn,
            &first.settings.model_info,
            &[tool_search_call("search-wait-1"), discovery],
        )
        .await;
    let second = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("second step should capture");
    assert_eq!(
        second.waiting_capabilities.native_wait,
        Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
    );
    // The paired discovery must survive the same normalization the sampling
    // request uses; an orphan here would panic in debug or be dropped in
    // release (D-F1 diagnostic).
    let normalized = session
        .clone_history()
        .await
        .for_prompt(&second.settings.model_info.input_modalities);
    assert!(
        normalized.iter().any(|item| matches!(
            item,
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                tools,
                ..
            } if call_id == "search-wait-1" && !tools.is_empty()
        )),
        "paired discovery must survive prompt normalization",
    );
}

#[tokio::test]
async fn headless_exec_host_keeps_exec_but_disables_notification_flags() {
    for support in [None, Some(AsyncNotificationSupport::Unavailable)] {
        let (_, snapshot) = snapshot_for(|session, _| {
            if let Some(support) = support {
                session.services.thread_extension_data.insert(support);
            }
        })
        .await;
        assert_eq!(
            snapshot,
            WaitingCapabilities {
                sleep: false,
                stdin: true,
                native_wait: None,
                exec_notify: false,
                native_mail_wait: false,
            },
            "support: {support:?}",
        );
    }
}

#[tokio::test]
async fn available_host_with_policy_enables_notification_flags() {
    let (_, snapshot) = snapshot_for(|session, _| {
        session
            .services
            .thread_extension_data
            .insert(AsyncNotificationSupport::Available);
        install_wait_policy(&session.services.thread_extension_data);
    })
    .await;
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: true,
            native_mail_wait: true,
        },
    );
}

#[tokio::test]
async fn code_mode_only_hides_nested_waits_but_keeps_direct_sleep() {
    // Co-killer for the `stdin_counts_nested` mutant through a production
    // router: nested-only controls must not count.
    let (router, snapshot) = snapshot_for(|session, turn| {
        set_feature(turn, Feature::CodeModeOnly, /*enabled*/ true);
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
        update_config(turn, |config| {
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: true,
                ..CurrentTimeReminderConfig::default()
            });
        });
        session
            .services
            .thread_extension_data
            .insert(AsyncNotificationSupport::Available);
    })
    .await;
    assert_eq!(router.tool_mode(), ToolMode::CodeModeOnly);
    assert!(
        router
            .code_mode_tool_names()
            .values()
            .any(|name| name == &ToolName::plain("write_stdin")),
        "test setup should nest write_stdin in code mode",
    );
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: true,
            stdin: false,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn excluded_receipt_tool_disables_exec_notify() {
    let (_, snapshot) = snapshot_for(|session, turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
        update_config(turn, |config| {
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: true,
                ..CurrentTimeReminderConfig::default()
            });
        });
        session
            .services
            .thread_extension_data
            .insert(AsyncNotificationSupport::Available);
        session.tool_policy = Arc::new(ToolPolicy {
            allowed_tools: Some(vec![
                ToolName::plain("exec_command"),
                ToolName::plain("write_stdin"),
                ToolName::namespaced("clock", "sleep"),
            ]),
            ..ToolPolicy::default()
        });
    })
    .await;
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: true,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

#[tokio::test]
async fn excluded_exec_command_disables_exec_notify() {
    // Realistic co-killer for the `exec_notify_ignores_opt_in` mutant: no
    // advertised exec schema means no opt-in surface.
    let (_, snapshot) = snapshot_for(|session, _| {
        session
            .services
            .thread_extension_data
            .insert(AsyncNotificationSupport::Available);
        session.tool_policy = Arc::new(ToolPolicy {
            allowed_tools: Some(vec![
                ToolName::plain("write_stdin"),
                ToolName::plain("exec_notification"),
            ]),
            ..ToolPolicy::default()
        });
    })
    .await;
    assert_eq!(
        snapshot,
        WaitingCapabilities {
            sleep: false,
            stdin: true,
            native_wait: None,
            exec_notify: false,
            native_mail_wait: false,
        },
    );
}

struct RecordingContributor {
    captures: Arc<Mutex<Vec<(usize, WaitingCapabilities)>>>,
}

impl ContextContributor for RecordingContributor {
    fn contribute_turn_context<'a>(
        &'a self,
        input: TurnContextContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            self.captures
                .lock()
                .expect("captures lock should not be poisoned")
                .push((
                    std::ptr::from_ref(input.waiting_capabilities) as usize,
                    input.waiting_capabilities.clone(),
                ));
            Vec::new()
        })
    }
}

#[tokio::test]
async fn same_snapshot_instance_reaches_both_context_paths() {
    let (mut session, turn) = make_session_and_context().await;
    let captures = Arc::new(Mutex::new(Vec::new()));
    let mut builder = ExtensionRegistryBuilder::new();
    builder.prompt_contributor(Arc::new(RecordingContributor {
        captures: Arc::clone(&captures),
    }));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step_context = session
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("step context should capture");
    let world_state = session
        .build_world_state_for_step(&step_context)
        .await
        .expect("world state should build");
    let _initial = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await;
    let _diff = session
        .build_turn_context_contribution_items(&step_context)
        .await;
    let captures = captures.lock().expect("captures lock should read");
    assert_eq!(captures.len(), 2);
    assert_eq!(
        captures[0].0, captures[1].0,
        "both context paths must share one snapshot instance",
    );
    assert_eq!(captures[0].1, captures[1].1);
}

#[tokio::test]
async fn waiting_snapshot_does_not_persist_to_history_or_rollout() {
    // Production capture retains its step, so an unshared test step is used
    // here: only the snapshot field changes between the two builds below,
    // while both production context paths plus the persisted turn item run.
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let mut step_context = StepContext::for_test(Arc::clone(&turn));
    let world_state = session
        .build_world_state_for_step(&step_context)
        .await
        .expect("world state should build");
    let before_initial = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await;
    let before_diff = session
        .build_turn_context_contribution_items(&step_context)
        .await;
    let before_item = step_context.to_turn_context_item();
    Arc::get_mut(&mut step_context)
        .expect("test step context should be unshared")
        .waiting_capabilities = WaitingCapabilities {
        sleep: true,
        stdin: true,
        native_wait: Some(ToolName::plain("wait_agent")),
        exec_notify: true,
        native_mail_wait: true,
    };
    let after_initial = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await;
    let after_diff = session
        .build_turn_context_contribution_items(&step_context)
        .await;
    assert_eq!(before_initial, after_initial);
    assert_eq!(before_diff, after_diff);
    assert_eq!(before_item, step_context.to_turn_context_item());
}
