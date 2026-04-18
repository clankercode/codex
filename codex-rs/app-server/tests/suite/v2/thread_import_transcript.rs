use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use codex_app_server_protocol::InjectedMessage;
use codex_app_server_protocol::InjectedMessageRole;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadImportTranscriptParams;
use codex_app_server_protocol::ThreadImportTranscriptResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::RolloutRecorder;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_import_transcript_creates_thread_from_typed_messages() -> Result<()> {
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

    let import_req = mcp
        .send_thread_import_transcript_request(ThreadImportTranscriptParams {
            messages: vec![
                InjectedMessage {
                    role: InjectedMessageRole::Developer,
                    text: "Stay terse.".to_string(),
                },
                InjectedMessage {
                    role: InjectedMessageRole::Assistant,
                    text: "Previously computed context.".to_string(),
                },
                InjectedMessage {
                    role: InjectedMessageRole::User,
                    text: "Resume from this snapshot.".to_string(),
                },
            ],
            base_instructions: Some("Use the thin downstream contract.".to_string()),
            developer_instructions: Some("Keep responses machine-readable.".to_string()),
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let import_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(import_req)),
    )
    .await??;
    let ThreadImportTranscriptResponse { thread, .. } =
        to_response::<ThreadImportTranscriptResponse>(import_resp)?;

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
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "Resume from this snapshot.".to_string(),
            }],
            end_turn: None,
            phase: None,
        },
    ];

    let rollout_path = thread.path.as_ref().context("thread path missing")?;
    let history = RolloutRecorder::get_rollout_history(rollout_path).await?;
    let persisted_items = match history {
        InitialHistory::Forked(items) => items,
        InitialHistory::Resumed(resumed_history) => resumed_history.history,
        InitialHistory::New | InitialHistory::Cleared => Vec::new(),
    };
    for expected_item in &expected_items {
        assert!(
            persisted_items.iter().any(
                |item| matches!(item, RolloutItem::ResponseItem(response_item) if response_item == expected_item)
            ),
            "expected imported item to be persisted: {expected_item:?}"
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

    let request = response_mock.single_request();
    let body = request.body_json();
    assert_eq!(
        body["instructions"],
        json!("Use the thin downstream contract.")
    );

    let input = request.input();
    assert!(
        input.iter().any(|item| {
            item["role"] == "developer"
                && item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|content_item| {
                        content_item["text"] == "Keep responses machine-readable."
                    })
                })
        }),
        "expected developer instructions in request: {input:?}"
    );
    for expected_item in expected_items {
        let expected_value = serde_json::to_value(expected_item)?;
        assert!(
            input.contains(&expected_value),
            "expected imported item in request: {expected_value:?}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn thread_import_transcript_inherits_source_thread_and_releases_subscription() -> Result<()> {
    let server = responses::start_mock_server().await;
    let bodies = vec![
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-2", "Done again"),
            responses::ev_completed("resp-2"),
        ]),
    ];
    let response_mock = responses::mount_sse_sequence(&server, bodies).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some("/tmp".to_string()),
            base_instructions: Some("Source base instructions.".to_string()),
            developer_instructions: Some("Source developer instructions.".to_string()),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_req)),
    )
    .await??;
    let ThreadStartResponse {
        thread: source_thread,
        ..
    } = to_response::<ThreadStartResponse>(start_resp)?;

    let source_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: source_thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Source turn".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(source_turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let import_req = mcp
        .send_thread_import_transcript_request(ThreadImportTranscriptParams {
            source_thread_id: Some(source_thread.id.clone()),
            messages: vec![InjectedMessage {
                role: InjectedMessageRole::User,
                text: "Imported turn".to_string(),
            }],
            ..Default::default()
        })
        .await?;
    let import_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(import_req)),
    )
    .await??;
    let ThreadImportTranscriptResponse { thread, cwd, .. } =
        to_response::<ThreadImportTranscriptResponse>(import_resp)?;
    assert_eq!(cwd.as_path(), std::path::Path::new("/tmp"));
    assert_ne!(thread.id, source_thread.id);

    let loaded_req = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let loaded_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(loaded_req)),
    )
    .await??;
    let ThreadLoadedListResponse { data, .. } =
        to_response::<ThreadLoadedListResponse>(loaded_resp)?;
    assert!(
        data.contains(&thread.id),
        "expected imported thread to remain loaded: {data:?}"
    );

    let imported_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "After import".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(imported_turn_req)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let imported_turn_input = requests[1].input();
    let imported_value = serde_json::to_value(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Imported turn".to_string(),
        }],
        end_turn: None,
        phase: None,
    })?;
    assert!(
        imported_turn_input.contains(&imported_value),
        "expected imported transcript in next request: {imported_turn_input:?}"
    );

    let unsubscribe_req = mcp
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: source_thread.id.clone(),
        })
        .await?;
    let unsubscribe_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(unsubscribe_req)),
    )
    .await??;
    let unsubscribe = to_response::<ThreadUnsubscribeResponse>(unsubscribe_resp)?;
    assert_eq!(unsubscribe.status, ThreadUnsubscribeStatus::NotSubscribed);

    Ok(())
}

#[tokio::test]
async fn thread_import_transcript_rejects_empty_messages() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), "http://127.0.0.1:1")?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let import_req = mcp
        .send_thread_import_transcript_request(ThreadImportTranscriptParams {
            messages: Vec::new(),
            ..Default::default()
        })
        .await?;
    let import_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(import_req)),
    )
    .await??;
    assert!(
        import_err
            .error
            .message
            .contains("messages must not be empty"),
        "unexpected import error: {}",
        import_err.error.message
    );

    Ok(())
}

fn create_config_toml(codex_home: &std::path::Path, server_uri: &str) -> std::io::Result<()> {
    let config = format!(
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
    );
    std::fs::create_dir_all(codex_home)?;
    std::fs::write(codex_home.join("config.toml"), config)
}
