use crate::cli::Cli;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_turn_start_bridge_core::sideband::ManagedServerRequest;
use codex_turn_start_bridge_core::sideband::ServerRequestResolution;
use codex_turn_start_bridge_core::sideband::SidebandOutputs;
use codex_turn_start_bridge_core::sideband::SidebandResponseEnvelope;
use codex_turn_start_bridge_core::sideband::SidebandResponseEvent;
use color_eyre::eyre::Result;
use color_eyre::eyre::eyre;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub(crate) struct ServerRequestSideband {
    outputs: SidebandOutputs,
    response_rx: Option<mpsc::UnboundedReceiver<SidebandResponseEvent>>,
    response_available: bool,
    pending_requests: HashMap<RequestId, ManagedServerRequest>,
}

impl ServerRequestSideband {
    pub(crate) fn from_cli(cli: &Cli) -> Result<Option<Self>> {
        validate_cli_fds(cli)?;
        let outputs = SidebandOutputs::from_fds(
            /*thread_id_fd*/ None,
            cli.server_request_events_fd,
            cli.control_events_fd,
        )
        .map_err(|err| eyre!("{err:#}"))?;
        let response_rx = match (cli.server_request_responses_fd, cli.control_responses_fd) {
            (Some(_), Some(_)) => {
                unreachable!("validate_cli_fds rejects duplicate response lanes");
            }
            (Some(fd), None) | (None, Some(fd)) => Some(
                codex_turn_start_bridge_core::sideband::spawn_response_reader(fd)
                    .map_err(|err| eyre!("{err:#}"))?,
            ),
            (None, None) => None,
        };

        if !outputs.has_request_event_sink() && response_rx.is_none() {
            return Ok(None);
        }

        Ok(Some(Self {
            outputs,
            response_available: response_rx.is_some(),
            response_rx,
            pending_requests: HashMap::new(),
        }))
    }

    #[cfg(test)]
    fn new_for_test(response_available: bool) -> Self {
        Self {
            outputs: SidebandOutputs::from_fds(
                /*thread_id_fd*/ None, /*server_request_events_fd*/ None,
                /*control_events_fd*/ None,
            )
            .expect("test sideband outputs"),
            response_rx: None,
            response_available,
            pending_requests: HashMap::new(),
        }
    }

    pub(crate) fn has_response_reader(&self) -> bool {
        self.response_rx.is_some()
    }

    pub(crate) async fn recv_response(&mut self) -> Option<SidebandResponseEvent> {
        match self.response_rx.as_mut() {
            Some(response_rx) => response_rx.recv().await,
            None => None,
        }
    }

    pub(crate) fn note_server_request(&mut self, request: &ServerRequest) -> Result<()> {
        let Ok(managed) = ManagedServerRequest::try_from(request) else {
            return Ok(());
        };

        self.outputs
            .emit_request(&managed)
            .map_err(|err| eyre!("{err:#}"))?;
        if self.response_available {
            self.pending_requests
                .insert(managed.request_id().clone(), managed);
        }
        Ok(())
    }

    pub(crate) fn take_valid_response(
        &mut self,
        response: SidebandResponseEnvelope,
    ) -> Option<(RequestId, ServerRequestResolution)> {
        let request_id = response.request_id.clone();
        let request = self.pending_requests.get(&request_id)?;
        match request.parse_response(response) {
            Ok(resolution) => {
                self.pending_requests.remove(&request_id);
                Some((request_id, resolution))
            }
            Err(err) => {
                tracing::warn!(
                    "ignoring invalid sideband response for request id `{request_id}`: {err}"
                );
                None
            }
        }
    }

    pub(crate) fn remove_pending_request(&mut self, request_id: &RequestId) {
        self.pending_requests.remove(request_id);
    }

    pub(crate) fn close_response_reader(&mut self, reason: &str) {
        self.response_rx = None;
        self.response_available = false;
        self.pending_requests.clear();
        tracing::warn!("server-request sideband response channel closed: {reason}");
    }

    #[cfg(test)]
    fn has_pending_request(&self, request_id: &RequestId) -> bool {
        self.pending_requests.contains_key(request_id)
    }
}

fn validate_cli_fds(cli: &Cli) -> Result<()> {
    #[cfg(not(unix))]
    {
        if cli.server_request_events_fd.is_some()
            || cli.server_request_responses_fd.is_some()
            || cli.control_events_fd.is_some()
            || cli.control_responses_fd.is_some()
        {
            color_eyre::eyre::bail!(
                "server-request sideband FDs are only supported on Unix targets"
            );
        }
    }

    #[cfg(unix)]
    {
        let mut seen = HashMap::<i32, &str>::new();
        for (name, fd) in [
            ("xml-input-fd", cli.xml_input_fd),
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
                color_eyre::eyre::bail!(
                    "fd `{fd}` is configured for both `--{previous}` and `--{name}`; use distinct sideband file descriptors"
                );
            }
        }
    }

    if cli.server_request_responses_fd.is_some() && cli.control_responses_fd.is_some() {
        color_eyre::eyre::bail!(
            "--server-request-responses-fd and --control-responses-fd are mutually exclusive"
        );
    }
    if (cli.server_request_responses_fd.is_some() || cli.control_responses_fd.is_some())
        && cli.server_request_events_fd.is_none()
        && cli.control_events_fd.is_none()
    {
        color_eyre::eyre::bail!(
            "a sideband response fd requires either --server-request-events-fd or --control-events-fd"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ServerRequestSideband;
    use codex_app_server_protocol::CommandExecutionApprovalDecision;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerRequest;
    use codex_turn_start_bridge_core::sideband::ServerRequestResolution;
    use codex_turn_start_bridge_core::sideband::SidebandResponseEnvelope;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn exec_approval_request() -> ServerRequest {
        ServerRequest::CommandExecutionRequestApproval {
            request_id: RequestId::Integer(1),
            params: CommandExecutionRequestApprovalParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                approval_id: None,
                reason: Some("needs approval".to_string()),
                network_approval_context: None,
                command: Some("echo hello".to_string()),
                cwd: Some(
                    AbsolutePathBuf::try_from(std::path::PathBuf::from("/tmp/project"))
                        .expect("absolute cwd"),
                ),
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        }
    }

    #[test]
    fn sideband_records_supported_request_and_accepts_matching_response() {
        let mut sideband = ServerRequestSideband::new_for_test(/*response_available*/ true);
        sideband
            .note_server_request(&exec_approval_request())
            .expect("request should be supported");

        let response = SidebandResponseEnvelope {
            kind: "command_execution_approval_response".to_string(),
            request_id: RequestId::Integer(1),
            response: serde_json::to_value(CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::Accept,
            })
            .expect("response serializes"),
        };
        let Some((request_id, resolution)) = sideband.take_valid_response(response) else {
            panic!("matching response should resolve");
        };

        assert_eq!(request_id, RequestId::Integer(1));
        let ServerRequestResolution::Resolve(value) = resolution else {
            panic!("command approval should resolve");
        };
        assert_eq!(value, json!({ "decision": "accept" }));
        assert!(!sideband.has_pending_request(&RequestId::Integer(1)));
    }

    #[test]
    fn sideband_ignores_unknown_response_without_clearing_pending_request() {
        let mut sideband = ServerRequestSideband::new_for_test(/*response_available*/ true);
        sideband
            .note_server_request(&exec_approval_request())
            .expect("request should be supported");

        let outcome = sideband.take_valid_response(SidebandResponseEnvelope {
            kind: "command_execution_approval_response".to_string(),
            request_id: RequestId::Integer(99),
            response: json!({ "decision": "accept" }),
        });

        assert!(outcome.is_none());
        assert!(sideband.has_pending_request(&RequestId::Integer(1)));
    }
}
