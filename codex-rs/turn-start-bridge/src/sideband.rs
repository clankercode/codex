use std::collections::HashMap;

use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::McpServerElicitationRequestResponse;
use codex_app_server_protocol::PermissionsRequestApprovalResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::LineWriter;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ThreadResolvedPayload {
    pub(crate) thread_id: String,
    pub(crate) source: String,
}

impl ThreadResolvedPayload {
    pub(crate) fn started(thread_id: &str) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            source: "started".to_string(),
        }
    }

    pub(crate) fn resumed(thread_id: &str) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            source: "resumed".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedServerRequestKind {
    CommandExecutionApproval,
    FileChangeApproval,
    PermissionsApproval,
    McpElicitation,
    ToolUserInput,
    LegacyExecCommandApproval,
    LegacyApplyPatchApproval,
}

impl ManagedServerRequestKind {
    fn request_kind(self) -> &'static str {
        match self {
            Self::CommandExecutionApproval | Self::LegacyExecCommandApproval => {
                "command_execution_approval_request"
            }
            Self::FileChangeApproval | Self::LegacyApplyPatchApproval => {
                "file_change_approval_request"
            }
            Self::PermissionsApproval => "permissions_approval_request",
            Self::McpElicitation => "mcp_elicitation_request",
            Self::ToolUserInput => "tool_user_input_request",
        }
    }

    fn response_kind(self) -> &'static str {
        match self {
            Self::CommandExecutionApproval | Self::LegacyExecCommandApproval => {
                "command_execution_approval_response"
            }
            Self::FileChangeApproval | Self::LegacyApplyPatchApproval => {
                "file_change_approval_response"
            }
            Self::PermissionsApproval => "permissions_approval_response",
            Self::McpElicitation => "mcp_elicitation_response",
            Self::ToolUserInput => "tool_user_input_response",
        }
    }

    fn default_resolution_on_input_closed(self) -> Result<ServerRequestResolution> {
        match self {
            Self::CommandExecutionApproval => Ok(ServerRequestResolution::Resolve(
                serde_json::to_value(CommandExecutionRequestApprovalResponse {
                    decision: CommandExecutionApprovalDecision::Decline,
                })?,
            )),
            Self::FileChangeApproval => Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                FileChangeRequestApprovalResponse {
                    decision: FileChangeApprovalDecision::Decline,
                },
            )?)),
            Self::PermissionsApproval => Ok(ServerRequestResolution::Reject(
                sideband_unavailable_error("permissions approval response channel closed"),
            )),
            Self::McpElicitation => Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                McpServerElicitationRequestResponse {
                    action: McpServerElicitationAction::Cancel,
                    content: None,
                    meta: None,
                },
            )?)),
            Self::ToolUserInput => Ok(ServerRequestResolution::Reject(sideband_unavailable_error(
                "tool input response channel closed",
            ))),
            Self::LegacyExecCommandApproval => Ok(ServerRequestResolution::Resolve(
                serde_json::json!({ "decision": "denied" }),
            )),
            Self::LegacyApplyPatchApproval => Ok(ServerRequestResolution::Resolve(
                serde_json::json!({ "decision": "denied" }),
            )),
        }
    }

    fn parse_response(self, response: SidebandResponseEnvelope) -> Result<ServerRequestResolution> {
        if response.kind != self.response_kind() {
            anyhow::bail!(
                "response kind mismatch for request {:?}: expected `{}`, got `{}`",
                response.request_id,
                self.response_kind(),
                response.kind
            );
        }

        match self {
            Self::CommandExecutionApproval => {
                let response: CommandExecutionRequestApprovalResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                    response,
                )?))
            }
            Self::FileChangeApproval => {
                let response: FileChangeRequestApprovalResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                    response,
                )?))
            }
            Self::PermissionsApproval => {
                let response: PermissionsRequestApprovalResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                    response,
                )?))
            }
            Self::McpElicitation => {
                let response: McpServerElicitationRequestResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                    response,
                )?))
            }
            Self::ToolUserInput => {
                let response: ToolRequestUserInputResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(serde_json::to_value(
                    response,
                )?))
            }
            Self::LegacyExecCommandApproval => {
                let response: CommandExecutionRequestApprovalResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(
                    legacy_exec_command_response_value(response.decision)?,
                ))
            }
            Self::LegacyApplyPatchApproval => {
                let response: FileChangeRequestApprovalResponse =
                    serde_json::from_value(response.response)?;
                Ok(ServerRequestResolution::Resolve(
                    legacy_apply_patch_response_value(response.decision),
                ))
            }
        }
    }
}

fn legacy_exec_command_response_value(decision: CommandExecutionApprovalDecision) -> Result<Value> {
    let decision = match decision {
        CommandExecutionApprovalDecision::Accept => Value::String("approved".to_string()),
        CommandExecutionApprovalDecision::AcceptForSession => {
            Value::String("approved_for_session".to_string())
        }
        CommandExecutionApprovalDecision::Decline => Value::String("denied".to_string()),
        CommandExecutionApprovalDecision::Cancel => Value::String("abort".to_string()),
        CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment { .. }
        | CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment { .. } => {
            anyhow::bail!("legacy exec approvals do not support advanced approval decisions")
        }
    };

    Ok(serde_json::json!({ "decision": decision }))
}

fn legacy_apply_patch_response_value(decision: FileChangeApprovalDecision) -> Value {
    let decision = match decision {
        FileChangeApprovalDecision::Accept => "approved",
        FileChangeApprovalDecision::AcceptForSession => "approved_for_session",
        FileChangeApprovalDecision::Decline => "denied",
        FileChangeApprovalDecision::Cancel => "abort",
    };

    serde_json::json!({ "decision": decision })
}

fn sideband_unavailable_error(message: &str) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32000,
        message: format!("turn-start bridge sideband unavailable: {message}"),
        data: None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ManagedServerRequest {
    kind: ManagedServerRequestKind,
    request_id: RequestId,
    thread_id: Option<String>,
    turn_id: Option<String>,
    item_id: Option<String>,
    server_name: Option<String>,
    raw_payload: Value,
}

impl ManagedServerRequest {
    pub(crate) fn kind(&self) -> &'static str {
        self.kind.request_kind()
    }

    pub(crate) fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[cfg(test)]
    pub(crate) fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn raw_payload(&self) -> &Value {
        &self.raw_payload
    }

    pub(crate) fn event(&self) -> ManagedServerRequestEvent<'_> {
        ManagedServerRequestEvent {
            kind: self.kind.request_kind(),
            request_id: &self.request_id,
            thread_id: self.thread_id.as_deref(),
            turn_id: self.turn_id.as_deref(),
            item_id: self.item_id.as_deref(),
            server_name: self.server_name.as_deref(),
            raw: &self.raw_payload,
        }
    }

    pub(crate) fn parse_response(
        &self,
        response: SidebandResponseEnvelope,
    ) -> Result<ServerRequestResolution> {
        self.kind.parse_response(response)
    }

    pub(crate) fn default_resolution_on_input_closed(&self) -> Result<ServerRequestResolution> {
        self.kind.default_resolution_on_input_closed()
    }
}

fn serialize_request_payload<T>(params: &T) -> Value
where
    T: Serialize,
{
    match serde_json::to_value(params) {
        Ok(value) => value,
        Err(err) => serde_json::json!({
            "serialization_error": err.to_string(),
        }),
    }
}

impl TryFrom<&ServerRequest> for ManagedServerRequest {
    type Error = ();

    fn try_from(request: &ServerRequest) -> std::result::Result<Self, Self::Error> {
        match request {
            ServerRequest::CommandExecutionRequestApproval { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::CommandExecutionApproval,
                request_id: request_id.clone(),
                thread_id: Some(params.thread_id.clone()),
                turn_id: Some(params.turn_id.clone()),
                item_id: Some(params.item_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::FileChangeRequestApproval { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::FileChangeApproval,
                request_id: request_id.clone(),
                thread_id: Some(params.thread_id.clone()),
                turn_id: Some(params.turn_id.clone()),
                item_id: Some(params.item_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::PermissionsRequestApproval { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::PermissionsApproval,
                request_id: request_id.clone(),
                thread_id: Some(params.thread_id.clone()),
                turn_id: Some(params.turn_id.clone()),
                item_id: Some(params.item_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::McpServerElicitationRequest { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::McpElicitation,
                request_id: request_id.clone(),
                thread_id: Some(params.thread_id.clone()),
                turn_id: params.turn_id.clone(),
                item_id: None,
                server_name: Some(params.server_name.clone()),
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::ToolRequestUserInput { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::ToolUserInput,
                request_id: request_id.clone(),
                thread_id: Some(params.thread_id.clone()),
                turn_id: Some(params.turn_id.clone()),
                item_id: Some(params.item_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::ExecCommandApproval { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::LegacyExecCommandApproval,
                request_id: request_id.clone(),
                thread_id: Some(params.conversation_id.to_string()),
                turn_id: None,
                item_id: Some(params.call_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            ServerRequest::ApplyPatchApproval { request_id, params } => Ok(Self {
                kind: ManagedServerRequestKind::LegacyApplyPatchApproval,
                request_id: request_id.clone(),
                thread_id: Some(params.conversation_id.to_string()),
                turn_id: None,
                item_id: Some(params.call_id.clone()),
                server_name: None,
                raw_payload: serialize_request_payload(params),
            }),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ManagedServerRequestEvent<'a> {
    kind: &'static str,
    request_id: &'a RequestId,
    thread_id: Option<&'a str>,
    turn_id: Option<&'a str>,
    item_id: Option<&'a str>,
    server_name: Option<&'a str>,
    raw: &'a Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct ControlThreadResolvedEvent<'a> {
    kind: &'static str,
    thread_id: &'a str,
    source: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct SidebandResponseEnvelope {
    pub(crate) kind: String,
    pub(crate) request_id: RequestId,
    pub(crate) response: Value,
}

#[derive(Debug)]
pub(crate) enum SidebandResponseEvent {
    Response(SidebandResponseEnvelope),
    ParseError(String),
    ReadError(String),
    Closed,
}

#[derive(Debug)]
pub(crate) enum ServerRequestResolution {
    Resolve(Value),
    Reject(JSONRPCErrorError),
}

#[cfg(unix)]
#[derive(Clone)]
struct JsonLineSink {
    writer: Arc<Mutex<LineWriter<File>>>,
}

#[cfg(unix)]
impl JsonLineSink {
    fn from_fd(fd: i32) -> Result<Self> {
        if fd < 0 {
            anyhow::bail!("invalid fd value `{fd}`");
        }

        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Self {
            writer: Arc::new(Mutex::new(LineWriter::new(file))),
        })
    }

    fn write_json<T>(&self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("sideband sink lock poisoned"))?;
        serde_json::to_writer(&mut *writer, value).context("serialize sideband JSON")?;
        writer.write_all(b"\n").context("write sideband newline")?;
        writer.flush().context("flush sideband output")?;
        Ok(())
    }
}

pub(crate) struct SidebandOutputs {
    #[cfg(unix)]
    thread_id_sinks: Vec<JsonLineSink>,
    #[cfg(unix)]
    request_event_sinks: Vec<JsonLineSink>,
    #[cfg(unix)]
    control_event_sinks: Vec<JsonLineSink>,
}

impl SidebandOutputs {
    #[cfg(unix)]
    pub(crate) fn from_fds(
        thread_id_fd: Option<i32>,
        server_request_events_fd: Option<i32>,
        control_events_fd: Option<i32>,
    ) -> Result<Self> {
        validate_unique_fds(&[
            ("thread-id-fd", thread_id_fd),
            ("server-request-events-fd", server_request_events_fd),
            ("control-events-fd", control_events_fd),
        ])?;

        let mut thread_id_sinks = Vec::new();
        let mut request_event_sinks = Vec::new();
        let mut control_event_sinks = Vec::new();

        if let Some(fd) = thread_id_fd {
            thread_id_sinks.push(JsonLineSink::from_fd(fd)?);
        }
        if let Some(fd) = server_request_events_fd {
            request_event_sinks.push(JsonLineSink::from_fd(fd)?);
        }
        if let Some(fd) = control_events_fd {
            let sink = JsonLineSink::from_fd(fd)?;
            request_event_sinks.push(sink.clone());
            control_event_sinks.push(sink);
        }

        Ok(Self {
            thread_id_sinks,
            request_event_sinks,
            control_event_sinks,
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn from_fds(
        thread_id_fd: Option<i32>,
        server_request_events_fd: Option<i32>,
        control_events_fd: Option<i32>,
    ) -> Result<Self> {
        let _ = (thread_id_fd, server_request_events_fd, control_events_fd);
        Ok(Self {})
    }

    pub(crate) fn has_request_event_sink(&self) -> bool {
        #[cfg(unix)]
        {
            !self.request_event_sinks.is_empty()
        }

        #[cfg(not(unix))]
        {
            false
        }
    }

    pub(crate) fn emit_thread_resolved(&self, payload: &ThreadResolvedPayload) -> Result<()> {
        #[cfg(unix)]
        {
            for sink in &self.thread_id_sinks {
                sink.write_json(payload)?;
            }
            let control_event = ControlThreadResolvedEvent {
                kind: "thread_resolved",
                thread_id: &payload.thread_id,
                source: &payload.source,
            };
            for sink in &self.control_event_sinks {
                sink.write_json(&control_event)?;
            }
        }

        Ok(())
    }

    pub(crate) fn emit_request(&self, request: &ManagedServerRequest) -> Result<()> {
        #[cfg(unix)]
        {
            let event = request.event();
            for sink in &self.request_event_sinks {
                sink.write_json(&event)?;
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
fn validate_unique_fds(entries: &[(&str, Option<i32>)]) -> Result<()> {
    let mut seen = HashMap::<i32, &str>::new();
    for (name, fd) in entries {
        if let Some(fd) = fd
            && let Some(previous) = seen.insert(*fd, name) {
                anyhow::bail!(
                    "fd `{fd}` is configured for both `--{previous}` and `--{name}`; use a single control lane instead of reusing the same fd"
                );
            }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn spawn_response_reader(
    fd: i32,
) -> Result<mpsc::UnboundedReceiver<SidebandResponseEvent>> {
    if fd < 0 {
        anyhow::bail!("invalid fd value `{fd}`");
    }

    let file = unsafe { File::from_raw_fd(fd) };
    let file = tokio::fs::File::from_std(file);
    let reader = tokio::io::BufReader::new(file);
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<SidebandResponseEnvelope>(&line) {
                        Ok(response) => {
                            let _ = tx.send(SidebandResponseEvent::Response(response));
                        }
                        Err(err) => {
                            let _ = tx.send(SidebandResponseEvent::ParseError(format!(
                                "invalid sideband response JSON: {err}"
                            )));
                        }
                    }
                }
                Ok(None) => {
                    let _ = tx.send(SidebandResponseEvent::Closed);
                    return;
                }
                Err(err) => {
                    let _ = tx.send(SidebandResponseEvent::ReadError(format!(
                        "failed to read sideband responses: {err}"
                    )));
                    return;
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(not(unix))]
pub(crate) fn spawn_response_reader(
    _fd: i32,
) -> Result<mpsc::UnboundedReceiver<SidebandResponseEvent>> {
    anyhow::bail!("sideband FDs are only supported on Unix targets")
}

#[cfg(test)]
mod tests {
    use super::ManagedServerRequest;
    use super::ServerRequestResolution;
    use super::SidebandResponseEnvelope;
    use super::ThreadResolvedPayload;
    use codex_app_server_protocol::ExecCommandApprovalParams;
    use codex_app_server_protocol::McpElicitationBooleanType;
    use codex_app_server_protocol::McpElicitationObjectType;
    use codex_app_server_protocol::McpElicitationPrimitiveSchema;
    use codex_app_server_protocol::McpElicitationSchema;
    use codex_app_server_protocol::McpServerElicitationRequest;
    use codex_app_server_protocol::McpServerElicitationRequestParams;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerRequest;
    use codex_app_server_protocol::ToolRequestUserInputOption;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn thread_resolved_payload_uses_started_source() {
        assert_eq!(
            ThreadResolvedPayload::started("thread-1"),
            ThreadResolvedPayload {
                thread_id: "thread-1".to_string(),
                source: "started".to_string(),
            }
        );
    }

    #[test]
    fn managed_request_normalizes_tool_user_input() {
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
                        description: "Continue".to_string(),
                    }]),
                }],
            },
        };

        let managed =
            ManagedServerRequest::try_from(&request).expect("tool user input should be supported");

        assert_eq!(managed.kind(), "tool_user_input_request");
        assert_eq!(
            managed.request_id(),
            &RequestId::String("req-1".to_string())
        );
        assert_eq!(managed.thread_id(), Some("thread-1"));
        assert_eq!(managed.turn_id(), Some("turn-1"));
    }

    #[test]
    fn managed_request_normalizes_legacy_exec_command_approval() {
        let request = ServerRequest::ExecCommandApproval {
            request_id: RequestId::Integer(7),
            params: serde_json::from_value::<ExecCommandApprovalParams>(json!({
                "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                "callId": "call-1",
                "approvalId": "approval-1",
                "command": ["git", "diff"],
                "cwd": "/tmp/project",
                "reason": "because",
                "parsedCmd": [],
            }))
            .expect("legacy exec approval params"),
        };

        let managed = ManagedServerRequest::try_from(&request)
            .expect("legacy exec approval should be normalized");

        assert_eq!(managed.kind(), "command_execution_approval_request");
        assert_eq!(managed.request_id(), &RequestId::Integer(7));
        assert_eq!(
            managed.thread_id(),
            Some("67e55044-10b1-426f-9247-bb680e5fe0c8")
        );
        assert_eq!(managed.turn_id(), None);
    }

    #[test]
    fn managed_request_serializes_raw_request_payload() {
        let request = ServerRequest::McpServerElicitationRequest {
            request_id: RequestId::String("req-1".to_string()),
            params: McpServerElicitationRequestParams {
                thread_id: "thread-1".to_string(),
                turn_id: Some("turn-1".to_string()),
                server_name: "server".to_string(),
                request: McpServerElicitationRequest::Form {
                    meta: None,
                    message: "Allow?".to_string(),
                    requested_schema: McpElicitationSchema {
                        schema_uri: None,
                        type_: McpElicitationObjectType::Object,
                        properties: BTreeMap::from([(
                            "confirmed".to_string(),
                            serde_json::from_value::<McpElicitationPrimitiveSchema>(json!({
                                "type": McpElicitationBooleanType::Boolean,
                            }))
                            .expect("schema"),
                        )]),
                        required: Some(vec!["confirmed".to_string()]),
                    },
                },
            },
        };

        let managed =
            ManagedServerRequest::try_from(&request).expect("mcp request should be supported");

        assert_eq!(managed.raw_payload()["serverName"], json!("server"));
        assert_eq!(managed.raw_payload()["threadId"], json!("thread-1"));
    }

    #[test]
    fn legacy_exec_command_response_maps_simple_acceptance_to_legacy_shape() {
        let request = ServerRequest::ExecCommandApproval {
            request_id: RequestId::Integer(7),
            params: serde_json::from_value::<ExecCommandApprovalParams>(json!({
                "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                "callId": "call-1",
                "approvalId": "approval-1",
                "command": ["git", "diff"],
                "cwd": "/tmp/project",
                "reason": "because",
                "parsedCmd": [],
            }))
            .expect("legacy exec approval params"),
        };

        let managed = ManagedServerRequest::try_from(&request)
            .expect("legacy exec approval should be normalized");
        let resolution = managed
            .parse_response(SidebandResponseEnvelope {
                kind: "command_execution_approval_response".to_string(),
                request_id: RequestId::Integer(7),
                response: json!({
                    "decision": "acceptForSession"
                }),
            })
            .expect("response should parse");

        let ServerRequestResolution::Resolve(value) = resolution else {
            panic!("legacy exec approval should resolve");
        };
        assert_eq!(value, json!({ "decision": "approved_for_session" }));
    }

    #[test]
    fn default_resolution_on_input_closed_cancels_mcp_and_rejects_tool_input() {
        let mcp_request =
            ManagedServerRequest::try_from(&ServerRequest::McpServerElicitationRequest {
                request_id: RequestId::String("req-mcp".to_string()),
                params: McpServerElicitationRequestParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: Some("turn-1".to_string()),
                    server_name: "server".to_string(),
                    request: McpServerElicitationRequest::Form {
                        meta: None,
                        message: "Allow?".to_string(),
                        requested_schema: McpElicitationSchema {
                            schema_uri: None,
                            type_: McpElicitationObjectType::Object,
                            properties: BTreeMap::new(),
                            required: None,
                        },
                    },
                },
            })
            .expect("mcp request should be supported");
        let tool_request = ManagedServerRequest::try_from(&ServerRequest::ToolRequestUserInput {
            request_id: RequestId::String("req-tool".to_string()),
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
                        description: "Continue".to_string(),
                    }]),
                }],
            },
        })
        .expect("tool request should be supported");

        let ServerRequestResolution::Resolve(mcp_value) = mcp_request
            .default_resolution_on_input_closed()
            .expect("mcp fallback should resolve")
        else {
            panic!("mcp fallback should resolve");
        };
        assert_eq!(
            mcp_value,
            json!({
                "action": "cancel",
                "content": null,
                "_meta": null
            })
        );

        let ServerRequestResolution::Reject(tool_error) = tool_request
            .default_resolution_on_input_closed()
            .expect("tool fallback should reject")
        else {
            panic!("tool fallback should reject");
        };
        assert!(
            tool_error
                .message
                .contains("tool input response channel closed")
        );
    }
}
