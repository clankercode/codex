/*
This module implements the stdio-backed app-server client transport.

It mirrors the websocket transport's responsibilities over newline-delimited
JSON-RPC carried on a spawned `codex app-server` child process. The transport
owns process startup, initialize/initialized handshake, request/response
routing, server request delivery, and graceful shutdown.
*/

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::AppServerEvent;
use crate::RequestResult;
use crate::SHUTDOWN_TIMEOUT;
use crate::TypedRequestError;
use crate::request_method_name;
use crate::server_notification_requires_delivery;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use serde::de::DeserializeOwned;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::warn;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct StdioAppServerConnectArgs {
    pub codex_bin: PathBuf,
    pub config_overrides: Vec<String>,
    pub client_name: String,
    pub client_version: String,
    pub experimental_api: bool,
    pub opt_out_notification_methods: Vec<String>,
    pub channel_capacity: usize,
}

impl StdioAppServerConnectArgs {
    fn initialize_params(&self) -> InitializeParams {
        let capabilities = InitializeCapabilities {
            experimental_api: self.experimental_api,
            opt_out_notification_methods: if self.opt_out_notification_methods.is_empty() {
                None
            } else {
                Some(self.opt_out_notification_methods.clone())
            },
        };

        InitializeParams {
            client_info: ClientInfo {
                name: self.client_name.clone(),
                title: None,
                version: self.client_version.clone(),
            },
            capabilities: Some(capabilities),
        }
    }
}

enum StdioClientCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<IoResult<RequestResult>>,
    },
    Notify {
        notification: ClientNotification,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    ResolveServerRequest {
        request_id: RequestId,
        result: JsonRpcResult,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    RejectServerRequest {
        request_id: RequestId,
        error: JSONRPCErrorError,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<IoResult<()>>,
    },
}

pub struct StdioAppServerClient {
    command_tx: mpsc::Sender<StdioClientCommand>,
    event_rx: mpsc::Receiver<AppServerEvent>,
    pending_events: VecDeque<AppServerEvent>,
    worker_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct StdioAppServerRequestHandle {
    command_tx: mpsc::Sender<StdioClientCommand>,
}

impl StdioAppServerClient {
    pub async fn connect(args: StdioAppServerConnectArgs) -> IoResult<Self> {
        let mut command = Command::new(&args.codex_bin);
        command.kill_on_drop(true);
        for override_kv in &args.config_overrides {
            command.arg("--config").arg(override_kv);
        }
        let mut child = command
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| {
                IoError::other(format!(
                    "failed to start `{}` app-server: {err}",
                    args.codex_bin.display()
                ))
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "spawned codex app-server stdin unavailable",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "spawned codex app-server stdout unavailable",
            )
        })?;

        Self::connect_with_io(
            stdout,
            stdin,
            args.initialize_params(),
            args.channel_capacity,
            format!("stdio app server `{}`", args.codex_bin.display()),
            Some(child),
        )
        .await
    }

    async fn connect_with_io<R, W>(
        read: R,
        mut write: W,
        initialize_params: InitializeParams,
        channel_capacity: usize,
        connection_label: String,
        child: Option<Child>,
    ) -> IoResult<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let channel_capacity = channel_capacity.max(1);
        let mut read = BufReader::new(read);
        let pending_events = initialize_stdio_connection(
            &mut read,
            &mut write,
            &connection_label,
            initialize_params,
            INITIALIZE_TIMEOUT,
        )
        .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<StdioClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut skipped_events = 0usize;
            let mut lines = read.lines();
            let mut child = child;

            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            drop(write);
                            let _ = shutdown_child(&mut child, &connection_label).await;
                            break;
                        };
                        match command {
                            StdioClientCommand::Request { request, response_tx } => {
                                let request_id = request_id_from_client_request(&request);
                                if pending_requests.contains_key(&request_id) {
                                    let _ = response_tx.send(Err(IoError::new(
                                        ErrorKind::InvalidInput,
                                        format!("duplicate stdio app-server request id `{request_id}`"),
                                    )));
                                    continue;
                                }
                                pending_requests.insert(request_id.clone(), response_tx);
                                if let Err(err) = write_jsonrpc_message(
                                    &mut write,
                                    JSONRPCMessage::Request(jsonrpc_request_from_client_request(*request)),
                                    &connection_label,
                                )
                                .await
                                {
                                    let err_message = err.to_string();
                                    if let Some(response_tx) = pending_requests.remove(&request_id) {
                                        let _ = response_tx.send(Err(err));
                                    }
                                    let _ = deliver_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: format!(
                                                "{connection_label} write failed: {err_message}"
                                            ),
                                        },
                                        &mut write,
                                        &connection_label,
                                    )
                                    .await;
                                    break;
                                }
                            }
                            StdioClientCommand::Notify { notification, response_tx } => {
                                let result = write_jsonrpc_message(
                                    &mut write,
                                    JSONRPCMessage::Notification(
                                        jsonrpc_notification_from_client_notification(notification),
                                    ),
                                    &connection_label,
                                )
                                .await;
                                let should_break = result.is_err();
                                let err_message = result.as_ref().err().map(ToString::to_string);
                                let _ = response_tx.send(result);
                                if should_break {
                                    let _ = deliver_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: format!(
                                                "{connection_label} write failed: {}",
                                                err_message.unwrap_or_else(|| "unknown write error".to_string())
                                            ),
                                        },
                                        &mut write,
                                        &connection_label,
                                    )
                                    .await;
                                    break;
                                }
                            }
                            StdioClientCommand::ResolveServerRequest { request_id, result, response_tx } => {
                                let result = write_jsonrpc_message(
                                    &mut write,
                                    JSONRPCMessage::Response(JSONRPCResponse { id: request_id, result }),
                                    &connection_label,
                                )
                                .await;
                                let should_break = result.is_err();
                                let err_message = result.as_ref().err().map(ToString::to_string);
                                let _ = response_tx.send(result);
                                if should_break {
                                    let _ = deliver_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: format!(
                                                "{connection_label} write failed: {}",
                                                err_message.unwrap_or_else(|| "unknown write error".to_string())
                                            ),
                                        },
                                        &mut write,
                                        &connection_label,
                                    )
                                    .await;
                                    break;
                                }
                            }
                            StdioClientCommand::RejectServerRequest { request_id, error, response_tx } => {
                                let result = write_jsonrpc_message(
                                    &mut write,
                                    JSONRPCMessage::Error(JSONRPCError { id: request_id, error }),
                                    &connection_label,
                                )
                                .await;
                                let should_break = result.is_err();
                                let err_message = result.as_ref().err().map(ToString::to_string);
                                let _ = response_tx.send(result);
                                if should_break {
                                    let _ = deliver_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: format!(
                                                "{connection_label} write failed: {}",
                                                err_message.unwrap_or_else(|| "unknown write error".to_string())
                                            ),
                                        },
                                        &mut write,
                                        &connection_label,
                                    )
                                    .await;
                                    break;
                                }
                            }
                            StdioClientCommand::Shutdown { response_tx } => {
                                drop(write);
                                let result = shutdown_child(&mut child, &connection_label).await;
                                let _ = response_tx.send(result);
                                break;
                            }
                        }
                    }
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                match serde_json::from_str::<JSONRPCMessage>(&line) {
                                    Ok(JSONRPCMessage::Response(response)) => {
                                        if let Some(response_tx) = pending_requests.remove(&response.id) {
                                            let _ = response_tx.send(Ok(Ok(response.result)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Error(error)) => {
                                        if let Some(response_tx) = pending_requests.remove(&error.id) {
                                            let _ = response_tx.send(Ok(Err(error.error)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Notification(notification)) => {
                                        if let Some(event) = app_server_event_from_notification(notification)
                                            && let Err(err) = deliver_event(
                                                &event_tx,
                                                &mut skipped_events,
                                                event,
                                                &mut write,
                                                &connection_label,
                                            )
                                            .await
                                        {
                                            warn!(%err, "failed to deliver stdio app-server event");
                                            break;
                                        }
                                    }
                                    Ok(JSONRPCMessage::Request(request)) => {
                                        let request_id = request.id.clone();
                                        let method = request.method.clone();
                                        match ServerRequest::try_from(request) {
                                            Ok(request) => {
                                                if let Err(err) = deliver_event(
                                                    &event_tx,
                                                    &mut skipped_events,
                                                    AppServerEvent::ServerRequest(request),
                                                    &mut write,
                                                    &connection_label,
                                                )
                                                .await
                                                {
                                                    warn!(%err, "failed to deliver stdio app-server server request");
                                                    break;
                                                }
                                            }
                                            Err(err) => {
                                                warn!(%err, method, "rejecting unknown stdio app-server request");
                                                if let Err(reject_err) = write_jsonrpc_message(
                                                    &mut write,
                                                    JSONRPCMessage::Error(JSONRPCError {
                                                        error: JSONRPCErrorError {
                                                            code: -32601,
                                                            message: format!(
                                                                "unsupported stdio app-server request `{method}`"
                                                            ),
                                                            data: None,
                                                        },
                                                        id: request_id,
                                                    }),
                                                    &connection_label,
                                                )
                                                .await
                                                {
                                                    let err_message = reject_err.to_string();
                                                    let _ = deliver_event(
                                                        &event_tx,
                                                        &mut skipped_events,
                                                        AppServerEvent::Disconnected {
                                                            message: format!(
                                                                "{connection_label} write failed: {err_message}"
                                                            ),
                                                        },
                                                        &mut write,
                                                        &connection_label,
                                                    )
                                                    .await;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        let _ = deliver_event(
                                            &event_tx,
                                            &mut skipped_events,
                                            AppServerEvent::Disconnected {
                                                message: format!(
                                                    "{connection_label} sent invalid JSON-RPC: {err}"
                                                ),
                                            },
                                            &mut write,
                                            &connection_label,
                                        )
                                        .await;
                                        break;
                                    }
                                }
                            }
                            Ok(None) => {
                                let _ = deliver_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: format!("{connection_label} closed the connection"),
                                    },
                                    &mut write,
                                    &connection_label,
                                )
                                .await;
                                break;
                            }
                            Err(err) => {
                                let _ = deliver_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: format!(
                                            "{connection_label} transport failed: {err}"
                                        ),
                                    },
                                    &mut write,
                                    &connection_label,
                                )
                                .await;
                                break;
                            }
                        }
                    }
                }
            }

            let err = IoError::new(
                ErrorKind::BrokenPipe,
                "stdio app-server worker channel is closed",
            );
            for (_, response_tx) in pending_requests {
                let _ = response_tx.send(Err(IoError::new(err.kind(), err.to_string())));
            }
        });

        Ok(Self {
            command_tx,
            event_rx,
            pending_events: pending_events.into(),
            worker_handle,
        })
    }

    pub fn request_handle(&self) -> StdioAppServerRequestHandle {
        StdioAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        send_stdio_command(
            &self.command_tx,
            |response_tx| StdioClientCommand::Request {
                request: Box::new(request),
                response_tx,
            },
            "stdio app-server worker channel is closed",
            "stdio app-server request channel is closed",
        )
        .await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request_method_name(&request);
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.clone(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.clone(),
            source,
        })?;
        serde_json::from_value(result)
            .map_err(|source| TypedRequestError::Deserialize { method, source })
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        send_stdio_command(
            &self.command_tx,
            |response_tx| StdioClientCommand::Notify {
                notification,
                response_tx,
            },
            "stdio app-server worker channel is closed",
            "stdio app-server notify channel is closed",
        )
        .await
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        send_stdio_command(
            &self.command_tx,
            |response_tx| StdioClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            },
            "stdio app-server worker channel is closed",
            "stdio app-server resolve channel is closed",
        )
        .await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        send_stdio_command(
            &self.command_tx,
            |response_tx| StdioClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            },
            "stdio app-server worker channel is closed",
            "stdio app-server reject channel is closed",
        )
        .await
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        if let Some(event) = self.pending_events.pop_front() {
            return Some(event);
        }
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            pending_events: _pending_events,
            worker_handle,
        } = self;
        let mut worker_handle = worker_handle;
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        if command_tx
            .send(StdioClientCommand::Shutdown { response_tx })
            .await
            .is_ok()
            && let Ok(command_result) = timeout(SHUTDOWN_TIMEOUT, response_rx).await
            && let Ok(command_result) = command_result
        {
            command_result?;
        }

        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, &mut worker_handle).await {
            worker_handle.abort();
            let _ = worker_handle.await;
        }
        Ok(())
    }
}

impl StdioAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        send_stdio_command(
            &self.command_tx,
            |response_tx| StdioClientCommand::Request {
                request: Box::new(request),
                response_tx,
            },
            "stdio app-server worker channel is closed",
            "stdio app-server request channel is closed",
        )
        .await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request_method_name(&request);
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.clone(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.clone(),
            source,
        })?;
        serde_json::from_value(result)
            .map_err(|source| TypedRequestError::Deserialize { method, source })
    }
}

async fn send_stdio_command<T, F>(
    command_tx: &mpsc::Sender<StdioClientCommand>,
    build_command: F,
    closed_worker_message: &'static str,
    closed_response_message: &'static str,
) -> IoResult<T>
where
    T: Send + 'static,
    F: FnOnce(oneshot::Sender<IoResult<T>>) -> StdioClientCommand,
{
    let (response_tx, response_rx) = oneshot::channel();
    command_tx
        .send(build_command(response_tx))
        .await
        .map_err(|_| IoError::new(ErrorKind::BrokenPipe, closed_worker_message))?;
    response_rx
        .await
        .map_err(|_| IoError::new(ErrorKind::BrokenPipe, closed_response_message))?
}

async fn initialize_stdio_connection<R, W>(
    read: &mut BufReader<R>,
    write: &mut W,
    connection_label: &str,
    params: InitializeParams,
    initialize_timeout: Duration,
) -> IoResult<Vec<AppServerEvent>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let initialize_request_id = RequestId::String("initialize".to_string());
    let mut pending_events = Vec::new();
    write_jsonrpc_message(
        write,
        JSONRPCMessage::Request(jsonrpc_request_from_client_request(
            ClientRequest::Initialize {
                request_id: initialize_request_id.clone(),
                params,
            },
        )),
        connection_label,
    )
    .await?;

    timeout(initialize_timeout, async {
        let mut lines = read.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let message = serde_json::from_str::<JSONRPCMessage>(&line).map_err(|err| {
                        IoError::other(format!(
                            "{connection_label} sent invalid initialize response: {err}"
                        ))
                    })?;
                    match message {
                        JSONRPCMessage::Response(response) if response.id == initialize_request_id => {
                            break Ok(());
                        }
                        JSONRPCMessage::Error(error) if error.id == initialize_request_id => {
                            break Err(IoError::other(format!(
                                "{connection_label} rejected initialize: {}",
                                error.error.message
                            )));
                        }
                        JSONRPCMessage::Notification(notification) => {
                            if let Some(event) = app_server_event_from_notification(notification) {
                                pending_events.push(event);
                            }
                        }
                        JSONRPCMessage::Request(request) => {
                            let request_id = request.id.clone();
                            let method = request.method.clone();
                            match ServerRequest::try_from(request) {
                                Ok(request) => {
                                    pending_events.push(AppServerEvent::ServerRequest(request));
                                }
                                Err(err) => {
                                    warn!(%err, method, "rejecting unknown stdio app-server request during initialize");
                                    write_jsonrpc_message(
                                        write,
                                        JSONRPCMessage::Error(JSONRPCError {
                                            error: JSONRPCErrorError {
                                                code: -32601,
                                                message: format!(
                                                    "unsupported stdio app-server request `{method}`"
                                                ),
                                                data: None,
                                            },
                                            id: request_id,
                                        }),
                                        connection_label,
                                    )
                                    .await?;
                                }
                            }
                        }
                        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
                    }
                }
                Ok(None) => {
                    break Err(IoError::new(
                        ErrorKind::UnexpectedEof,
                        format!("{connection_label} closed during initialize"),
                    ));
                }
                Err(err) => {
                    break Err(IoError::other(format!(
                        "{connection_label} transport failed during initialize: {err}"
                    )));
                }
            }
        }
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out waiting for initialize response from {connection_label}"),
        )
    })??;

    write_jsonrpc_message(
        write,
        JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
            ClientNotification::Initialized,
        )),
        connection_label,
    )
    .await?;

    Ok(pending_events)
}

fn app_server_event_from_notification(notification: JSONRPCNotification) -> Option<AppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(AppServerEvent::ServerNotification(notification)),
        Err(_) => None,
    }
}

async fn deliver_event<W>(
    event_tx: &mpsc::Sender<AppServerEvent>,
    skipped_events: &mut usize,
    event: AppServerEvent,
    write: &mut W,
    connection_label: &str,
) -> IoResult<()>
where
    W: AsyncWrite + Unpin,
{
    if *skipped_events > 0 {
        if event_requires_delivery(&event) {
            if event_tx
                .send(AppServerEvent::Lagged {
                    skipped: *skipped_events,
                })
                .await
                .is_err()
            {
                return Err(IoError::new(
                    ErrorKind::BrokenPipe,
                    "stdio app-server event consumer channel is closed",
                ));
            }
            *skipped_events = 0;
        } else {
            match event_tx.try_send(AppServerEvent::Lagged {
                skipped: *skipped_events,
            }) {
                Ok(()) => *skipped_events = 0,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    *skipped_events = (*skipped_events).saturating_add(1);
                    reject_if_server_request_dropped(write, &event, connection_label).await?;
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(IoError::new(
                        ErrorKind::BrokenPipe,
                        "stdio app-server event consumer channel is closed",
                    ));
                }
            }
        }
    }

    if event_requires_delivery(&event) {
        event_tx.send(event).await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "stdio app-server event consumer channel is closed",
            )
        })?;
        return Ok(());
    }

    match event_tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(event)) => {
            *skipped_events = (*skipped_events).saturating_add(1);
            reject_if_server_request_dropped(write, &event, connection_label).await
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(IoError::new(
            ErrorKind::BrokenPipe,
            "stdio app-server event consumer channel is closed",
        )),
    }
}

async fn reject_if_server_request_dropped<W>(
    write: &mut W,
    event: &AppServerEvent,
    connection_label: &str,
) -> IoResult<()>
where
    W: AsyncWrite + Unpin,
{
    let AppServerEvent::ServerRequest(request) = event else {
        return Ok(());
    };
    write_jsonrpc_message(
        write,
        JSONRPCMessage::Error(JSONRPCError {
            error: JSONRPCErrorError {
                code: -32001,
                message: "stdio app-server event queue is full".to_string(),
                data: None,
            },
            id: request.id().clone(),
        }),
        connection_label,
    )
    .await
}

fn event_requires_delivery(event: &AppServerEvent) -> bool {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        AppServerEvent::Disconnected { .. } => true,
        AppServerEvent::Lagged { .. } | AppServerEvent::ServerRequest(_) => false,
    }
}

fn request_id_from_client_request(request: &ClientRequest) -> RequestId {
    jsonrpc_request_from_client_request(request.clone()).id
}

fn jsonrpc_request_from_client_request(request: ClientRequest) -> JSONRPCRequest {
    let value = match serde_json::to_value(request) {
        Ok(value) => value,
        Err(err) => panic!("client request should serialize: {err}"),
    };
    match serde_json::from_value(value) {
        Ok(request) => request,
        Err(err) => panic!("client request should encode as JSON-RPC request: {err}"),
    }
}

fn jsonrpc_notification_from_client_notification(
    notification: ClientNotification,
) -> JSONRPCNotification {
    let value = match serde_json::to_value(notification) {
        Ok(value) => value,
        Err(err) => panic!("client notification should serialize: {err}"),
    };
    match serde_json::from_value(value) {
        Ok(notification) => notification,
        Err(err) => panic!("client notification should encode as JSON-RPC notification: {err}"),
    }
}

async fn write_jsonrpc_message<W>(
    write: &mut W,
    message: JSONRPCMessage,
    connection_label: &str,
) -> IoResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut payload = serde_json::to_string(&message).map_err(IoError::other)?;
    payload.push('\n');
    write.write_all(payload.as_bytes()).await.map_err(|err| {
        IoError::other(format!(
            "failed to write stdio message to {connection_label}: {err}"
        ))
    })?;
    write.flush().await.map_err(|err| {
        IoError::other(format!(
            "failed to flush stdio message to {connection_label}: {err}"
        ))
    })
}

async fn shutdown_child(child: &mut Option<Child>, connection_label: &str) -> IoResult<()> {
    let Some(child) = child.as_mut() else {
        return Ok(());
    };
    match timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(err)) => Err(IoError::other(format!(
            "failed waiting for {connection_label} shutdown: {err}"
        ))),
        Err(_) => {
            child.kill().await.map_err(|err| {
                IoError::other(format!(
                    "failed killing {connection_label} after shutdown timeout: {err}"
                ))
            })?;
            child.wait().await.map_err(|err| {
                IoError::other(format!(
                    "failed waiting for killed {connection_label}: {err}"
                ))
            })?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
    use codex_app_server_protocol::GetAccountParams;
    use codex_app_server_protocol::GetAccountResponse;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::ThreadItem;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnStatus;
    use pretty_assertions::assert_eq;
    use tokio::io::duplex;
    use tokio::io::split;
    use tokio::time::Duration;
    use tokio::time::timeout;

    async fn connect_test_client() -> (
        StdioAppServerClient,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        connect_test_client_with_capacity(8).await
    }

    async fn connect_test_client_with_capacity(
        channel_capacity: usize,
    ) -> (
        StdioAppServerClient,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client_stream, server_stream) = duplex(8192);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let connect = tokio::spawn(async move {
            StdioAppServerClient::connect_with_io(
                client_read,
                client_write,
                InitializeParams {
                    client_info: ClientInfo {
                        name: "codex-app-server-client-test".to_string(),
                        title: None,
                        version: "0.0.0-test".to_string(),
                    },
                    capabilities: Some(InitializeCapabilities {
                        experimental_api: true,
                        opt_out_notification_methods: None,
                    }),
                },
                channel_capacity,
                "test stdio app server".to_string(),
                None,
            )
            .await
        });

        let mut server_read = BufReader::new(server_read);
        let initialize = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Request(request) = initialize else {
            panic!("expected initialize request");
        };
        assert_eq!(request.method, "initialize");

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({
                    "userAgent": "codex-test",
                    "codexHome": "/tmp/codex-home",
                    "platformFamily": "unix",
                    "platformOs": "linux"
                }),
            }),
            "test stdio app server",
        )
        .await
        .expect("initialize response should write");

        let initialized = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Notification(notification) = initialized else {
            panic!("expected initialized notification");
        };
        assert_eq!(notification.method, "initialized");

        let client = connect
            .await
            .expect("connect task should join")
            .expect("client should connect");
        (client, server_read, server_write)
    }

    async fn read_jsonrpc_message<R>(read: &mut BufReader<R>) -> JSONRPCMessage
    where
        R: AsyncRead + Unpin,
    {
        let mut line = String::new();
        read.read_line(&mut line)
            .await
            .expect("line should read successfully");
        serde_json::from_str(&line).expect("line should decode as JSON-RPC")
    }

    fn command_execution_output_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::CommandExecutionOutputDelta(CommandExecutionOutputDeltaNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            item_id: "item".to_string(),
            delta: delta.to_string(),
        })
    }

    fn agent_message_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                item_id: "item".to_string(),
                delta: delta.to_string(),
            },
        )
    }

    fn item_completed_notification(text: &str) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            item: ThreadItem::AgentMessage {
                id: "item".to_string(),
                text: text.to_string(),
                phase: None,
                memory_citation: None,
            },
        })
    }

    fn turn_completed_notification() -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread".to_string(),
            turn: Turn {
                id: "turn".to_string(),
                items: Vec::new(),
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: Some(1),
            },
        })
    }

    #[tokio::test]
    async fn initialize_waits_for_response_before_initialized_and_buffers_events() {
        let (client_stream, server_stream) = duplex(8192);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let connect = tokio::spawn(async move {
            StdioAppServerClient::connect_with_io(
                client_read,
                client_write,
                InitializeParams {
                    client_info: ClientInfo {
                        name: "codex-app-server-client-test".to_string(),
                        title: None,
                        version: "0.0.0-test".to_string(),
                    },
                    capabilities: Some(InitializeCapabilities {
                        experimental_api: true,
                        opt_out_notification_methods: None,
                    }),
                },
                8,
                "test stdio app server".to_string(),
                None,
            )
            .await
        });
        let mut server_read = BufReader::new(server_read);

        let initialize = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Request(request) = initialize else {
            panic!("expected initialize request");
        };
        assert_eq!(request.method, "initialize");

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Notification(
                serde_json::from_value(
                    serde_json::to_value(ServerNotification::AccountUpdated(
                        AccountUpdatedNotification {
                            auth_mode: None,
                            plan_type: None,
                        },
                    ))
                    .expect("notification should serialize"),
                )
                .expect("notification should convert to JSON-RPC"),
            ),
            "test stdio app server",
        )
        .await
        .expect("notification should write");

        assert!(
            timeout(
                Duration::from_millis(100),
                read_jsonrpc_message(&mut server_read)
            )
            .await
            .is_err(),
            "initialized should not be sent before initialize response"
        );

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({
                    "userAgent": "codex-test",
                    "codexHome": "/tmp/codex-home",
                    "platformFamily": "unix",
                    "platformOs": "linux"
                }),
            }),
            "test stdio app server",
        )
        .await
        .expect("initialize response should write");

        let initialized = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Notification(notification) = initialized else {
            panic!("expected initialized notification");
        };
        assert_eq!(notification.method, "initialized");

        let mut client = connect
            .await
            .expect("connect task should join")
            .expect("client should connect");
        let event = client
            .next_event()
            .await
            .expect("pending event should arrive");
        assert!(matches!(
            event,
            AppServerEvent::ServerNotification(ServerNotification::AccountUpdated(_))
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_server_request_received_during_initialize_is_delivered() {
        let (client_stream, server_stream) = duplex(8192);
        let (client_read, client_write) = split(client_stream);
        let (server_read, mut server_write) = split(server_stream);
        let connect = tokio::spawn(async move {
            StdioAppServerClient::connect_with_io(
                client_read,
                client_write,
                InitializeParams {
                    client_info: ClientInfo {
                        name: "codex-app-server-client-test".to_string(),
                        title: None,
                        version: "0.0.0-test".to_string(),
                    },
                    capabilities: Some(InitializeCapabilities {
                        experimental_api: true,
                        opt_out_notification_methods: None,
                    }),
                },
                8,
                "test stdio app server".to_string(),
                None,
            )
            .await
        });
        let mut server_read = BufReader::new(server_read);

        let initialize = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Request(request) = initialize else {
            panic!("expected initialize request");
        };
        assert_eq!(request.method, "initialize");

        let request_id = RequestId::String("srv-init".to_string());
        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id.clone(),
                method: "item/tool/requestUserInput".to_string(),
                params: Some(
                    serde_json::to_value(ToolRequestUserInputParams {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: "call-1".to_string(),
                        questions: vec![ToolRequestUserInputQuestion {
                            id: "question-1".to_string(),
                            header: "Mode".to_string(),
                            question: "Pick one".to_string(),
                            is_other: false,
                            is_secret: false,
                            options: Some(vec![]),
                        }],
                    })
                    .expect("params should serialize"),
                ),
                trace: None,
            }),
            "test stdio app server",
        )
        .await
        .expect("server request should write");
        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({
                    "userAgent": "codex-test",
                    "codexHome": "/tmp/codex-home",
                    "platformFamily": "unix",
                    "platformOs": "linux"
                }),
            }),
            "test stdio app server",
        )
        .await
        .expect("initialize response should write");

        let initialized = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Notification(notification) = initialized else {
            panic!("expected initialized notification");
        };
        assert_eq!(notification.method, "initialized");

        let mut client = connect
            .await
            .expect("connect task should join")
            .expect("client should connect");
        let AppServerEvent::ServerRequest(request) = client
            .next_event()
            .await
            .expect("request event should arrive")
        else {
            panic!("expected server request event");
        };
        assert_eq!(request.id(), &request_id);

        client
            .resolve_server_request(request.id().clone(), serde_json::json!({}))
            .await
            .expect("server request should resolve");

        let JSONRPCMessage::Response(response) = read_jsonrpc_message(&mut server_read).await
        else {
            panic!("expected server request response");
        };
        assert_eq!(response.id, request_id);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_request_response_routing_uses_request_id() {
        let (client, mut server_read, mut server_write) = connect_test_client().await;
        let first_handle = client.request_handle();
        let second_handle = first_handle.clone();

        let first_task = tokio::spawn(async move {
            first_handle
                .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(1),
                    params: GetAccountParams {
                        refresh_token: false,
                    },
                })
                .await
        });
        let second_task = tokio::spawn(async move {
            second_handle
                .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(2),
                    params: GetAccountParams {
                        refresh_token: true,
                    },
                })
                .await
        });

        let first_request = read_jsonrpc_message(&mut server_read).await;
        let second_request = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Request(first_request) = first_request else {
            panic!("expected first request");
        };
        let JSONRPCMessage::Request(second_request) = second_request else {
            panic!("expected second request");
        };
        assert_eq!(first_request.method, "account/read");
        assert_eq!(second_request.method, "account/read");

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: second_request.id.clone(),
                result: serde_json::to_value(GetAccountResponse {
                    account: None,
                    requires_openai_auth: true,
                })
                .expect("response should serialize"),
            }),
            "test stdio app server",
        )
        .await
        .expect("second response should write");
        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: first_request.id.clone(),
                result: serde_json::to_value(GetAccountResponse {
                    account: None,
                    requires_openai_auth: false,
                })
                .expect("response should serialize"),
            }),
            "test stdio app server",
        )
        .await
        .expect("first response should write");

        assert_eq!(
            first_task
                .await
                .expect("first task should join")
                .expect("first request should succeed"),
            GetAccountResponse {
                account: None,
                requires_openai_auth: false,
            }
        );
        assert_eq!(
            second_task
                .await
                .expect("second task should join")
                .expect("second request should succeed"),
            GetAccountResponse {
                account: None,
                requires_openai_auth: true,
            }
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_duplicate_request_id_rejection_keeps_original_waiter() {
        let (client, mut server_read, mut server_write) = connect_test_client().await;
        let first_request_handle = client.request_handle();
        let second_request_handle = first_request_handle.clone();

        let first_request = tokio::spawn(async move {
            first_request_handle
                .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(1),
                    params: GetAccountParams {
                        refresh_token: false,
                    },
                })
                .await
        });

        let JSONRPCMessage::Request(first_request_message) =
            read_jsonrpc_message(&mut server_read).await
        else {
            panic!("expected first request");
        };
        assert_eq!(first_request_message.id, RequestId::Integer(1));
        assert_eq!(first_request_message.method, "account/read");

        let second_err = second_request_handle
            .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                request_id: RequestId::Integer(1),
                params: GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .expect_err("duplicate request id should be rejected");
        assert_eq!(
            second_err.to_string(),
            "account/read transport error: duplicate stdio app-server request id `1`"
        );

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: first_request_message.id,
                result: serde_json::to_value(GetAccountResponse {
                    account: None,
                    requires_openai_auth: false,
                })
                .expect("response should serialize"),
            }),
            "test stdio app server",
        )
        .await
        .expect("first response should write");

        assert_eq!(
            first_request
                .await
                .expect("first request task should join")
                .expect("first request should succeed"),
            GetAccountResponse {
                account: None,
                requires_openai_auth: false,
            }
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_notifications_and_server_requests_arrive_on_event_stream() {
        let (mut client, mut server_read, mut server_write) = connect_test_client().await;

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Notification(
                serde_json::from_value(
                    serde_json::to_value(ServerNotification::AccountUpdated(
                        AccountUpdatedNotification {
                            auth_mode: None,
                            plan_type: None,
                        },
                    ))
                    .expect("notification should serialize"),
                )
                .expect("notification should convert to JSON-RPC"),
            ),
            "test stdio app server",
        )
        .await
        .expect("notification should write");

        let event = client
            .next_event()
            .await
            .expect("notification event should arrive");
        assert!(matches!(
            event,
            AppServerEvent::ServerNotification(ServerNotification::AccountUpdated(_))
        ));

        let request_id = RequestId::String("srv-1".to_string());
        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id.clone(),
                method: "item/tool/requestUserInput".to_string(),
                params: Some(
                    serde_json::to_value(ToolRequestUserInputParams {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: "call-1".to_string(),
                        questions: vec![ToolRequestUserInputQuestion {
                            id: "question-1".to_string(),
                            header: "Mode".to_string(),
                            question: "Pick one".to_string(),
                            is_other: false,
                            is_secret: false,
                            options: Some(vec![]),
                        }],
                    })
                    .expect("params should serialize"),
                ),
                trace: None,
            }),
            "test stdio app server",
        )
        .await
        .expect("server request should write");

        let AppServerEvent::ServerRequest(request) = client
            .next_event()
            .await
            .expect("server request event should arrive")
        else {
            panic!("expected server request event");
        };
        assert_eq!(request.id(), &request_id);

        client
            .resolve_server_request(request.id().clone(), serde_json::json!({}))
            .await
            .expect("server request should resolve");

        let response = read_jsonrpc_message(&mut server_read).await;
        let JSONRPCMessage::Response(response) = response else {
            panic!("expected server request response");
        };
        assert_eq!(response.id, request_id);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_unknown_server_request_is_rejected() {
        let (client, mut server_read, mut server_write) = connect_test_client().await;
        let request_id = RequestId::String("srv-unknown".to_string());

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id.clone(),
                method: "thread/unknown".to_string(),
                params: None,
                trace: None,
            }),
            "test stdio app server",
        )
        .await
        .expect("unknown request should write");

        let JSONRPCMessage::Error(response) = read_jsonrpc_message(&mut server_read).await else {
            panic!("expected JSON-RPC error response");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(response.error.code, -32601);
        assert_eq!(
            response.error.message,
            "unsupported stdio app-server request `thread/unknown`"
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_explicit_reject_server_request_roundtrip_works() {
        let (mut client, mut server_read, mut server_write) = connect_test_client().await;
        let request_id = RequestId::String("srv-reject".to_string());

        write_jsonrpc_message(
            &mut server_write,
            JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id.clone(),
                method: "item/tool/requestUserInput".to_string(),
                params: Some(
                    serde_json::to_value(ToolRequestUserInputParams {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: "call-1".to_string(),
                        questions: vec![ToolRequestUserInputQuestion {
                            id: "question-1".to_string(),
                            header: "Mode".to_string(),
                            question: "Pick one".to_string(),
                            is_other: false,
                            is_secret: false,
                            options: Some(vec![]),
                        }],
                    })
                    .expect("params should serialize"),
                ),
                trace: None,
            }),
            "test stdio app server",
        )
        .await
        .expect("server request should write");

        let AppServerEvent::ServerRequest(request) = client
            .next_event()
            .await
            .expect("request event should arrive")
        else {
            panic!("expected server request event");
        };
        let error = JSONRPCErrorError {
            code: -32042,
            message: "request rejected for test".to_string(),
            data: Some(serde_json::json!({"reason": "declined"})),
        };
        client
            .reject_server_request(request.id().clone(), error.clone())
            .await
            .expect("server request should reject");

        let JSONRPCMessage::Error(response) = read_jsonrpc_message(&mut server_read).await else {
            panic!("expected server request rejection");
        };
        assert_eq!(response.id, request_id);
        assert_eq!(response.error, error);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn stdio_disconnect_surfaces_as_event() {
        let (mut client, server_read, server_write) = connect_test_client().await;
        drop(server_read);
        drop(server_write);

        let event = client
            .next_event()
            .await
            .expect("disconnect event should arrive");
        assert!(matches!(event, AppServerEvent::Disconnected { .. }));
    }

    #[tokio::test]
    async fn stdio_backpressure_preserves_transcript_notifications() {
        let (mut client, _server_read, mut server_write) =
            connect_test_client_with_capacity(1).await;

        for notification in [
            command_execution_output_delta_notification("stdout-1"),
            command_execution_output_delta_notification("stdout-2"),
            agent_message_delta_notification("hello"),
            item_completed_notification("hello"),
            turn_completed_notification(),
        ] {
            write_jsonrpc_message(
                &mut server_write,
                JSONRPCMessage::Notification(
                    serde_json::from_value(
                        serde_json::to_value(notification).expect("notification should serialize"),
                    )
                    .expect("notification should convert to JSON-RPC"),
                ),
                "test stdio app server",
            )
            .await
            .expect("notification should write");
        }

        let first_event = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("first event should arrive before timeout")
            .expect("event stream should stay open");
        assert!(matches!(
            first_event,
            AppServerEvent::ServerNotification(ServerNotification::CommandExecutionOutputDelta(
                notification
            )) if notification.delta == "stdout-1"
        ));

        let mut remaining_events = Vec::new();
        for _ in 0..4 {
            remaining_events.push(
                timeout(Duration::from_secs(2), client.next_event())
                    .await
                    .expect("event should arrive before timeout")
                    .expect("event stream should stay open"),
            );
        }

        let mut transcript_event_names = Vec::new();
        for event in &remaining_events {
            match event {
                AppServerEvent::Lagged { skipped: 1 } => {}
                AppServerEvent::ServerNotification(
                    ServerNotification::CommandExecutionOutputDelta(notification),
                ) if notification.delta == "stdout-2" => {}
                AppServerEvent::ServerNotification(ServerNotification::AgentMessageDelta(
                    notification,
                )) if notification.delta == "hello" => {
                    transcript_event_names.push("agent_message_delta");
                }
                AppServerEvent::ServerNotification(ServerNotification::ItemCompleted(
                    notification,
                )) if matches!(
                    &notification.item,
                    ThreadItem::AgentMessage { text, .. } if text == "hello"
                ) =>
                {
                    transcript_event_names.push("item_completed");
                }
                AppServerEvent::ServerNotification(ServerNotification::TurnCompleted(
                    notification,
                )) if notification.turn.status == TurnStatus::Completed => {
                    transcript_event_names.push("turn_completed");
                }
                _ => panic!("unexpected remaining event: {event:?}"),
            }
        }
        assert_eq!(
            transcript_event_names,
            vec!["agent_message_delta", "item_completed", "turn_completed"]
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[test]
    fn event_requires_delivery_marks_transcript_and_disconnect_events() {
        assert!(event_requires_delivery(
            &AppServerEvent::ServerNotification(ServerNotification::AgentMessageDelta(
                codex_app_server_protocol::AgentMessageDeltaNotification {
                    thread_id: "thread".to_string(),
                    turn_id: "turn".to_string(),
                    item_id: "item".to_string(),
                    delta: "hello".to_string(),
                },
            ))
        ));
        assert!(event_requires_delivery(&AppServerEvent::Disconnected {
            message: "closed".to_string(),
        }));
        assert!(!event_requires_delivery(&AppServerEvent::Lagged {
            skipped: 1,
        }));
    }
}
