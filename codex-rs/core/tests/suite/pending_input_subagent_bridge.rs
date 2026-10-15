//! Native completion delivery through the real spawn, child input and parent sampling paths.
use super::*;
use codex_features::SleepToolMode;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;

fn contains(request: &Request, text: &str) -> bool {
    String::from_utf8_lossy(&request.body).contains(text)
}

fn is_thread(request: &Request, id: codex_protocol::ThreadId) -> bool {
    request.body_json::<Value>().expect("model request JSON")["client_metadata"]["thread_id"]
        == id.to_string()
}

async fn request_count(server: &wiremock::MockServer, thread: codex_protocol::ThreadId) -> usize {
    let requests = server.received_requests().await.expect("captured requests");
    requests.iter().filter(|r| is_thread(r, thread)).count()
}

fn transcript(items: &[RolloutItem]) -> Vec<&ResponseItem> {
    items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(&envelope.item),
            _ => None,
        })
        .collect()
}

async fn bridge_fixture(
    version: MultiAgentVersion,
    workers: usize,
) -> (TestCodex, wiremock::MockServer) {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("fixture");
            match version {
                MultiAgentVersion::Disabled => unreachable!("native version required"),
                MultiAgentVersion::V1 => config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("fixture"),
                MultiAgentVersion::V2 => config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("fixture"),
            }
            config
                .features
                .disable(Feature::CurrentTimeReminder)
                .expect("fixture");
            config
                .features
                .disable(Feature::EnableRequestCompression)
                .expect("plain JSON request matching");
            config.sleep_tool_mode = SleepToolMode::AlwaysOn;
        })
        .build_with_auto_env(&server)
        .await
        .expect("fixture");
    let parent_id = test.session_configured.thread_id;
    let mut events = vec![ev_response_created("spawn")];
    for index in 0..workers {
        let call_id = format!("spawn-worker-{index}");
        events.push(match version {
            MultiAgentVersion::Disabled => unreachable!("native version required"),
            MultiAgentVersion::V1 => ev_function_call_with_namespace(
                &call_id, "multi_agent_v1", "spawn_agent", r#"{"message":"bridge-child","fork_context":false}"#,
            ),
            MultiAgentVersion::V2 => ev_function_call_with_namespace(
                &call_id, "collaboration", "spawn_agent",
                &json!({"message":"bridge-child", "task_name":format!("worker{index}"), "fork_turns":"none"}).to_string(),
            ),
        });
    }
    events.push(ev_completed("spawn"));
    responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id) && !contains(r, "spawn-worker"),
        responses::sse(events),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id) && contains(r, "spawn-worker"),
        responses::sse_completed("parent-idle"),
    )
    .await;
    for _ in 0..workers {
        responses::mount_sse_once_match(
            &server,
            move |r: &Request| !is_thread(r, parent_id),
            responses::sse(vec![
                ev_response_created("child-wait"),
                ev_function_call_with_namespace(
                    "child-sleep",
                    "clock",
                    "sleep",
                    r#"{"duration_ms":60000}"#,
                ),
                ev_completed("child-wait"),
            ]),
        )
        .await;
    }
    submit_user_input(&test.codex, "bridge-parent").await;
    wait_for_successful_turn(&test.codex).await;
    (test, server)
}

async fn sleeping_children(
    test: &TestCodex,
    count: usize,
) -> Vec<(codex_protocol::ThreadId, Arc<CodexThread>)> {
    let ids = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let children = test
                .codex
                .inspect_directly_owned_native_children()
                .await
                .expect("fixture");
            if children.len() == count
                && children
                    .iter()
                    .all(|child| matches!(child.status(), Some(AgentStatus::Running)))
            {
                break children
                    .into_iter()
                    .map(|child| child.thread_id())
                    .collect::<Vec<_>>();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spawn must create running children");
    let mut children = Vec::new();
    for id in ids {
        let child = test.thread_manager.get_thread(id).await.expect("fixture");
        wait_for_sleep_item_started(&child, "child-sleep", 60000).await;
        children.push((id, child));
    }
    children
}

fn notification_items(body: &Value, version: MultiAgentVersion) -> Vec<Value> {
    body["input"]
        .as_array()
        .expect("fixture")
        .iter()
        .filter(|item| match version {
            MultiAgentVersion::Disabled => unreachable!("native version required"),
            MultiAgentVersion::V1 => item.to_string().contains("<subagent_notification>"),
            MultiAgentVersion::V2 => item["type"] == "agent_message",
        })
        .cloned()
        .collect()
}

#[test_case(MultiAgentVersion::V1, false; "v1_without_wait")]
#[test_case(MultiAgentVersion::V1, true; "v1_registered_wait")]
#[test_case(MultiAgentVersion::V2, false; "v2_without_wait")]
#[test_case(MultiAgentVersion::V2, true; "v2_registered_wait")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_completion_mailbox_history_and_wake(version: MultiAgentVersion, registered: bool) {
    let (test, server) = bridge_fixture(version, /*workers*/ 1).await;
    let (child_id, child) = sleeping_children(&test, /*count*/ 1).await.remove(0);
    let before = test
        .codex
        .load_history(/*include_archived*/ true)
        .await
        .expect("fixture");
    let initial = json!({"input": transcript(&before.items)});
    assert!(notification_items(&initial, version).is_empty());
    if registered {
        test.codex.thread_extension_data().insert(SleepItem {
            id: codex_extension_items::sleep::goal_wait_sleep_id(1),
            duration_ms: 0,
        });
    }
    let parent_id = test.session_configured.thread_id;
    let child_done = responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, child_id),
        responses::sse(vec![
            ev_response_created("child-done"),
            responses::ev_assistant_message("child-answer", "bridge-result"),
            ev_completed("child-done"),
        ]),
    )
    .await;
    // start_task queues V2 mail behind fresh user input; it is sampled on
    // the following step, as before the V1 bridge. This is an explicit turn,
    // not permission for an idle wake.
    let deferred_mail = version == MultiAgentVersion::V2 && !registered;
    let before_mail = responses::mount_sse_once_match(
        &server,
        move |r: &Request| {
            deferred_mail
                && is_thread(r, parent_id)
                && contains(r, "read queued result")
                && !contains(r, "bridge-result")
        },
        responses::sse_completed("user-input-before-mail"),
    )
    .await;
    let parent_wake = responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id) && contains(r, "bridge-result"),
        responses::sse_completed("wake"),
    )
    .await;
    submit_user_input(&child, "release-child").await;
    wait_for_successful_turn(&child).await;
    assert_eq!(child_done.requests().len(), 1);
    if registered {
        wait_for_successful_turn(&test.codex).await;
    } else {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            parent_wake.requests().len(),
            0,
            "queue-only mail cannot wake an ordinary parent"
        );
        let queued = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let history = test
                    .codex
                    .load_history(/*include_archived*/ true)
                    .await
                    .expect("history");
                if version == MultiAgentVersion::V2
                    || transcript(&history.items).len() > transcript(&before.items).len()
                {
                    break history;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle V1 runtime data must remain visible in history");
        let prior = transcript(&before.items);
        let current = transcript(&queued.items);
        assert_eq!(&current[..prior.len()], prior.as_slice());
        assert_eq!(
            current.len() - prior.len(),
            usize::from(version == MultiAgentVersion::V1)
        );
        if version == MultiAgentVersion::V1 {
            assert!(
                serde_json::to_string(current.last().expect("idle notification"))
                    .expect("JSON")
                    .contains("<subagent_notification>")
            );
        }
        assert_eq!(
            parent_wake.requests().len(),
            0,
            "history delivery must not sample"
        );
        submit_user_input(&test.codex, "read queued result").await;
        wait_for_successful_turn(&test.codex).await;
    }
    assert_eq!(before_mail.requests().len(), usize::from(deferred_mail));
    if deferred_mail {
        assert!(notification_items(&before_mail.single_request().body_json(), version).is_empty());
    }
    let request = parent_wake.single_request().body_json();
    let notifications = notification_items(&request, version);
    assert_eq!(notifications.len(), 1, "one delivered completion");
    assert!(notifications[0].to_string().contains("bridge-result"));
    if version == MultiAgentVersion::V1 {
        assert_eq!(notifications[0]["role"], "user");
        let text = notifications[0]["content"][0]["text"]
            .as_str()
            .expect("fixture");
        assert!(text.contains(&child_id.to_string()));
        assert!(!text.contains("/root"), "V1 must not fabricate a path");
        let after = test
            .codex
            .load_history(/*include_archived*/ true)
            .await
            .expect("fixture");
        assert_eq!(
            notification_items(&json!({"input": transcript(&after.items)}), version).len(),
            1
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(parent_wake.requests().len(), 1);
    assert_eq!(
        request_count(&server, parent_id).await,
        3 + usize::from(version == MultiAgentVersion::V2 && !registered)
    );
    child.submit(Op::Shutdown {}).await.expect("fixture");
}

#[test_case(MultiAgentVersion::V1, true; "v1_registered")]
#[test_case(MultiAgentVersion::V2, true; "v2_registered")]
#[test_case(MultiAgentVersion::V1, false; "v1_unregistered")]
#[test_case(MultiAgentVersion::V2, false; "v2_unregistered")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_interrupted_wait_wakes_once(version: MultiAgentVersion, registered: bool) {
    let (test, server) = bridge_fixture(version, /*workers*/ 1).await;
    let (_, child) = sleeping_children(&test, /*count*/ 1).await.remove(0);
    if registered {
        test.codex.thread_extension_data().insert(SleepItem {
            id: codex_extension_items::sleep::goal_wait_sleep_id(2),
            duration_ms: 0,
        });
    }
    let parent_id = test.session_configured.thread_id;
    let wake = responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id),
        responses::sse_completed("interrupted-wake"),
    )
    .await;
    child.submit(Op::Interrupt).await.expect("fixture");
    if !registered {
        wait_for_event(&child, |event| matches!(event, EventMsg::TurnAborted(_))).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(wake.requests().len(), 0);
        submit_user_input(&test.codex, "continue without a goal wait").await;
    }
    wait_for_successful_turn(&test.codex).await;
    let notifications = notification_items(&wake.single_request().body_json(), version);
    assert_eq!(notifications.len(), usize::from(registered));
    if registered {
        assert!(
            notifications[0]
                .to_string()
                .to_lowercase()
                .contains("interrupt")
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(wake.requests().len(), 1);
    assert_eq!(request_count(&server, parent_id).await, 3);
    test.codex.thread_extension_data().remove::<SleepItem>();
    child.submit(Op::Shutdown {}).await.expect("fixture");
}

#[test_case(MultiAgentVersion::V1; "v1")]
#[test_case(MultiAgentVersion::V2; "v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_second_child_finishes_mid_turn_once(version: MultiAgentVersion) {
    let (test, server) = bridge_fixture(version, /*workers*/ 2).await;
    let children = sleeping_children(&test, /*count*/ 2).await;
    test.codex.thread_extension_data().insert(SleepItem {
        id: codex_extension_items::sleep::goal_wait_sleep_id(3),
        duration_ms: 0,
    });
    let parent_id = test.session_configured.thread_id;
    for (index, (child_id, _)) in children.iter().enumerate() {
        let child_id = *child_id;
        responses::mount_sse_once_match(
            &server,
            move |r: &Request| is_thread(r, child_id),
            responses::sse(vec![
                ev_response_created("child-done"),
                responses::ev_assistant_message("answer", &format!("result-{index}")),
                ev_completed("child-done"),
            ]),
        )
        .await;
    }
    let first = responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id) && !contains(r, "parent-sleep"),
        responses::sse(vec![
            ev_response_created("parent-working"),
            ev_function_call_with_namespace(
                "parent-sleep",
                "clock",
                "sleep",
                r#"{"duration_ms":60000}"#,
            ),
            ev_completed("parent-working"),
        ]),
    )
    .await;
    let second = responses::mount_sse_once_match(
        &server,
        move |r: &Request| is_thread(r, parent_id) && contains(r, "parent-sleep"),
        responses::sse_completed("both-delivered"),
    )
    .await;
    submit_user_input(&children[0].1, "release-0").await;
    wait_for_successful_turn(&children[0].1).await;
    wait_for_sleep_item_started(&test.codex, "parent-sleep", 60000).await;
    assert_eq!(
        notification_items(&first.single_request().body_json(), version).len(),
        1
    );
    submit_user_input(&children[1].1, "release-1").await;
    wait_for_successful_turn(&children[1].1).await;
    wait_for_successful_turn(&test.codex).await;
    let notifications = notification_items(&second.single_request().body_json(), version);
    assert_eq!(notifications.len(), 2);
    for index in 0..2 {
        assert_eq!(
            notifications
                .iter()
                .filter(|item| item.to_string().contains(&format!("result-{index}")))
                .count(),
            1
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(first.requests().len() + second.requests().len(), 2);
    assert_eq!(request_count(&server, parent_id).await, 4);
    test.codex.thread_extension_data().remove::<SleepItem>();
    for (_, child) in children {
        child.submit(Op::Shutdown {}).await.expect("fixture");
    }
}
