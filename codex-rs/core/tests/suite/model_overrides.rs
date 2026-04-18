use anyhow::Result;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;

const CONFIG_TOML: &str = "config.toml";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_turn_context_does_not_persist_when_config_exists() {
    let server = start_mock_server().await;
    let initial_contents = "model = \"gpt-4o\"\n";
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            let config_path = home.join(CONFIG_TOML);
            std::fs::write(config_path, initial_contents).expect("seed config.toml");
        })
        .with_config(|config| {
            config.model = Some("gpt-4o".to_string());
        });
    let test = builder.build(&server).await.expect("create conversation");
    let codex = test.codex.clone();
    let config_path = test.home.path().join(CONFIG_TOML);

    codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: None,
            windows_sandbox_level: None,
            model: Some("o3".to_string()),
            effort: Some(Some(ReasoningEffort::High)),
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await
        .expect("submit override");

    codex.submit(Op::Shutdown).await.expect("request shutdown");
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let contents = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config.toml after override");
    assert_eq!(contents, initial_contents);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_turn_context_does_not_create_config_file() {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await.expect("create conversation");
    let codex = test.codex.clone();
    let config_path = test.home.path().join(CONFIG_TOML);
    assert!(
        !config_path.exists(),
        "test setup should start without config"
    );

    codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: None,
            windows_sandbox_level: None,
            model: Some("o3".to_string()),
            effort: Some(Some(ReasoningEffort::Medium)),
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await
        .expect("submit override");

    codex.submit(Op::Shutdown).await.expect("request shutdown");
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    assert!(
        !config_path.exists(),
        "override should not create config.toml"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_turn_context_updates_reasoning_effort_during_active_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "call-1";
    let args = serde_json::json!({
        "command": "sleep 2; echo model-shell",
        "timeout_ms": 10_000,
    });
    let request_mock = mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
                ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex().with_model("gpt-5.1").build(&server).await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "run shell command".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: "gpt-5.1".to_string(),
            effort: None,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
            final_output_json_schema: None,
        })
        .await?;

    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(begin) if begin.source == ExecCommandSource::Agent => {
            Some(begin.clone())
        }
        _ => None,
    })
    .await;

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: None,
            effort: Some(Some(ReasoningEffort::High)),
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = request_mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected initial and follow-up model requests"
    );
    let second_request_body = requests[1].body_json();
    assert_eq!(
        second_request_body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(|effort| effort.as_str()),
        Some("high")
    );

    Ok(())
}
