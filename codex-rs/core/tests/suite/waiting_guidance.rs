//! Goal waiting-guidance agreement between the developer fragment and request tools.
//!
//! Every test drives real `/responses` requests through `test_codex` with the
//! goal extension installed and asserts the exact request count. The World
//! State section rendered from each sampling step's `WaitingCapabilities`
//! must match what the same request advertises: the sleep/stdin sentences iff
//! those tools are present, the native wait sentence with the actual dotted
//! tool name, and the negative exec sentence whenever no completion wake can
//! be promised.
//!
//! World State runs on every sampling step and diffs against the persisted
//! snapshot: identical capabilities add no new item across steps or turn
//! boundaries, a capability change renders exactly one updated fragment on
//! the next request, and the goal stopping renders exactly one retraction
//! that supersedes the earlier fragment. History is append-only, so "matches
//! the same request's tools" means the most recent guidance or retraction in
//! that request's history. The V1 native wait tool is deferred behind tool
//! search by default, so fixtures without a discovery step agree on its
//! absence; only the discovery test carries the native sentence.
//!
//! Request counts stay exact because no automatic goal continuation can fire
//! after the final turn: tool-driven tests end with the goal completed, while
//! the service-set tests skip the live-manager rebind, so continuation is
//! structurally impossible there and the production turn-start reconcile
//! still publishes the goal marker for the first request.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::config::CurrentTimeReminderConfig;
use codex_extension_api::AsyncNotificationSupport;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolPolicy;
use codex_extension_api::TurnContextContributionInput;
use codex_extension_api::WaitingCapabilities;
use codex_features::Feature;
use codex_features::SleepToolMode;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalRuntimeHandle;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_goal_extension::install_with_backend;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_tools::ToolName;
use core_test_support::apps_test_server::configure_search_capable_model;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use wiremock::BodyPrintLimit;
use wiremock::MockServer;

// Distinctive substrings of the §9 sentences. Suite tests match these inside
// developer message texts only, so tool descriptions elsewhere in the request
// can never pollute the counts; byte-exactness is pinned in ext/goal/tests.
const EXEC_POSITIVE_MARKER: &str = "can acknowledge notify_on_exit subscriptions";
const EXEC_NEGATIVE_MARKER: &str = "cannot promise an exec completion wake";
const SLEEP_MARKER: &str = "clock.sleep is available as a direct tool";
const STDIN_MARKER: &str = "An empty write_stdin with a long yield_time_ms";
const NATIVE_WAIT_MARKER: &str = "The native agent wait tool is available";
const NATIVE_MAIL_MARKER: &str = "child mailbox results";
const NATIVE_V1_DOTTED_NAME: &str = "multi_agent_v1.wait_agent";
// Distinctive substring of the one-shot active→inactive retraction, which is
// not a §9 sentence and is therefore tracked separately.
const RETRACTION_MARKER: &str = "no longer applies; the goal is not active";
const ALL_SENTENCE_MARKERS: [&str; 6] = [
    EXEC_POSITIVE_MARKER,
    EXEC_NEGATIVE_MARKER,
    SLEEP_MARKER,
    STDIN_MARKER,
    NATIVE_WAIT_MARKER,
    NATIVE_MAIL_MARKER,
];

fn developer_occurrences(request: &ResponsesRequest, needle: &str) -> usize {
    request
        .message_input_texts("developer")
        .iter()
        .map(|text| text.matches(needle).count())
        .sum()
}

fn assert_no_fragment(request: &ResponsesRequest) {
    for marker in ALL_SENTENCE_MARKERS {
        assert_eq!(
            developer_occurrences(request, marker),
            0,
            "unexpected waiting guidance for marker {marker}",
        );
    }
    assert_eq!(
        developer_occurrences(request, RETRACTION_MARKER),
        0,
        "unexpected waiting retraction",
    );
}

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn exec_command_advertises_notify(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("name").and_then(Value::as_str) == Some("exec_command")
                    && tool
                        .get("parameters")
                        .and_then(|parameters| parameters.get("properties"))
                        .and_then(|properties| properties.get("notify_on_exit"))
                        .is_some()
            })
        })
}

fn tool_search_output_has_namespace_child(
    request: &ResponsesRequest,
    call_id: &str,
    namespace: &str,
    tool_name: &str,
) -> bool {
    namespace_child_tool(&request.tool_search_output(call_id), namespace, tool_name).is_some()
}

struct WaitingCapabilitiesRecorder {
    captures: Arc<Mutex<Vec<WaitingCapabilities>>>,
}

impl ContextContributor for WaitingCapabilitiesRecorder {
    fn contribute_turn_context<'a>(
        &'a self,
        input: TurnContextContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            self.captures
                .lock()
                .expect("captures lock should not be poisoned")
                .push(input.waiting_capabilities.clone());
            Vec::new()
        })
    }
}

fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    db: &Arc<codex_state::StateRuntime>,
    service: &Arc<GoalService>,
    manager: Weak<codex_core::ThreadManager>,
) {
    install_with_backend(
        registry,
        db.clone(),
        AnalyticsEventsClient::disabled(),
        /*metrics_client*/ None,
        manager,
        service.clone(),
        |c: &Config| GoalExtensionConfig {
            enabled: c.features.enabled(Feature::Goals),
            max_goal_token_budget: c.max_goal_token_budget,
        },
    );
}

async fn fixture(
    server: &MockServer,
    configure: impl FnOnce(&mut Config) + Send + 'static,
    live_manager: bool,
) -> Result<(
    TestCodex,
    Arc<codex_state::StateRuntime>,
    Arc<GoalService>,
    Arc<Mutex<Vec<WaitingCapabilities>>>,
)> {
    let home = Arc::new(TempDir::new()?);
    let config = core_test_support::load_default_config_for_test(&home).await;
    let db = codex_core::init_state_db(&config)
        .await
        .expect("state database");
    let service = Arc::new(GoalService::new());
    let captures = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    registry.prompt_contributor(Arc::new(WaitingCapabilitiesRecorder {
        captures: Arc::clone(&captures),
    }));
    install(&mut registry, &db, &service, Weak::new());
    let test = test_codex()
        .with_home(home)
        .with_extensions(Arc::new(registry.build()))
        .with_config(|c| {
            c.features
                .disable(Feature::CurrentTimeReminder)
                .expect("feature");
            c.sleep_tool_mode = SleepToolMode::ModelDriven;
        })
        .with_model_info_override("gpt-5.5", |m| {
            m.experimental_supported_tools.retain(|t| t != "clock");
        })
        .with_config(configure)
        .build_with_auto_env(server)
        .await?;
    if live_manager {
        // TestCodex creates its manager after accepting the extension
        // registry. Bind the installed goal runtime to that live manager via
        // the production thread-start hook before driving any tool or turn.
        test.codex
            .thread_extension_data()
            .remove::<GoalRuntimeHandle>();
        let mut bound = ExtensionRegistryBuilder::<Config>::new();
        install(
            &mut bound,
            &db,
            &service,
            Arc::downgrade(&test.thread_manager),
        );
        for contributor in bound.build().thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &test.config,
                    session_source: &SessionSource::Cli,
                    persistent_thread_state_available: true,
                    environments: &[],
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &ExtensionData::new(
                        test.session_configured.session_id.to_string(),
                    ),
                    thread_store: test.codex.thread_extension_data(),
                })
                .await;
        }
    }
    Ok((test, db, service, captures))
}

async fn set_active_goal(
    service: &GoalService,
    db: &Arc<codex_state::StateRuntime>,
    thread_id: ThreadId,
) -> Result<()> {
    let outcome = service
        .set_thread_goal(
            db,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("waiting guidance work"),
                status: None,
                token_budget: GoalTokenBudgetUpdate::Keep,
                max_goal_token_budget: None,
            },
        )
        .await
        .map_err(anyhow::Error::new)?;
    outcome.apply_runtime_effects(service).await;
    Ok(())
}

async fn tool_steps(server: &MockServer, steps: &[(&str, &str)]) -> ResponseMock {
    let scripts = steps
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            let id = format!("response-{index}");
            let mut events = vec![ev_response_created(&id)];
            if !name.is_empty() {
                events.push(ev_function_call(name, name, arguments));
            }
            events.push(ev_completed_with_tokens(&id, /*total_tokens*/ 10));
            sse(events)
        })
        .collect();
    mount_sse_sequence(server, scripts).await
}

async fn assistant_turn(server: &MockServer) -> ResponseMock {
    mount_sse_once(
        server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_post_create_carries_fragment_matching_tools() -> Result<()> {
    // AC5 (goal set mid-thread) + AC6 (post-create, sleep iff): turn 1 has no
    // goal so its request carries no fragment and no sleep tool; the goal is
    // then set mid-thread via the service and turn 2's request carries
    // exactly one fragment whose claims match the same request's tools. The
    // V1 native wait tool is deferred behind tool search and undiscovered in
    // this fixture, so sentence and tool agree on its absence. Dead manager,
    // so no continuation can follow the final turn even with the goal active.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = tool_steps(&server, &[("", ""), ("", "")]).await;
    let (test, db, service, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;
    test.submit_text_turn("hello without a goal").await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("work with a goal").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    // Exact split: the pre-goal request carries nothing; the post-create
    // request carries exactly one fragment.
    assert_no_fragment(&requests[0]);
    assert!(requests[0].tool_by_name("clock", "sleep").is_none());
    for request in &requests {
        assert_eq!(
            developer_occurrences(request, SLEEP_MARKER),
            request.tool_by_name("clock", "sleep").is_some() as usize,
        );
    }
    let request = &requests[1];
    assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
    assert_eq!(developer_occurrences(request, STDIN_MARKER), 1);
    assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
    assert_eq!(developer_occurrences(request, EXEC_POSITIVE_MARKER), 0);
    assert_eq!(developer_occurrences(request, NATIVE_MAIL_MARKER), 0);
    assert!(request.tool_by_name("clock", "sleep").is_some());
    assert!(
        request
            .tool_by_name("multi_agent_v1", "wait_agent")
            .is_none()
    );
    assert!(
        tool_names(&request.body_json()).contains(&"write_stdin".to_string()),
        "stdin sentence needs the write_stdin tool",
    );
    assert!(!exec_command_advertises_notify(&request.body_json()));
    assert!(!tool_names(&request.body_json()).contains(&"exec_notification".to_string()),);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_tool_created_goal_reacts_at_next_turn() -> Result<()> {
    // AC5/AC6 (tool-driven create): turn 1 creates the goal via the
    // create_goal tool and the post-create follow-up already carries the
    // fragment, because World State runs on every sampling step
    // (`record_step_world_state_if_changed` in `core/src/session/turn.rs`
    // records the diff before the request input is assembled). Turn 2 adds
    // nothing while the snapshot is unchanged; completing the goal mid-turn-2
    // renders exactly one retraction on the follow-up request, superseding
    // the stale fragment. The V1 native wait tool is deferred behind tool
    // search and undiscovered here, so sentence and tool agree on its
    // absence. Ends completed, so no continuation fires after the final turn.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let create = json!({"objective": "waiting guidance work"}).to_string();
    let stop = json!({"status": "complete"}).to_string();
    let mock = tool_steps(
        &server,
        &[
            ("create_goal", &create),
            ("", ""),
            ("update_goal", &stop),
            ("", ""),
        ],
    )
    .await;
    let (test, _, _, _) = fixture(&server, |_| {}, /*live_manager*/ true).await?;
    test.submit_text_turn("create a goal").await?;
    test.submit_text_turn("complete it").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_no_fragment(&requests[0]);
    assert!(requests[0].tool_by_name("clock", "sleep").is_none());
    // requests[1] is the post-create follow-up: the fragment arrives with
    // the same request that advertises clock.sleep (P2's next-request grant
    // via the `on_tool_finish` reconcile in `ext/goal/src/extension.rs`).
    // requests[2] adds nothing at the turn boundary: the count stays 1.
    for request in &requests[1..3] {
        assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
        assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
        assert_eq!(developer_occurrences(request, STDIN_MARKER), 1);
        assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
        assert_eq!(developer_occurrences(request, EXEC_POSITIVE_MARKER), 0);
        assert_eq!(developer_occurrences(request, NATIVE_MAIL_MARKER), 0);
        assert_eq!(developer_occurrences(request, RETRACTION_MARKER), 0);
        assert!(request.tool_by_name("clock", "sleep").is_some());
        assert!(
            request
                .tool_by_name("multi_agent_v1", "wait_agent")
                .is_none()
        );
        assert!(
            tool_names(&request.body_json()).contains(&"write_stdin".to_string()),
            "stdin sentence needs the write_stdin tool",
        );
        assert!(!exec_command_advertises_notify(&request.body_json()));
        assert!(!tool_names(&request.body_json()).contains(&"exec_notification".to_string()),);
    }
    // requests[3] is post-completion: the rebuilt router drops clock.sleep,
    // no new fragment is added, and exactly one retraction supersedes the
    // stale sleep sentence still retained in history.
    assert_eq!(developer_occurrences(&requests[3], RETRACTION_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[3], EXEC_NEGATIVE_MARKER), 1);
    assert!(requests[3].tool_by_name("clock", "sleep").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_code_mode_only_agrees_with_nested_tools() -> Result<()> {
    // AC6 (code-mode-only agreement): code mode nests direct tools inside the
    // exec cell but never hides clock.sleep: SleepHandler is DirectModelOnly
    // (`core/src/tools/handlers/sleep.rs::exposure`), which
    // `is_hidden_by_code_mode_only` (`core/src/tools/spec_plan.rs`) leaves
    // direct (P3-1's `code_mode_only_hides_nested_waits_but_keeps_direct_sleep`
    // pins sleep=true, stdin=false). With an active goal granting sleep under
    // ModelDriven, the request therefore keeps clock.sleep direct and the
    // fragment keeps the sleep sentence: agreement in the present direction.
    // write_stdin IS nested-only here, so tool and sentence agree absent.
    // Dead manager, so no continuation can follow the single turn.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = assistant_turn(&server).await;
    let (test, db, service, _) = fixture(
        &server,
        |config| {
            config.model = Some("test-gpt-5.1-codex".to_string());
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::CodeModeOnly);
            let _ = config.features.enable(Feature::ExecutedToolCallMetadata);
        },
        /*live_manager*/ false,
    )
    .await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("hello code mode").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
    for marker in [
        STDIN_MARKER,
        NATIVE_WAIT_MARKER,
        NATIVE_MAIL_MARKER,
        EXEC_POSITIVE_MARKER,
    ] {
        assert_eq!(
            developer_occurrences(request, marker),
            0,
            "unexpected waiting guidance for marker {marker}",
        );
    }
    assert!(request.tool_by_name("clock", "sleep").is_some());
    let names = tool_names(&request.body_json());
    assert!(!names.contains(&"write_stdin".to_string()));
    assert!(
        request
            .tool_by_name("multi_agent_v1", "wait_agent")
            .is_none()
    );
    assert!(request.inputs_of_type("tool_search_output").is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_remote_catalog_without_clock_omits_sleep_sentence() -> Result<()> {
    // AC6 (remote catalog): the remote entry without clock support is the
    // active catalog; with reminder sleep suppressed and no shell-backed
    // tools, the request carries no sleep tool and the fragment carries only
    // the negative exec sentence. Goal-driven sleep is suppressed here by the
    // reminder `sleep_tool: false` gate (a goal alone would grant it); P3-1's
    // matrix proves the snapshot itself follows the remote entry.
    skip_if_no_network!(Ok(()));

    // Use MockServer directly so the first /models request returns our
    // same-slug remote replacement.
    let server = MockServer::builder()
        .body_print_limit(BodyPrintLimit::Limited(80_000))
        .start()
        .await;
    let catalog = bundled_models_response()?;
    let mut remote = catalog
        .models
        .iter()
        .find(|model| model.slug == "gpt-6-astra")
        .expect("bundled gpt-6-astra should exist")
        .clone();
    assert!(
        remote
            .experimental_supported_tools
            .iter()
            .any(|tool| tool == "clock"),
        "bundled gpt-6-astra should support clock",
    );
    remote.visibility = ModelVisibility::List;
    remote.experimental_supported_tools = vec!["send_user_message_async".to_string()];
    let models_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: vec![remote],
        },
    )
    .await;
    let responses_mock = assistant_turn(&server).await;

    let home = Arc::new(TempDir::new()?);
    let config = core_test_support::load_default_config_for_test(&home).await;
    let db = codex_core::init_state_db(&config)
        .await
        .expect("state database");
    let service = Arc::new(GoalService::new());
    let mut registry = ExtensionRegistryBuilder::<Config>::new();
    install(&mut registry, &db, &service, Weak::new());
    let test = test_codex()
        .with_home(home)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("gpt-6-astra")
        .with_extensions(Arc::new(registry.build()))
        .with_config(|c| {
            c.features
                .enable(Feature::CurrentTimeReminder)
                .expect("feature");
            c.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: false,
                ..Default::default()
            });
            c.features.enable(Feature::SleepTool).expect("feature");
            c.sleep_tool_mode = SleepToolMode::ModelDriven;
            c.agents_enabled = false;
        })
        .build_with_auto_env(&server)
        .await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("hello remote catalog").await?;

    assert_eq!(models_mock.requests().len(), 1);
    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
    for marker in [
        SLEEP_MARKER,
        STDIN_MARKER,
        NATIVE_WAIT_MARKER,
        NATIVE_MAIL_MARKER,
        EXEC_POSITIVE_MARKER,
    ] {
        assert_eq!(
            developer_occurrences(request, marker),
            0,
            "unexpected waiting guidance for marker {marker}",
        );
    }
    assert!(request.tool_by_name("clock", "sleep").is_none());
    assert!(!tool_names(&request.body_json()).contains(&"write_stdin".to_string()),);
    let remote_models = test
        .thread_manager
        .get_models_manager()
        .get_remote_models()
        .await;
    let replaced = remote_models
        .iter()
        .find(|model| model.slug == "gpt-6-astra")
        .expect("replaced slug should be listed");
    assert_eq!(
        replaced.experimental_supported_tools,
        vec!["send_user_message_async".to_string()],
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_excluded_receipt_tool_gives_negative_exec_sentence() -> Result<()> {
    // AC6 (excluded receipt tool): on a capable host with the receipt tool
    // allow-listed out, exec_command still advertises the opt-in but the
    // fragment carries the negative exec sentence. A second thread supplies
    // the startup ToolPolicy; its runtime never binds a live manager and the
    // turn completes the goal, so no continuation can follow.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = tool_steps(
        &server,
        &[("update_goal", r#"{"status":"complete"}"#), ("", "")],
    )
    .await;
    let (test, db, service, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;

    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(ToolPolicy {
        allowed_tools: Some(vec![
            ToolName::plain("update_goal"),
            ToolName::plain("exec_command"),
            ToolName::plain("write_stdin"),
            ToolName::namespaced("clock", "sleep"),
        ]),
        ..ToolPolicy::default()
    });
    let second = test
        .thread_manager
        .start_thread(StartThreadOptions {
            thread_extension_init,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    second
        .thread
        .thread_extension_data()
        .insert(AsyncNotificationSupport::Available);
    set_active_goal(&service, &db, second.thread_id).await?;
    second
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "complete the waiting work".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&second.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
    assert_eq!(developer_occurrences(request, STDIN_MARKER), 1);
    assert_eq!(developer_occurrences(request, EXEC_POSITIVE_MARKER), 0);
    assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
    assert_eq!(developer_occurrences(request, NATIVE_MAIL_MARKER), 0);
    let names = tool_names(&request.body_json());
    assert!(names.contains(&"exec_command".to_string()));
    assert!(exec_command_advertises_notify(&request.body_json()));
    assert!(!names.contains(&"exec_notification".to_string()));
    assert!(!names.contains(&"wait_agent".to_string()));
    assert!(request.tool_by_name("clock", "sleep").is_some());
    // The follow-up runs after the completion removed the marker: no new
    // fragment, exactly one retraction, and the rebuilt router drops
    // clock.sleep.
    assert_eq!(developer_occurrences(&requests[1], EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[1], RETRACTION_MARKER), 1);
    assert!(requests[1].tool_by_name("clock", "sleep").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_headless_host_gives_negative_exec_sentence() -> Result<()> {
    // AC6 (headless host): without host wake support the exec schema omits
    // the opt-in and the fragment carries the negative exec sentence, while
    // sleep/stdin guidance still matches the advertised tools. The V1 native
    // wait tool is deferred behind tool search and undiscovered here, so
    // sentence and tool agree on its absence. Dead manager, so no
    // continuation can follow the single turn.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = assistant_turn(&server).await;
    let (test, db, service, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("hello headless host").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
    assert_eq!(developer_occurrences(request, STDIN_MARKER), 1);
    assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
    assert_eq!(developer_occurrences(request, EXEC_POSITIVE_MARKER), 0);
    assert_eq!(developer_occurrences(request, NATIVE_MAIL_MARKER), 0);
    assert!(!exec_command_advertises_notify(&request.body_json()));
    assert!(!tool_names(&request.body_json()).contains(&"exec_notification".to_string()),);
    assert!(request.tool_by_name("clock", "sleep").is_some());
    assert!(
        request
            .tool_by_name("multi_agent_v1", "wait_agent")
            .is_none()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_non_goal_thread_has_no_fragment() -> Result<()> {
    // AC3/AC6 (non-goal thread): the goal extension is installed but no goal
    // exists, so no waiting sentence reaches the request.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = assistant_turn(&server).await;
    let (test, _, _, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;
    test.submit_text_turn("hello without a goal").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_no_fragment(&requests[0]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_unchanged_capabilities_stable_across_sampling_steps() -> Result<()> {
    // AC5 (no churn): three sampling requests within one turn carry the
    // fragment exactly once each; identical capabilities add no new item.
    // Dead manager, so no continuation can follow the single turn.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut first = ev_completed("resp-1");
    first["response"]["end_turn"] = json!(false);
    let mut second = ev_completed("resp-2");
    second["response"]["end_turn"] = json!(false);
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "waiting"),
                first,
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "still waiting"),
                second,
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let (test, db, service, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("wait without working").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
        assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
        // The V1 native wait tool is deferred behind tool search and
        // undiscovered here, so sentence and tool agree on its absence.
        assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
        assert!(
            request
                .tool_by_name("multi_agent_v1", "wait_agent")
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_unchanged_capabilities_stable_across_turns() -> Result<()> {
    // AC5/F2 (no churn across turns): three consecutive turns with an active
    // goal and identical capabilities carry exactly one fragment in total;
    // the turn boundary adds no new item. Dead manager, so no continuation
    // can follow the final turn.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = tool_steps(&server, &[("", ""), ("", ""), ("", "")]).await;
    let (test, db, service, _) = fixture(&server, |_| {}, /*live_manager*/ false).await?;
    set_active_goal(&service, &db, test.session_configured.thread_id).await?;
    test.submit_text_turn("first waiting turn").await?;
    test.submit_text_turn("second waiting turn").await?;
    test.submit_text_turn("third waiting turn").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
        assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
        assert_eq!(developer_occurrences(request, STDIN_MARKER), 1);
        assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 0);
        assert_eq!(developer_occurrences(request, RETRACTION_MARKER), 0);
        assert!(request.tool_by_name("clock", "sleep").is_some());
        assert!(
            request
                .tool_by_name("multi_agent_v1", "wait_agent")
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_goal_stop_adds_no_new_fragment() -> Result<()> {
    // AC5/F1 (goal stops being active): completing the goal in turn 2 means
    // the follow-up request carries exactly one retraction, and turn 3 adds
    // nothing further while history stays intact. Ends with no goal, so no
    // continuation fires.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let create = json!({"objective": "waiting guidance work"}).to_string();
    let stop = json!({"status": "complete"}).to_string();
    let mock = tool_steps(
        &server,
        &[
            ("create_goal", &create),
            ("", ""),
            ("update_goal", &stop),
            ("", ""),
            ("", ""),
        ],
    )
    .await;
    let (test, _, _, _) = fixture(&server, |_| {}, /*live_manager*/ true).await?;
    test.submit_text_turn("create a goal").await?;
    test.submit_text_turn("complete it").await?;
    test.submit_text_turn("follow up after completion").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_no_fragment(&requests[0]);
    // The turn-1 follow-up and the turn-2 first request share the single
    // fragment; nothing is added at the turn boundary.
    for request in &requests[1..3] {
        assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 1);
        assert_eq!(developer_occurrences(request, SLEEP_MARKER), 1);
        assert_eq!(developer_occurrences(request, RETRACTION_MARKER), 0);
        assert!(request.tool_by_name("clock", "sleep").is_some());
    }
    // The first request after the stop carries the retraction exactly once;
    // the stale fragment is retained but superseded, never duplicated.
    assert_eq!(developer_occurrences(&requests[3], RETRACTION_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[3], EXEC_NEGATIVE_MARKER), 1);
    assert!(requests[3].tool_by_name("clock", "sleep").is_none());
    // Turn 3 adds nothing: same counts, while its context update is still
    // recorded.
    assert_eq!(
        developer_occurrences(&requests[4], RETRACTION_MARKER),
        1,
        "stopping the goal must add no further retraction",
    );
    assert_eq!(
        developer_occurrences(&requests[4], EXEC_NEGATIVE_MARKER),
        1,
        "stopping the goal must add no new fragment",
    );
    assert!(
        requests[4].input().len() > requests[3].input().len(),
        "turn 3 must still record its context update",
    );
    assert!(requests[4].tool_by_name("clock", "sleep").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_guidance_discovered_native_wait_updates_fragment() -> Result<()> {
    // AC5 (native wait discovered via tool search): the post-discovery
    // follow-up renders exactly one updated fragment carrying the native
    // sentence with the snapshot's actual dotted name, because World State
    // runs on every sampling step. Turn 2 adds nothing at the boundary, and
    // completing the goal renders exactly one retraction. Ends completed, so
    // no continuation fires.
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let search_call_id = "tool-search-wait-agent";
    let create = json!({"objective": "waiting guidance work"}).to_string();
    let stop = json!({"status": "complete"}).to_string();
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call("call-create", "create_goal", &create),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_tool_search_call(search_call_id, &json!({"query": "wait agent"})),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "found it"),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_function_call("call-update", "update_goal", &stop),
                ev_completed("resp-4"),
            ]),
            sse(vec![
                ev_response_created("resp-5"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-5"),
            ]),
        ],
    )
    .await;
    let (test, _, _, captures) = fixture(
        &server,
        configure_search_capable_model,
        /*live_manager*/ true,
    )
    .await?;
    test.submit_turn_with_approval_and_permission_profile(
        "Find the wait agent tool",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;
    test.submit_turn_with_approval_and_permission_profile(
        "Use what you found",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    let first_tools = tool_names(&requests[0].body_json());
    assert!(
        first_tools.iter().any(|name| name == "tool_search"),
        "first request should advertise tool_search: {first_tools:?}",
    );
    assert!(
        !first_tools.iter().any(|name| name == "wait_agent"),
        "v1 wait_agent should be hidden before search: {first_tools:?}",
    );
    assert_no_fragment(&requests[0]);
    // requests[1] is the post-create follow-up: the fragment arrives without
    // the native sentence (V1 still undiscovered).
    assert_eq!(developer_occurrences(&requests[1], EXEC_NEGATIVE_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[1], SLEEP_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[1], NATIVE_WAIT_MARKER), 0);
    assert_eq!(developer_occurrences(&requests[1], RETRACTION_MARKER), 0);
    assert!(requests[1].tool_by_name("clock", "sleep").is_some());
    assert!(
        tool_search_output_has_namespace_child(
            &requests[2],
            search_call_id,
            "multi_agent_v1",
            "wait_agent",
        ),
        "expected tool_search to return multi_agent_v1.wait_agent",
    );
    // requests[2] is the post-discovery follow-up: exactly one updated
    // fragment is added, so the shared sentences count 2 while the native
    // sentence and dotted name appear once. Turn 2 (requests[3]) adds
    // nothing at the boundary.
    for request in &requests[2..4] {
        assert_eq!(developer_occurrences(request, EXEC_NEGATIVE_MARKER), 2);
        assert_eq!(developer_occurrences(request, SLEEP_MARKER), 2);
        assert_eq!(developer_occurrences(request, NATIVE_WAIT_MARKER), 1);
        assert_eq!(developer_occurrences(request, NATIVE_V1_DOTTED_NAME), 1);
        assert_eq!(developer_occurrences(request, RETRACTION_MARKER), 0);
    }
    // requests[4] is post-completion: exactly one retraction, no new
    // fragment.
    assert_eq!(developer_occurrences(&requests[4], RETRACTION_MARKER), 1);
    assert_eq!(developer_occurrences(&requests[4], EXEC_NEGATIVE_MARKER), 2);
    assert_eq!(developer_occurrences(&requests[4], NATIVE_WAIT_MARKER), 1);
    assert!(
        !tool_names(&requests[3].body_json())
            .iter()
            .any(|name| name == "wait_agent"),
        "deferred wait stays out of the tools list after discovery",
    );
    let captures = captures.lock().expect("captures lock should read");
    assert!(
        captures.len() >= 2,
        "expected pre- and post-discovery samples, got {}",
        captures.len(),
    );
    assert_eq!(
        captures.first().expect("pre-discovery sample").native_wait,
        None,
        "native_wait should be None before discovery",
    );
    assert_eq!(
        captures.last().expect("post-discovery sample").native_wait,
        Some(ToolName::namespaced("multi_agent_v1", "wait_agent")),
        "native_wait should carry the discovered native name after search",
    );
    Ok(())
}
