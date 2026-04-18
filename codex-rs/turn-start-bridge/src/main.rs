use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::ValueEnum;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::CodexTurnClient;
use codex_app_server_client::CodexTurnSession;
use codex_app_server_client::StdioAppServerClient;
use codex_app_server_client::StdioAppServerConnectArgs;
use codex_app_server_client::ThreadSessionRequest;
use codex_app_server_client::TurnRequest;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnSteerResponse;
use codex_turn_start_bridge_core::BridgeController;
use codex_turn_start_bridge_core::CompletionSignal;
use codex_turn_start_bridge_core::ControllerEvent;
use codex_turn_start_bridge_core::ParsedMessage;
use codex_turn_start_bridge_core::ParsedXmlInput;
use codex_turn_start_bridge_core::QuiescencePolicy;
use codex_turn_start_bridge_core::ReadSignal;
use codex_turn_start_bridge_core::ReleaseAction;
use codex_turn_start_bridge_core::ReleaseDecision;
use codex_turn_start_bridge_core::XmlInputParser;
use codex_turn_start_bridge_core::parse_prefixed_message;
use codex_utils_cli::CliConfigOverrides;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[derive(Parser, Debug)]
#[command(
    author = "Codex",
    version,
    about = "Bridge chunked stdin to app-server turns"
)]
struct Cli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,

    #[arg(long)]
    thread_id: Option<String>,

    #[arg(long, default_value = "on-request")]
    approval_policy: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    cwd: Option<PathBuf>,

    #[arg(long)]
    system_prompt: Option<String>,

    #[arg(long, value_enum, default_value_t = StdinFormat::Raw)]
    stdin_format: StdinFormat,

    #[arg(long, default_value_t = 25)]
    chunk_quiescence_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum StdinFormat {
    #[default]
    Raw,
    Xml,
}

#[derive(Clone, Debug, Default)]
struct XmlReadPrelude {
    system_prompt: Option<String>,
    pending_messages: Vec<ParsedMessage>,
    stdin_closed: bool,
    parser: XmlInputParser,
}

#[ctor::ctor]
fn pre_main() {
    codex_process_hardening::pre_main_hardening();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let approval_policy = parse_approval_policy(&cli.approval_policy)?;
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<ParsedMessage>();
    let (input_error_tx, mut input_error_rx) = mpsc::unbounded_channel::<String>();
    let mut stdin_closed = false;
    let mut initial_messages = Vec::new();
    let xml_system_prompt = match cli.stdin_format {
        StdinFormat::Raw => None,
        StdinFormat::Xml => {
            let mut stdin = tokio::io::stdin();
            let prelude = read_xml_prelude_from_reader(&mut stdin).await?;
            stdin_closed = prelude.stdin_closed;
            initial_messages = prelude.pending_messages;
            if !stdin_closed {
                let chunk_tx = chunk_tx.clone();
                let input_error_tx = input_error_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        read_xml_chunks_from_reader(stdin, prelude.parser, chunk_tx).await
                    {
                        let _ = input_error_tx.send(err.to_string());
                    }
                });
            }
            prelude.system_prompt
        }
    };

    let mut client = AppServerClient::Stdio(
        StdioAppServerClient::connect(StdioAppServerConnectArgs {
            codex_bin: cli.codex_bin.clone(),
            config_overrides: cli.config_overrides.raw_overrides.clone(),
            client_name: "codex-turn-start-bridge".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            experimental_api: true,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 64,
        })
        .await
        .with_context(|| format!("connect stdio app-server via {}", cli.codex_bin.display()))?,
    );

    let turn_client = CodexTurnClient::new(client.request_handle());
    let thread_request = thread_session_request_from_cli(&cli, approval_policy, xml_system_prompt)
        .context("build thread session request")?;
    let thread_start = turn_client
        .start_or_resume_thread(
            if cli.thread_id.is_some() {
                "thread-resume"
            } else {
                "thread-start"
            },
            thread_request,
        )
        .await
        .context("start or resume thread")?;
    let mut session = thread_start.session;
    let mut controller = BridgeController::new(thread_start.thread.id.clone());
    restore_controller_from_thread(&mut controller, &thread_start.thread);

    if matches!(cli.stdin_format, StdinFormat::Raw) {
        let chunk_policy = QuiescencePolicy {
            chunk_quiescence_ms: cli.chunk_quiescence_ms,
        };
        let chunk_tx = chunk_tx.clone();
        let input_error_tx = input_error_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = read_stdin_chunks(chunk_policy, chunk_tx).await {
                let _ = input_error_tx.send(err.to_string());
            }
        });
    }
    drop(chunk_tx);
    drop(input_error_tx);

    let mut next_request_id: i64 = 1;
    let mut pending_start_request_id: Option<String> = None;
    let mut pending_request: Option<oneshot::Receiver<PendingRequestOutcome>> = None;
    let mut pending_decisions = VecDeque::new();
    for message in initial_messages {
        if let Some(decision) =
            controller.on_event(ControllerEvent::MessageReceived(parsed_to_queued(message)))
        {
            pending_decisions.push_back(decision);
        }
    }

    loop {
        while pending_request.is_none() {
            let Some(decision) = pending_decisions.pop_front() else {
                break;
            };
            start_request_for_decision(
                &turn_client,
                &session,
                &cli,
                approval_policy,
                &mut next_request_id,
                &mut pending_start_request_id,
                &mut pending_request,
                decision,
            );
        }

        if should_exit_bridge(
            &controller,
            pending_start_request_id.as_ref(),
            pending_request.is_some(),
            stdin_closed,
        ) {
            break;
        }

        tokio::select! {
            biased;
            maybe_chunk = chunk_rx.recv() => {
                let Some(chunk) = maybe_chunk else {
                    stdin_closed = true;
                    continue;
                };
                if let Some(decision) = controller.on_event(ControllerEvent::MessageReceived(parsed_to_queued(chunk))) {
                    pending_decisions.push_back(decision);
                }
                if let Some(error) = controller.take_validation_error() {
                    anyhow::bail!(error);
                }
            }
            maybe_error = input_error_rx.recv() => {
                let Some(error) = maybe_error else {
                    continue;
                };
                anyhow::bail!(error);
            }
            maybe_event = client.next_event() => {
                let Some(event) = maybe_event else {
                    break;
                };
                if let Some(decision) = handle_app_event(
                    &mut client,
                    &mut session,
                    &mut controller,
                    &mut pending_start_request_id,
                    event,
                ).await? {
                    pending_decisions.push_back(decision);
                }
                if let Some(error) = controller.take_validation_error() {
                    anyhow::bail!(error);
                }
            }
            outcome = async {
                match pending_request.as_mut() {
                    Some(receiver) => receiver.await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                let Some(outcome) = outcome else {
                    anyhow::bail!("pending request task dropped before returning a response");
                };
                pending_request = None;
                if let Some(decision) = handle_pending_request_outcome(
                    &mut controller,
                    &mut pending_start_request_id,
                    outcome,
                )? {
                    pending_decisions.push_back(decision);
                }
                if let Some(error) = controller.take_validation_error() {
                    anyhow::bail!(error);
                }
            }
        }
    }

    client
        .shutdown()
        .await
        .context("shutdown app-server client")
}

fn parsed_to_queued(message: ParsedMessage) -> codex_turn_start_bridge_core::QueuedMessage {
    codex_turn_start_bridge_core::QueuedMessage {
        queue_mode: message.queue_mode,
        text: message.text,
    }
}

fn thread_session_request_from_cli(
    cli: &Cli,
    approval_policy: AskForApproval,
    xml_system_prompt: Option<String>,
) -> Result<ThreadSessionRequest> {
    let base_instructions = match (cli.system_prompt.clone(), xml_system_prompt) {
        (Some(_), Some(_)) => {
            anyhow::bail!("system prompt provided by both --system-prompt and XML stdin");
        }
        (Some(prompt), None) | (None, Some(prompt)) => Some(prompt),
        (None, None) => None,
    };

    Ok(ThreadSessionRequest {
        thread_id: cli.thread_id.clone(),
        model: cli.model.clone(),
        cwd: cli.cwd.clone(),
        approval_policy: Some(approval_policy),
        base_instructions,
    })
}

fn restore_controller_from_thread(
    controller: &mut BridgeController,
    thread: &codex_app_server_protocol::Thread,
) {
    if !matches!(thread.status, ThreadStatus::Active { .. }) {
        return;
    }

    if let Some(turn) = thread.turns.iter().rev().find(|turn| {
        matches!(
            turn.status,
            codex_app_server_protocol::TurnStatus::InProgress
        )
    }) {
        let _ = controller.on_event(ControllerEvent::TurnStarted {
            thread_id: thread.id.clone(),
            turn_id: turn.id.clone(),
        });
    } else {
        controller.restore_busy_unknown_turn();
    }
}

async fn handle_app_event(
    client: &mut AppServerClient,
    session: &mut CodexTurnSession,
    controller: &mut BridgeController,
    pending_start_request_id: &mut Option<String>,
    event: AppServerEvent,
) -> Result<Option<ReleaseDecision>> {
    match event {
        AppServerEvent::Disconnected { message } => {
            anyhow::bail!("app-server disconnected: {message}");
        }
        AppServerEvent::ServerRequest(request) => {
            client
                .reject_server_request(
                    request.id().clone(),
                    unsupported_server_request_error(&request),
                )
                .await
                .context("reject unsupported app-server request")?;
            Err(unsupported_server_request_failure(&request))
        }
        AppServerEvent::ServerNotification(notification) => Ok(on_notification(
            session,
            controller,
            pending_start_request_id,
            notification,
        )),
        AppServerEvent::Lagged { .. } => Ok(None),
    }
}

fn should_exit_bridge(
    controller: &BridgeController,
    pending_start_request_id: Option<&String>,
    pending_request_in_flight: bool,
    stdin_closed: bool,
) -> bool {
    let (after_tool_call, after_any_item, next_turn) = controller.queued_counts();
    stdin_closed
        && pending_start_request_id.is_none()
        && !pending_request_in_flight
        && controller.pending_turn_message().is_none()
        && matches!(
            controller.turn_state(),
            codex_turn_start_bridge_core::TurnState::Idle
        )
        && after_tool_call == 0
        && after_any_item == 0
        && next_turn == 0
}

fn on_notification(
    session: &mut CodexTurnSession,
    controller: &mut BridgeController,
    _pending_start_request_id: &mut Option<String>,
    notification: ServerNotification,
) -> Option<ReleaseDecision> {
    match notification {
        ServerNotification::TurnStarted(payload) => {
            session.on_turn_started(&payload.thread_id, &payload.turn.id);
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: payload.thread_id,
                turn_id: payload.turn.id,
            })
        }
        ServerNotification::TurnCompleted(payload) => {
            session.on_turn_completed(&payload.thread_id, &payload.turn.id);
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: payload.thread_id,
                turn_id: payload.turn.id,
            })
        }
        ServerNotification::TerminalInteraction(payload) => {
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: payload.thread_id,
                turn_id: payload.turn_id,
            })
        }
        ServerNotification::ItemCompleted(payload) => {
            let signal = classify_item_completed(&payload);
            controller.on_event(ControllerEvent::ItemCompleted {
                thread_id: payload.thread_id,
                turn_id: payload.turn_id,
                signal,
            })
        }
        _ => None,
    }
}

fn classify_item_completed(payload: &ItemCompletedNotification) -> CompletionSignal {
    match &payload.item {
        ThreadItem::CommandExecution { .. }
        | ThreadItem::FileChange { .. }
        | ThreadItem::McpToolCall { .. }
        | ThreadItem::DynamicToolCall { .. }
        | ThreadItem::CollabAgentToolCall { .. }
        | ThreadItem::WebSearch { .. }
        | ThreadItem::ImageGeneration { .. } => CompletionSignal::ReleasesAfterAnyItem,
        _ => CompletionSignal::Ignore,
    }
}

enum PendingRequestOutcome {
    TurnStart {
        result: Result<TurnStartResponse>,
    },
    TurnSteer {
        message: codex_turn_start_bridge_core::QueuedMessage,
        result: Result<TurnSteerResponse>,
    },
}

#[allow(clippy::too_many_arguments)]
fn start_request_for_decision(
    turn_client: &CodexTurnClient,
    session: &CodexTurnSession,
    cli: &Cli,
    approval_policy: AskForApproval,
    next_request_id: &mut i64,
    pending_start_request_id: &mut Option<String>,
    pending_request: &mut Option<oneshot::Receiver<PendingRequestOutcome>>,
    decision: ReleaseDecision,
) {
    let (tx, rx) = oneshot::channel();
    match decision.action {
        ReleaseAction::StartTurn => {
            let request_id = format!("turn-start-{}", *next_request_id);
            *next_request_id += 1;
            *pending_start_request_id = Some(request_id.clone());
            let turn_client = turn_client.clone();
            let session = session.clone();
            let cwd = cli.cwd.clone();
            let model = cli.model.clone();
            tokio::spawn(async move {
                let result = turn_client
                    .start_turn(
                        RequestId::String(request_id),
                        &session,
                        TurnRequest {
                            text: decision.message.text,
                            model,
                            cwd,
                            approval_policy: Some(approval_policy),
                        },
                    )
                    .await
                    .map_err(anyhow::Error::from);
                let _ = tx.send(PendingRequestOutcome::TurnStart { result });
            });
        }
        ReleaseAction::SteerTurn { turn_id } => {
            let request_id = RequestId::String(format!("turn-steer-{}", *next_request_id));
            *next_request_id += 1;
            let turn_client = turn_client.clone();
            let session = session.clone();
            let message = decision.message;
            let text = message.text.clone();
            tokio::spawn(async move {
                let result = turn_client
                    .steer_turn(request_id, &session, turn_id, text)
                    .await
                    .map_err(anyhow::Error::from);
                let _ = tx.send(PendingRequestOutcome::TurnSteer { message, result });
            });
        }
    }

    *pending_request = Some(rx);
}

fn handle_pending_request_outcome(
    controller: &mut BridgeController,
    pending_start_request_id: &mut Option<String>,
    outcome: PendingRequestOutcome,
) -> Result<Option<ReleaseDecision>> {
    match outcome {
        PendingRequestOutcome::TurnStart { result } => {
            *pending_start_request_id = None;
            match result {
                Ok(response) => Ok(controller.on_event(
                    ControllerEvent::TurnStartAcceptedWithTurnId {
                        turn_id: response.turn.id,
                    },
                )),
                Err(err) if is_active_turn_not_steerable(&err.to_string()) => {
                    Ok(controller
                        .on_event(ControllerEvent::TurnStartRejectedActiveTurnNotSteerable))
                }
                Err(err) => Err(err).context("turn/start failed"),
            }
        }
        PendingRequestOutcome::TurnSteer { message, result } => match result {
            Ok(response) => Ok(controller.on_event(ControllerEvent::SteerAccepted {
                turn_id: response.turn_id,
            })),
            Err(err) if should_retry_steer_next_turn(&err.to_string()) => Ok(controller
                .on_event(ControllerEvent::SteerRejectedActiveTurnNotSteerable { message })),
            Err(err) => Err(err).context("turn/steer failed"),
        },
    }
}

async fn read_stdin_chunks(
    policy: QuiescencePolicy,
    chunk_tx: mpsc::UnboundedSender<ParsedMessage>,
) -> Result<()> {
    read_chunks_from_reader(tokio::io::stdin(), policy, chunk_tx).await
}

async fn read_chunks_from_reader<R>(
    mut reader: R,
    policy: QuiescencePolicy,
    chunk_tx: mpsc::UnboundedSender<ParsedMessage>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut accumulator = codex_turn_start_bridge_core::ChunkAccumulator::new(policy);
    let mut buf = [0_u8; 4096];
    let quiet_for = Duration::from_millis(policy.chunk_quiescence_ms);

    loop {
        let bytes_read = reader.read(&mut buf).await.context("read stdin")?;
        if bytes_read == 0 {
            if let Some(bytes) = accumulator.push(ReadSignal::Eof)
                && let Ok(text) = String::from_utf8(bytes)
            {
                let _ = chunk_tx.send(parse_prefixed_message(&text));
            }
            break;
        }

        let _ = accumulator.push(ReadSignal::Data(&buf[..bytes_read]));

        loop {
            match timeout(quiet_for, reader.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    if let Some(bytes) = accumulator.push(ReadSignal::Eof)
                        && let Ok(text) = String::from_utf8(bytes)
                    {
                        let _ = chunk_tx.send(parse_prefixed_message(&text));
                    }
                    return Ok(());
                }
                Ok(Ok(bytes_read)) => {
                    let _ = accumulator.push(ReadSignal::Data(&buf[..bytes_read]));
                }
                Ok(Err(err)) => return Err(err).context("read stdin"),
                Err(_elapsed) => {
                    if let Some(bytes) = accumulator.push(ReadSignal::Quiescent)
                        && let Ok(text) = String::from_utf8(bytes)
                    {
                        let _ = chunk_tx.send(parse_prefixed_message(&text));
                    }
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn read_xml_prelude_from_reader<R>(reader: &mut R) -> Result<XmlReadPrelude>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser = XmlInputParser::default();
    let mut buf = [0_u8; 4096];
    let mut system_prompt = None;

    loop {
        let bytes_read = reader.read(&mut buf).await.context("read stdin")?;
        if bytes_read == 0 {
            parser.finish().context("parse XML stdin")?;
            return Ok(XmlReadPrelude {
                system_prompt,
                pending_messages: Vec::new(),
                stdin_closed: true,
                parser,
            });
        }

        let input = std::str::from_utf8(&buf[..bytes_read]).context("read XML stdin as UTF-8")?;
        let items = parser.push(input).context("parse XML stdin")?;
        let mut pending_messages = Vec::new();
        for item in items {
            match item {
                ParsedXmlInput::SystemPrompt(prompt) => {
                    if system_prompt.replace(prompt).is_some() {
                        anyhow::bail!("system_prompt specified more than once");
                    }
                }
                ParsedXmlInput::Message(message) => pending_messages.push(message),
            }
        }

        if !pending_messages.is_empty() {
            return Ok(XmlReadPrelude {
                system_prompt,
                pending_messages,
                stdin_closed: false,
                parser,
            });
        }
    }
}

async fn read_xml_chunks_from_reader<R>(
    mut reader: R,
    mut parser: XmlInputParser,
    chunk_tx: mpsc::UnboundedSender<ParsedMessage>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = [0_u8; 4096];

    loop {
        let bytes_read = reader.read(&mut buf).await.context("read stdin")?;
        if bytes_read == 0 {
            parser.finish().context("parse XML stdin")?;
            break;
        }

        let input = std::str::from_utf8(&buf[..bytes_read]).context("read XML stdin as UTF-8")?;
        for item in parser.push(input).context("parse XML stdin")? {
            match item {
                ParsedXmlInput::SystemPrompt(_) => {
                    anyhow::bail!("system_prompt must appear before the first message");
                }
                ParsedXmlInput::Message(message) => {
                    let _ = chunk_tx.send(message);
                }
            }
        }
    }

    Ok(())
}

fn parse_approval_policy(value: &str) -> Result<AskForApproval> {
    match value {
        "untrusted" | "unless-trusted" | "unlessTrusted" => Ok(AskForApproval::UnlessTrusted),
        "on-failure" | "onFailure" => Ok(AskForApproval::OnFailure),
        "on-request" | "onRequest" => Ok(AskForApproval::OnRequest),
        "never" => Ok(AskForApproval::Never),
        _ => anyhow::bail!(
            "unknown approval policy: {value}. Expected one of: untrusted, on-failure, on-request, never"
        ),
    }
}

fn is_active_turn_not_steerable(message: &str) -> bool {
    message.contains("ActiveTurnNotSteerable")
        || message.contains("cannot accept same-turn steering")
        || message.contains("cannot steer a review turn")
        || message.contains("cannot steer a compact turn")
}

fn should_retry_steer_next_turn(message: &str) -> bool {
    is_active_turn_not_steerable(message)
        || message.contains("expected_turn_id")
        || message.contains("expected turn")
        || message.contains("expected active turn id")
        || message.contains("no active turn to steer")
        || message.contains("stale")
        || message.contains("turn completed")
}

fn unsupported_server_request_error(request: &ServerRequest) -> JSONRPCErrorError {
    let method = server_request_method_name(request);

    JSONRPCErrorError {
        code: -32601,
        message: format!(
            "turn-start bridge does not support interactive server request `{method}`"
        ),
        data: None,
    }
}

fn unsupported_server_request_failure(request: &ServerRequest) -> anyhow::Error {
    anyhow::anyhow!(
        "turn-start bridge does not support interactive server request `{}`",
        server_request_method_name(request)
    )
}

fn server_request_method_name(request: &ServerRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "<unknown>".to_string())
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use super::CodexTurnSession;
    use super::QuiescencePolicy;
    use super::StdinFormat;
    use super::on_notification;
    use super::read_chunks_from_reader;
    use super::read_xml_prelude_from_reader;
    use super::server_request_method_name;
    use super::should_exit_bridge;
    use super::should_retry_steer_next_turn;
    use super::thread_session_request_from_cli;
    use super::unsupported_server_request_error;
    use super::unsupported_server_request_failure;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ServerRequest;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadStatus;
    use codex_app_server_protocol::ToolRequestUserInputOption;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnStartedNotification;
    use codex_app_server_protocol::TurnStatus;
    use codex_turn_start_bridge_core::BridgeController;
    use codex_turn_start_bridge_core::ControllerEvent;
    use codex_turn_start_bridge_core::QueueMode;
    use codex_turn_start_bridge_core::QueuedMessage;
    use codex_turn_start_bridge_core::ReleaseAction;
    use codex_turn_start_bridge_core::ReleaseDecision;
    use codex_turn_start_bridge_core::ReleaseReason;
    use codex_turn_start_bridge_core::TurnState;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;

    #[test]
    fn thread_session_request_includes_system_prompt_override() {
        let cli = Cli {
            config_overrides: Default::default(),
            codex_bin: "codex".into(),
            thread_id: None,
            approval_policy: "on-request".to_string(),
            model: None,
            cwd: None,
            system_prompt: Some("stay terse".to_string()),
            stdin_format: StdinFormat::Raw,
            chunk_quiescence_ms: 25,
        };

        let request = thread_session_request_from_cli(&cli, AskForApproval::OnRequest, None)
            .expect("request should be valid");

        assert_eq!(request.base_instructions, Some("stay terse".to_string()));
    }

    #[tokio::test]
    async fn xml_prelude_reads_startup_system_prompt_and_first_messages() {
        let reader = tokio::io::BufReader::new(
            b"<system_prompt>be terse</system_prompt>\
              <message type=\"user\">first</message>\
              <message type=\"user\" queue=\"Immediate\">second</message>"
                .as_slice(),
        );
        let mut reader = reader;

        let prelude = read_xml_prelude_from_reader(&mut reader)
            .await
            .expect("prelude should parse");

        assert_eq!(prelude.system_prompt, Some("be terse".to_string()));
        assert_eq!(
            prelude.pending_messages,
            vec![
                codex_turn_start_bridge_core::ParsedMessage {
                    queue_mode: QueueMode::Default,
                    text: "first".to_string(),
                },
                codex_turn_start_bridge_core::ParsedMessage {
                    queue_mode: QueueMode::Immediate,
                    text: "second".to_string(),
                },
            ]
        );
        assert_eq!(prelude.stdin_closed, false);
    }

    #[test]
    fn cli_system_prompt_conflicts_with_xml_system_prompt() {
        let cli = Cli {
            config_overrides: Default::default(),
            codex_bin: "codex".into(),
            thread_id: None,
            approval_policy: "on-request".to_string(),
            model: None,
            cwd: None,
            system_prompt: Some("from cli".to_string()),
            stdin_format: StdinFormat::Xml,
            chunk_quiescence_ms: 25,
        };

        let err = thread_session_request_from_cli(
            &cli,
            AskForApproval::OnRequest,
            Some("from xml".to_string()),
        )
        .expect_err("duplicate system prompt should fail");

        assert_eq!(
            err.to_string(),
            "system prompt provided by both --system-prompt and XML stdin"
        );
    }

    fn active_thread_fixture() -> Thread {
        Thread {
            id: "thread-1".to_string(),
            forked_from_id: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            status: ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            path: None,
            cwd: AbsolutePathBuf::try_from("/tmp").expect("absolute path"),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
            turns: Vec::new(),
        }
    }

    #[tokio::test]
    async fn chunk_reader_merges_bytes_arriving_within_quiescence_window() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();

        let writer_task = tokio::spawn(async move {
            writer.write_all(b"hello").await.expect("write hello");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            writer.write_all(b" world").await.expect("write world");
        });

        read_chunks_from_reader(
            reader,
            QuiescencePolicy {
                chunk_quiescence_ms: 25,
            },
            chunk_tx,
        )
        .await
        .expect("read chunks");
        writer_task.await.expect("writer task should succeed");

        let mut messages = Vec::new();
        while let Some(message) = chunk_rx.recv().await {
            messages.push(message.text);
        }

        assert_eq!(messages, vec!["hello world"]);
    }

    #[tokio::test]
    async fn chunk_reader_splits_bytes_after_quiescence_window() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();

        let writer_task = tokio::spawn(async move {
            writer.write_all(b"hello").await.expect("write hello");
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            writer.write_all(b" world").await.expect("write world");
        });

        read_chunks_from_reader(
            reader,
            QuiescencePolicy {
                chunk_quiescence_ms: 25,
            },
            chunk_tx,
        )
        .await
        .expect("read chunks");
        writer_task.await.expect("writer task should succeed");

        let mut messages = Vec::new();
        while let Some(message) = chunk_rx.recv().await {
            messages.push(message.text);
        }

        assert_eq!(messages, vec!["hello", " world"]);
    }

    #[test]
    fn stale_turn_steer_errors_retry_on_next_turn() {
        assert!(should_retry_steer_next_turn(
            "expected_turn_id does not match the active turn"
        ));
        assert!(should_retry_steer_next_turn(
            "expected active turn id `turn-1` but found `turn-2`"
        ));
        assert!(should_retry_steer_next_turn("no active turn to steer"));
        assert!(should_retry_steer_next_turn(
            "cannot accept same-turn steering after turn completed"
        ));
        assert!(should_retry_steer_next_turn("cannot steer a review turn"));
        assert!(should_retry_steer_next_turn("cannot steer a compact turn"));
        assert!(!should_retry_steer_next_turn("network timeout"));
    }

    #[test]
    fn bridge_only_exits_after_stdin_eof_when_work_is_drained() {
        let controller = BridgeController::new("thread-1");

        assert!(should_exit_bridge(&controller, None, false, true));
        assert!(!should_exit_bridge(&controller, None, false, false));
        assert!(!should_exit_bridge(
            &controller,
            Some(&"pending".to_string()),
            false,
            true
        ));
        assert!(!should_exit_bridge(&controller, None, true, true));
    }

    #[test]
    fn unsupported_server_requests_are_rejected_with_clear_error() {
        let request = ServerRequest::ToolRequestUserInput {
            request_id: RequestId::String("req-1".to_string()),
            params: ToolRequestUserInputParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                questions: vec![ToolRequestUserInputQuestion {
                    id: "q-1".to_string(),
                    header: "Input".to_string(),
                    question: "Continue?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![ToolRequestUserInputOption {
                        label: "Yes".to_string(),
                        description: "Continue the tool".to_string(),
                    }]),
                }],
            },
        };

        let error = unsupported_server_request_error(&request);

        assert_eq!(error.code, -32601);
        assert!(error.message.contains("tool/requestUserInput"));
        assert!(
            error
                .message
                .contains("does not support interactive server request")
        );
        assert_eq!(
            server_request_method_name(&request),
            "item/tool/requestUserInput"
        );
    }

    #[test]
    fn unsupported_server_requests_fail_loudly_after_rejection() {
        let request = ServerRequest::ToolRequestUserInput {
            request_id: RequestId::String("req-1".to_string()),
            params: ToolRequestUserInputParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                questions: vec![ToolRequestUserInputQuestion {
                    id: "q-1".to_string(),
                    header: "Input".to_string(),
                    question: "Continue?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![ToolRequestUserInputOption {
                        label: "Yes".to_string(),
                        description: "Continue the tool".to_string(),
                    }]),
                }],
            },
        };
        let error = unsupported_server_request_failure(&request);

        assert!(error.to_string().contains(
            "turn-start bridge does not support interactive server request `item/tool/requestUserInput`"
        ));
    }

    #[test]
    fn turn_started_notification_does_not_accept_pending_start_early() {
        let mut controller = BridgeController::new("thread-1");
        let mut session = CodexTurnSession::from_thread(&active_thread_fixture());
        let mut pending_start_request_id = Some("turn-start-1".to_string());
        let queued = QueuedMessage {
            queue_mode: QueueMode::AfterToolCall,
            text: "hello".to_string(),
        };

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued.clone())),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued.clone(),
            })
        );

        let decision = on_notification(
            &mut session,
            &mut controller,
            &mut pending_start_request_id,
            ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: "thread-1".to_string(),
                turn: Turn {
                    id: "turn-1".to_string(),
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            }),
        );

        assert_eq!(decision, None);
        assert_eq!(pending_start_request_id, Some("turn-start-1".to_string()));
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: Some("turn-1".to_string()),
            }
        );
        assert_eq!(controller.pending_turn_message(), Some(&queued));
    }

    #[test]
    fn turn_started_notification_before_chunk_preserves_pending_window_queueing() {
        let mut controller = BridgeController::new("thread-1");
        let mut session = CodexTurnSession::from_thread(&active_thread_fixture());
        let mut pending_start_request_id = Some("turn-start-1".to_string());

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(QueuedMessage {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            })),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: QueuedMessage {
                    queue_mode: QueueMode::AfterToolCall,
                    text: "hello".to_string(),
                },
            })
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: None,
            }
        );

        let decision = on_notification(
            &mut session,
            &mut controller,
            &mut pending_start_request_id,
            ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: "thread-1".to_string(),
                turn: Turn {
                    id: "turn-1".to_string(),
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            }),
        );

        assert_eq!(decision, None);
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(QueuedMessage {
                queue_mode: QueueMode::Immediate,
                text: "interrupt".to_string(),
            })),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: Some("turn-1".to_string()),
            }
        );
    }
}
