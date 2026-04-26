use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;

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
use codex_app_server_protocol::ApprovalsReviewer;
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
use codex_turn_start_bridge_core::sideband;
use codex_turn_start_bridge_core::sideband::ManagedServerRequest;
use codex_turn_start_bridge_core::sideband::ServerRequestResolution;
use codex_turn_start_bridge_core::sideband::SidebandOutputs;
use codex_turn_start_bridge_core::sideband::SidebandResponseEvent;
use codex_turn_start_bridge_core::sideband::ThreadResolvedPayload;
use codex_utils_cli::CliConfigOverrides;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[cfg(unix)]
use std::os::fd::FromRawFd;

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
    approvals_reviewer: Option<String>,

    #[arg(long)]
    system_prompt: Option<String>,

    #[arg(long = "thread-id-fd", value_name = "FD")]
    thread_id_fd: Option<i32>,

    #[arg(long = "xml-input-fd", value_name = "FD")]
    xml_input_fd: Option<i32>,

    #[arg(long = "server-request-events-fd", value_name = "FD")]
    server_request_events_fd: Option<i32>,

    #[arg(long = "server-request-responses-fd", value_name = "FD")]
    server_request_responses_fd: Option<i32>,

    #[arg(long = "control-events-fd", value_name = "FD")]
    control_events_fd: Option<i32>,

    #[arg(long = "control-responses-fd", value_name = "FD")]
    control_responses_fd: Option<i32>,

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

type BoxedAsyncReader = Pin<Box<dyn AsyncRead + Send>>;

#[ctor::ctor]
fn pre_main() {
    codex_process_hardening::pre_main_hardening();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let approval_policy = parse_approval_policy(&cli.approval_policy)?;
    let approvals_reviewer = cli
        .approvals_reviewer
        .as_deref()
        .map(parse_approvals_reviewer)
        .transpose()?;
    validate_xml_input_mode(&cli)?;
    validate_sideband_platform_support(&cli)?;
    validate_unique_sideband_fds(&cli)?;
    let sideband_outputs = SidebandOutputs::from_fds(
        cli.thread_id_fd,
        cli.server_request_events_fd,
        cli.control_events_fd,
    )
    .context("configure sideband outputs")?;
    let mut sideband_response_rx = match (cli.server_request_responses_fd, cli.control_responses_fd)
    {
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "--server-request-responses-fd and --control-responses-fd are mutually exclusive"
            );
        }
        (Some(fd), None) | (None, Some(fd)) => {
            if !sideband_outputs.has_request_event_sink() {
                anyhow::bail!(
                    "a sideband response fd requires either --server-request-events-fd or --control-events-fd"
                );
            }
            Some(sideband::spawn_response_reader(fd).context("spawn sideband response reader")?)
        }
        (None, None) => None,
    };
    let mut sideband_response_available = sideband_response_rx.is_some();
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<ParsedMessage>();
    let (input_error_tx, mut input_error_rx) = mpsc::unbounded_channel::<String>();
    let mut stdin_closed = false;
    let mut initial_messages = Vec::new();
    let xml_system_prompt = match cli.stdin_format {
        StdinFormat::Raw => None,
        StdinFormat::Xml => {
            let mut xml_reader = open_xml_input_reader(cli.xml_input_fd)?;
            let prelude = read_xml_prelude_from_reader(&mut xml_reader).await?;
            stdin_closed = prelude.stdin_closed;
            initial_messages = prelude.pending_messages;
            if !stdin_closed {
                let chunk_tx = chunk_tx.clone();
                let input_error_tx = input_error_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        read_xml_chunks_from_reader(xml_reader, prelude.parser, chunk_tx).await
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
    let thread_request = thread_session_request_from_cli(
        &cli,
        approval_policy,
        approvals_reviewer,
        xml_system_prompt,
    )
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
    let thread_resolved = if cli.thread_id.is_some() {
        ThreadResolvedPayload::resumed(&thread_start.thread.id)
    } else {
        ThreadResolvedPayload::started(&thread_start.thread.id)
    };
    sideband_outputs
        .emit_thread_resolved(&thread_resolved)
        .context("write thread-id handoff")?;
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
    let mut pending_server_requests = HashMap::<RequestId, ManagedServerRequest>::new();
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
                approvals_reviewer,
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
                    &sideband_outputs,
                    &mut pending_server_requests,
                    sideband_response_available,
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
            sideband_event = async {
                match sideband_response_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match sideband_event {
                    Some(SidebandResponseEvent::Response(response)) => {
                        if let Some((request_id, resolution)) =
                            take_valid_sideband_response(&mut pending_server_requests, response)?
                        {
                            apply_server_request_resolution(&mut client, request_id, resolution)
                                .await
                                .context("apply sideband server-request resolution")?;
                        }
                    }
                    Some(SidebandResponseEvent::ParseError(message)) => {
                        eprintln!("{message}");
                    }
                    Some(SidebandResponseEvent::ReadError(message)) => {
                        resolve_pending_requests_on_response_close(
                            &mut client,
                            &mut pending_server_requests,
                            &message,
                        )
                        .await?;
                        anyhow::bail!(message);
                    }
                    Some(SidebandResponseEvent::Closed) | None => {
                        sideband_response_available = false;
                        sideband_response_rx = None;
                        let close_message = "turn-start bridge sideband response channel closed";
                        if pending_server_requests.is_empty() {
                            continue;
                        }
                        resolve_pending_requests_on_response_close(
                            &mut client,
                            &mut pending_server_requests,
                            close_message,
                        )
                        .await?;
                        anyhow::bail!(close_message);
                    }
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
    approvals_reviewer: Option<ApprovalsReviewer>,
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
        approvals_reviewer,
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

#[allow(clippy::too_many_arguments)]
async fn handle_app_event(
    client: &mut AppServerClient,
    session: &mut CodexTurnSession,
    controller: &mut BridgeController,
    pending_start_request_id: &mut Option<String>,
    sideband_outputs: &SidebandOutputs,
    pending_server_requests: &mut HashMap<RequestId, ManagedServerRequest>,
    sideband_response_available: bool,
    event: AppServerEvent,
) -> Result<Option<ReleaseDecision>> {
    match event {
        AppServerEvent::Disconnected { message } => {
            anyhow::bail!("app-server disconnected: {message}");
        }
        AppServerEvent::ServerRequest(request) => {
            if let Ok(managed) = ManagedServerRequest::try_from(&request) {
                if let Err(err) = sideband_outputs.emit_request(&managed) {
                    client
                        .reject_server_request(
                            request.id().clone(),
                            JSONRPCErrorError {
                                code: -32000,
                                message: format!(
                                    "turn-start bridge failed to emit sideband event for `{}`: {err}",
                                    managed.kind()
                                ),
                                data: None,
                            },
                        )
                        .await
                        .context("reject request after sideband event write failure")?;
                    return Err(err).context("write sideband server-request event");
                }

                if sideband_response_available {
                    pending_server_requests.insert(managed.request_id().clone(), managed);
                    Ok(None)
                } else {
                    client
                        .reject_server_request(
                            request.id().clone(),
                            JSONRPCErrorError {
                                code: -32601,
                                message: format!(
                                    "turn-start bridge received interactive server request `{}` but no sideband response fd is configured",
                                    server_request_method_name(&request)
                                ),
                                data: None,
                            },
                        )
                        .await
                        .context("reject supported request without sideband response lane")?;
                    Err(anyhow::anyhow!(
                        "turn-start bridge received interactive server request `{}` but no sideband response fd is configured",
                        server_request_method_name(&request)
                    ))
                }
            } else {
                client
                    .reject_server_request(
                        request.id().clone(),
                        unsupported_server_request_error(&request),
                    )
                    .await
                    .context("reject unsupported app-server request")?;
                Err(unsupported_server_request_failure(&request))
            }
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
                item_key: Some(payload.item.id().to_string()),
            })
        }
        _ => None,
    }
}

fn classify_item_completed(payload: &ItemCompletedNotification) -> CompletionSignal {
    match &payload.item {
        ThreadItem::UserMessage { .. } | ThreadItem::HookPrompt { .. } => CompletionSignal::Ignore,
        ThreadItem::AgentMessage { .. }
        | ThreadItem::Plan { .. }
        | ThreadItem::Reasoning { .. }
        | ThreadItem::CommandExecution { .. }
        | ThreadItem::FileChange { .. }
        | ThreadItem::McpToolCall { .. }
        | ThreadItem::DynamicToolCall { .. }
        | ThreadItem::CollabAgentToolCall { .. }
        | ThreadItem::WebSearch { .. }
        | ThreadItem::ImageView { .. }
        | ThreadItem::ImageGeneration { .. }
        | ThreadItem::EnteredReviewMode { .. }
        | ThreadItem::ExitedReviewMode { .. }
        | ThreadItem::ContextCompaction { .. } => CompletionSignal::ReleasesAfterAnyItem,
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
    approvals_reviewer: Option<ApprovalsReviewer>,
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
                            approvals_reviewer,
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
    R: AsyncRead + Unpin,
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
    R: AsyncRead + Unpin,
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

fn parse_approvals_reviewer(value: &str) -> Result<ApprovalsReviewer> {
    match value {
        "user" => Ok(ApprovalsReviewer::User),
        "auto_review" | "auto-review" | "guardian_subagent" | "guardian-subagent" => {
            Ok(ApprovalsReviewer::AutoReview)
        }
        _ => anyhow::bail!(
            "unknown approvals reviewer: {value}. Expected one of: user, auto_review, guardian_subagent"
        ),
    }
}

fn validate_sideband_platform_support(_cli: &Cli) -> Result<()> {
    #[cfg(not(unix))]
    {
        if _cli.xml_input_fd.is_some()
            || _cli.thread_id_fd.is_some()
            || _cli.server_request_events_fd.is_some()
            || _cli.server_request_responses_fd.is_some()
            || _cli.control_events_fd.is_some()
            || _cli.control_responses_fd.is_some()
        {
            anyhow::bail!("bridge sideband FDs are only supported on Unix targets");
        }
    }

    Ok(())
}

fn validate_xml_input_mode(cli: &Cli) -> Result<()> {
    if cli.xml_input_fd.is_some() && !matches!(cli.stdin_format, StdinFormat::Xml) {
        anyhow::bail!("--xml-input-fd requires --stdin-format xml");
    }

    Ok(())
}

fn validate_unique_sideband_fds(cli: &Cli) -> Result<()> {
    let mut seen = HashMap::<i32, &str>::new();
    for (name, fd) in [
        ("xml-input-fd", cli.xml_input_fd),
        ("thread-id-fd", cli.thread_id_fd),
        ("server-request-events-fd", cli.server_request_events_fd),
        (
            "server-request-responses-fd",
            cli.server_request_responses_fd,
        ),
        ("control-events-fd", cli.control_events_fd),
        ("control-responses-fd", cli.control_responses_fd),
    ] {
        if let Some(fd) = fd
            && let Some(previous) = seen.insert(fd, name)
        {
            anyhow::bail!(
                "fd `{fd}` is configured for both `--{previous}` and `--{name}`; use distinct sideband file descriptors"
            );
        }
    }

    Ok(())
}

#[cfg(unix)]
fn open_xml_input_reader(xml_input_fd: Option<i32>) -> Result<BoxedAsyncReader> {
    if let Some(fd) = xml_input_fd {
        if fd < 0 {
            anyhow::bail!("invalid --xml-input-fd value `{fd}`");
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        return Ok(Box::pin(tokio::fs::File::from_std(file)));
    }

    Ok(Box::pin(tokio::io::stdin()))
}

#[cfg(not(unix))]
fn open_xml_input_reader(xml_input_fd: Option<i32>) -> Result<BoxedAsyncReader> {
    let _ = xml_input_fd;
    Ok(Box::pin(tokio::io::stdin()))
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

async fn apply_server_request_resolution(
    client: &mut AppServerClient,
    request_id: RequestId,
    resolution: ServerRequestResolution,
) -> Result<()> {
    match resolution {
        ServerRequestResolution::Resolve(response) => client
            .resolve_server_request(request_id, response)
            .await
            .context("resolve app-server request"),
        ServerRequestResolution::Reject(error) => client
            .reject_server_request(request_id, error)
            .await
            .context("reject app-server request"),
    }
}

fn take_valid_sideband_response(
    pending_server_requests: &mut HashMap<RequestId, ManagedServerRequest>,
    response: sideband::SidebandResponseEnvelope,
) -> Result<Option<(RequestId, ServerRequestResolution)>> {
    let request_id = response.request_id.clone();
    let Some(request) = pending_server_requests.get(&request_id) else {
        eprintln!(
            "turn-start bridge ignoring sideband response for unknown request id `{request_id}`"
        );
        return Ok(None);
    };

    match request.parse_response(response) {
        Ok(resolution) => {
            pending_server_requests.remove(&request_id);
            Ok(Some((request_id, resolution)))
        }
        Err(err) => {
            eprintln!(
                "turn-start bridge ignoring invalid sideband response for request id `{request_id}`: {err}"
            );
            Ok(None)
        }
    }
}

async fn resolve_pending_requests_on_response_close(
    client: &mut AppServerClient,
    pending_server_requests: &mut HashMap<RequestId, ManagedServerRequest>,
    reason: &str,
) -> Result<()> {
    let pending = std::mem::take(pending_server_requests);
    for (request_id, request) in pending {
        let resolution = request
            .default_resolution_on_input_closed()
            .with_context(|| format!("resolve pending request `{request_id}` after `{reason}`"))?;
        apply_server_request_resolution(client, request_id, resolution)
            .await
            .with_context(|| format!("apply fallback resolution after `{reason}`"))?;
    }
    Ok(())
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
    use super::classify_item_completed;
    use super::on_notification;
    #[cfg(unix)]
    use super::open_xml_input_reader;
    use super::read_chunks_from_reader;
    use super::read_xml_prelude_from_reader;
    use super::server_request_method_name;
    use super::should_exit_bridge;
    use super::should_retry_steer_next_turn;
    use super::sideband::ManagedServerRequest;
    use super::sideband::SidebandResponseEnvelope;
    use super::sideband::ThreadResolvedPayload;
    use super::take_valid_sideband_response;
    use super::thread_session_request_from_cli;
    use super::unsupported_server_request_error;
    use super::unsupported_server_request_failure;
    use super::validate_unique_sideband_fds;
    use super::validate_xml_input_mode;
    use codex_app_server_protocol::ApprovalsReviewer;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::ChatgptAuthTokensRefreshParams;
    use codex_app_server_protocol::ChatgptAuthTokensRefreshReason;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ServerRequest;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadItem;
    use codex_app_server_protocol::ThreadStatus;
    use codex_app_server_protocol::ToolRequestUserInputOption;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnStartedNotification;
    use codex_app_server_protocol::TurnStatus;
    use codex_turn_start_bridge_core::BridgeController;
    use codex_turn_start_bridge_core::CompletionSignal;
    use codex_turn_start_bridge_core::ControllerEvent;
    use codex_turn_start_bridge_core::QueueMode;
    use codex_turn_start_bridge_core::QueuedMessage;
    use codex_turn_start_bridge_core::ReleaseAction;
    use codex_turn_start_bridge_core::ReleaseDecision;
    use codex_turn_start_bridge_core::ReleaseReason;
    use codex_turn_start_bridge_core::TurnState;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    #[cfg(unix)]
    use std::os::fd::IntoRawFd;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
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
            approvals_reviewer: None,
            system_prompt: Some("stay terse".to_string()),
            thread_id_fd: None,
            xml_input_fd: None,
            server_request_events_fd: None,
            server_request_responses_fd: None,
            control_events_fd: None,
            control_responses_fd: None,
            stdin_format: StdinFormat::Raw,
            chunk_quiescence_ms: 25,
        };

        let request = thread_session_request_from_cli(&cli, AskForApproval::OnRequest, None, None)
            .expect("request should be valid");

        assert_eq!(request.base_instructions, Some("stay terse".to_string()));
    }

    #[test]
    fn thread_session_request_includes_approvals_reviewer_override() {
        let cli = Cli {
            config_overrides: Default::default(),
            codex_bin: "codex".into(),
            thread_id: None,
            approval_policy: "on-request".to_string(),
            model: None,
            cwd: None,
            approvals_reviewer: Some("guardian_subagent".to_string()),
            system_prompt: None,
            thread_id_fd: None,
            xml_input_fd: None,
            server_request_events_fd: None,
            server_request_responses_fd: None,
            control_events_fd: None,
            control_responses_fd: None,
            stdin_format: StdinFormat::Raw,
            chunk_quiescence_ms: 25,
        };

        let request = thread_session_request_from_cli(
            &cli,
            AskForApproval::OnRequest,
            Some(ApprovalsReviewer::AutoReview),
            None,
        )
        .expect("request should be valid");

        assert_eq!(
            request.approvals_reviewer,
            Some(ApprovalsReviewer::AutoReview)
        );
    }

    #[test]
    fn validate_unique_sideband_fds_rejects_reused_input_and_output_fd() {
        let cli = Cli {
            config_overrides: Default::default(),
            codex_bin: "codex".into(),
            thread_id: None,
            approval_policy: "on-request".to_string(),
            model: None,
            cwd: None,
            approvals_reviewer: None,
            system_prompt: None,
            thread_id_fd: None,
            xml_input_fd: None,
            server_request_events_fd: None,
            server_request_responses_fd: None,
            control_events_fd: Some(9),
            control_responses_fd: Some(9),
            stdin_format: StdinFormat::Raw,
            chunk_quiescence_ms: 25,
        };

        let err = validate_unique_sideband_fds(&cli).expect_err("duplicate fd should fail");

        assert_eq!(
            err.to_string(),
            "fd `9` is configured for both `--control-events-fd` and `--control-responses-fd`; use distinct sideband file descriptors"
        );
    }

    #[test]
    fn thread_resolved_payload_uses_resumed_source() {
        assert_eq!(
            ThreadResolvedPayload::resumed("thread-2"),
            ThreadResolvedPayload {
                thread_id: "thread-2".to_string(),
                source: "resumed".to_string(),
            }
        );
    }

    #[test]
    fn tool_user_input_request_is_supported_by_sideband_layer() {
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

        let managed =
            ManagedServerRequest::try_from(&request).expect("tool user input should be supported");

        assert_eq!(managed.kind(), "tool_user_input_request");
    }

    #[test]
    fn invalid_sideband_response_is_ignored_and_request_stays_pending() {
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
        let managed =
            ManagedServerRequest::try_from(&request).expect("tool user input should be supported");
        let mut pending = HashMap::from([(managed.request_id().clone(), managed)]);

        let outcome = take_valid_sideband_response(
            &mut pending,
            SidebandResponseEnvelope {
                kind: "tool_user_input_response".to_string(),
                request_id: RequestId::String("req-1".to_string()),
                response: serde_json::json!({ "unexpected": true }),
            },
        )
        .expect("invalid responses should be ignored");

        assert!(outcome.is_none());
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key(&RequestId::String("req-1".to_string())));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn xml_prelude_can_read_from_explicit_xml_input_fd() {
        use std::io::Write;

        let (read_end, mut write_end) = UnixStream::pair().expect("socket pair should open");
        write_end
            .write_all(b"<message type=\"user\">fd payload</message>")
            .expect("pipe write should succeed");
        drop(write_end);

        let mut reader =
            open_xml_input_reader(Some(read_end.into_raw_fd())).expect("xml input fd should open");
        let prelude = read_xml_prelude_from_reader(&mut reader)
            .await
            .expect("prelude should parse");

        assert_eq!(prelude.system_prompt, None);
        assert_eq!(
            prelude.pending_messages,
            vec![codex_turn_start_bridge_core::ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "fd payload".to_string(),
            }]
        );
        assert_eq!(prelude.stdin_closed, false);
    }

    #[test]
    fn xml_input_fd_requires_xml_mode() {
        let cli = Cli {
            config_overrides: Default::default(),
            codex_bin: "codex".into(),
            thread_id: None,
            approval_policy: "on-request".to_string(),
            model: None,
            cwd: None,
            approvals_reviewer: None,
            system_prompt: None,
            thread_id_fd: None,
            xml_input_fd: Some(4),
            server_request_events_fd: None,
            server_request_responses_fd: None,
            control_events_fd: None,
            control_responses_fd: None,
            stdin_format: StdinFormat::Raw,
            chunk_quiescence_ms: 25,
        };

        let err = validate_xml_input_mode(&cli).expect_err("raw stdin mode should reject xml fd");

        assert_eq!(
            err.to_string(),
            "--xml-input-fd requires --stdin-format xml"
        );
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
            approvals_reviewer: None,
            system_prompt: Some("from cli".to_string()),
            thread_id_fd: None,
            xml_input_fd: None,
            server_request_events_fd: None,
            server_request_responses_fd: None,
            control_events_fd: None,
            control_responses_fd: None,
            stdin_format: StdinFormat::Xml,
            chunk_quiescence_ms: 25,
        };

        let err = thread_session_request_from_cli(
            &cli,
            AskForApproval::OnRequest,
            None,
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
    fn classify_item_completed_releases_after_agent_message_items() {
        assert_eq!(
            classify_item_completed(&ItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ThreadItem::AgentMessage {
                    id: "item-1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                    memory_citation: None,
                },
            }),
            CompletionSignal::ReleasesAfterAnyItem
        );
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
        let request = ServerRequest::ChatgptAuthTokensRefresh {
            request_id: RequestId::String("req-1".to_string()),
            params: ChatgptAuthTokensRefreshParams {
                reason: ChatgptAuthTokensRefreshReason::Unauthorized,
                previous_account_id: Some("acct-1".to_string()),
            },
        };

        let error = unsupported_server_request_error(&request);

        assert_eq!(error.code, -32601);
        assert!(error.message.contains("account/chatgptAuthTokens/refresh"));
        assert!(
            error
                .message
                .contains("does not support interactive server request")
        );
        assert_eq!(
            server_request_method_name(&request),
            "account/chatgptAuthTokens/refresh"
        );
    }

    #[test]
    fn unsupported_server_requests_fail_loudly_after_rejection() {
        let request = ServerRequest::ChatgptAuthTokensRefresh {
            request_id: RequestId::String("req-1".to_string()),
            params: ChatgptAuthTokensRefreshParams {
                reason: ChatgptAuthTokensRefreshReason::Unauthorized,
                previous_account_id: Some("acct-1".to_string()),
            },
        };
        let error = unsupported_server_request_failure(&request);

        assert!(error.to_string().contains(
            "turn-start bridge does not support interactive server request `account/chatgptAuthTokens/refresh`"
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
