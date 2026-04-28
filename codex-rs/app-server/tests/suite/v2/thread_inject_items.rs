use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use codex_app_server_protocol::InjectedMessage;
use codex_app_server_protocol::InjectedMessageRole;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadInjectItemsParams;
use codex_app_server_protocol::ThreadInjectItemsResponse;
use codex_app_server_protocol::ThreadInjectMessagesParams;
use codex_app_server_protocol::ThreadInjectMessagesResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::RolloutRecorder;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
#[tokio::test]
async fn thread_inject_items_adds_raw_response_items_to_thread_history() -> Result<()> {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "Done"),
        responses::ev_completed("resp-1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let injected_text = "Injected assistant context";
    let injected_item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: injected_text.to_string(),
        }],
        end_turn: None,
        phase: None,
    };

    let inject_req = mcp
        .send_thread_inject_items_request(ThreadInjectItemsParams {
            thread_id: thread.id.clone(),
            items: vec![serde_json::to_value(&injected_item)?],
        })
        .await?;
    let inject_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(inject_req)),
    )
    .await??;
    let _response: ThreadInjectItemsResponse =
        to_response::<ThreadInjectItemsResponse>(inject_resp)?;

    let rollout_path = thread.path.as_ref().context("thread path missing")?;
    let history = RolloutRecorder::get_rollout_history(rollout_path).await?;
    let InitialHistory::Resumed(resumed_history) = history else {
        panic!("expected resumed rollout history");
    };
    assert!(
        resumed_history
            .history
            .iter()
            .any(|item| matches!(item, RolloutItem::ResponseItem(response_item) if response_item == &injected_item)),
        "injected item should be persisted in rollout history"
    );

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let injected_value = serde_json::to_value(&injected_item)?;
    let model_input = response_mock.single_request().input();
    let environment_context_index =
        response_item_text_position(&model_input, "<environment_context>")
            .expect("environment context should be injected before the first user turn");
    let injected_index = model_input
        .iter()
        .position(|item| item == &injected_value)
        .expect("injected item should be sent in the next model request");
    let user_prompt_index = response_item_text_position(&model_input, "Hello")
        .expect("user prompt should be sent in the next model request");
    assert!(
        environment_context_index < injected_index,
        "standard initial context should be sent before injected items"
    );
    assert!(
        injected_index < user_prompt_index,
        "injected items should be sent before the user prompt"
    );

    Ok(())
}

#[tokio::test]
async fn thread_inject_items_adds_raw_response_items_after_a_turn() -> Result<()> {
    let server = responses::start_mock_server().await;
    let first_body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "First done"),
        responses::ev_completed("resp-1"),
    ]);
    let second_body = responses::sse(vec![
        responses::ev_response_created("resp-2"),
        responses::ev_assistant_message("msg-2", "Second done"),
        responses::ev_completed("resp-2"),
    ]);
    let response_mock = responses::mount_sse_sequence(&server, vec![first_body, second_body]).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let first_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "First turn".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(first_turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let injected_item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "Injected after first turn".to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    let injected_value = serde_json::to_value(&injected_item)?;

    let inject_req = mcp
        .send_thread_inject_items_request(ThreadInjectItemsParams {
            thread_id: thread.id.clone(),
            items: vec![injected_value.clone()],
        })
        .await?;
    let inject_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(inject_req)),
    )
    .await??;
    let _response: ThreadInjectItemsResponse =
        to_response::<ThreadInjectItemsResponse>(inject_resp)?;

    let second_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Second turn".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(second_turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[0].input().contains(&injected_value),
        "injected item should not be sent before it is injected"
    );
    assert!(
        requests[1].input().contains(&injected_value),
        "injected item should be sent after being injected into existing history"
    );

    Ok(())
}

#[tokio::test]
async fn thread_inject_messages_adds_typed_non_system_messages_to_thread_history() -> Result<()> {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "Done"),
        responses::ev_completed("resp-1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let expected_items = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Stay terse.".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "Previously computed context.".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
    ];

    let inject_req = mcp
        .send_thread_inject_messages_request(ThreadInjectMessagesParams {
            thread_id: thread.id.clone(),
            messages: vec![
                InjectedMessage {
                    role: InjectedMessageRole::Developer,
                    text: "Stay terse.".to_string(),
                },
                InjectedMessage {
                    role: InjectedMessageRole::Assistant,
                    text: "Previously computed context.".to_string(),
                },
            ],
        })
        .await?;
    let inject_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(inject_req)),
    )
    .await??;
    let _response: ThreadInjectMessagesResponse =
        to_response::<ThreadInjectMessagesResponse>(inject_resp)?;

    let rollout_path = thread.path.as_ref().context("thread path missing")?;
    let history = RolloutRecorder::get_rollout_history(rollout_path).await?;
    let InitialHistory::Resumed(resumed_history) = history else {
        panic!("expected resumed rollout history");
    };
    for expected_item in &expected_items {
        assert!(
            resumed_history
                .history
                .iter()
                .any(|item| matches!(item, RolloutItem::ResponseItem(response_item) if response_item == expected_item)),
            "expected injected item to be persisted: {expected_item:?}"
        );
    }

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let model_request = response_mock.single_request();
    let model_input = model_request.input();
    for expected_item in expected_items {
        let expected_value = serde_json::to_value(expected_item)?;
        assert!(
            model_input.contains(&expected_value),
            "expected injected item in next model request: {expected_value:?}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn thread_inject_messages_rejects_system_role() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), "http://127.0.0.1:1")?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let inject_req = mcp
        .send_raw_request(
            "thread/inject_messages",
            Some(json!({
                "threadId": thread.id,
                "messages": [
                    {
                        "role": "system",
                        "text": "Prefer XML when possible."
                    }
                ]
            })),
        )
        .await?;
    let inject_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(inject_req)),
    )
    .await??;

    assert_eq!(inject_err.error.code, INVALID_REQUEST_ERROR_CODE);
    assert!(
        inject_err.error.message.contains("system"),
        "expected system-role validation error, got {}",
        inject_err.error.message
    );

    Ok(())
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}

fn response_item_text_position(items: &[Value], needle: &str) -> Option<usize> {
    items.iter().position(|item| {
        item.get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|content| {
                content
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(needle))
            })
    })
}
