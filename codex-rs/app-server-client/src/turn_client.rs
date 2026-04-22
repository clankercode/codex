use std::fmt;
use std::path::PathBuf;

use crate::AppServerRequestHandle;
use crate::TypedRequestError;
use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;

#[derive(Clone)]
pub struct CodexTurnClient {
    request_handle: AppServerRequestHandle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadSessionRequest {
    pub thread_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub base_instructions: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnRequest {
    pub text: String,
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSessionStart {
    pub session: CodexTurnSession,
    pub thread: Thread,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexTurnSession {
    thread_id: String,
    active_turn_id: Option<String>,
}

#[derive(Debug)]
pub enum TurnClientError {
    MissingThread,
    Request(TypedRequestError),
}

impl fmt::Display for TurnClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingThread => write!(f, "thread session is not initialized"),
            Self::Request(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TurnClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingThread => None,
            Self::Request(err) => Some(err),
        }
    }
}

impl From<TypedRequestError> for TurnClientError {
    fn from(value: TypedRequestError) -> Self {
        Self::Request(value)
    }
}

impl CodexTurnClient {
    pub fn new(request_handle: AppServerRequestHandle) -> Self {
        Self { request_handle }
    }

    pub async fn start_or_resume_thread(
        &self,
        request_id: impl Into<String>,
        request: ThreadSessionRequest,
    ) -> Result<ThreadSessionStart, TurnClientError> {
        let request_id = RequestId::String(request_id.into());
        let thread = match build_thread_request(request_id, request) {
            ClientRequest::ThreadResume { request_id, params } => {
                self.request_handle
                    .request_typed::<ThreadResumeResponse>(ClientRequest::ThreadResume {
                        request_id,
                        params,
                    })
                    .await?
                    .thread
            }
            ClientRequest::ThreadStart { request_id, params } => {
                self.request_handle
                    .request_typed::<ThreadStartResponse>(ClientRequest::ThreadStart {
                        request_id,
                        params,
                    })
                    .await?
                    .thread
            }
            _ => unreachable!("thread request helper only produces start or resume requests"),
        };

        Ok(ThreadSessionStart {
            session: CodexTurnSession::from_thread(&thread),
            thread,
        })
    }

    pub async fn start_turn(
        &self,
        request_id: RequestId,
        session: &CodexTurnSession,
        request: TurnRequest,
    ) -> Result<TurnStartResponse, TurnClientError> {
        self.request_handle
            .request_typed::<TurnStartResponse>(build_turn_start_request(
                request_id,
                session.thread_id(),
                request,
            ))
            .await
            .map_err(Into::into)
    }

    pub async fn steer_turn(
        &self,
        request_id: RequestId,
        session: &CodexTurnSession,
        expected_turn_id: String,
        text: String,
    ) -> Result<TurnSteerResponse, TurnClientError> {
        self.request_handle
            .request_typed::<TurnSteerResponse>(build_turn_steer_request(
                request_id,
                session.thread_id(),
                expected_turn_id,
                text,
            ))
            .await
            .map_err(Into::into)
    }
}

impl CodexTurnSession {
    pub fn from_thread(thread: &Thread) -> Self {
        let active_turn_id = if matches!(thread.status, ThreadStatus::Active { .. }) {
            thread
                .turns
                .iter()
                .rev()
                .find(|turn| matches!(turn.status, TurnStatus::InProgress))
                .map(|turn| turn.id.clone())
        } else {
            None
        };

        Self {
            thread_id: thread.id.clone(),
            active_turn_id,
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        self.active_turn_id.as_deref()
    }

    pub fn on_turn_started(&mut self, thread_id: &str, turn_id: &str) {
        if thread_id == self.thread_id {
            self.active_turn_id = Some(turn_id.to_string());
        }
    }

    pub fn on_turn_completed(&mut self, thread_id: &str, turn_id: &str) {
        if thread_id == self.thread_id && self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
    }
}

pub(crate) fn build_thread_request(
    request_id: RequestId,
    request: ThreadSessionRequest,
) -> ClientRequest {
    let cwd = request.cwd.as_ref().map(|path| path.display().to_string());

    if let Some(thread_id) = request.thread_id {
        return ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id,
                model: request.model,
                model_provider: None,
                cwd,
                approval_policy: request.approval_policy,
                approvals_reviewer: request.approvals_reviewer,
                base_instructions: request.base_instructions,
                ..Default::default()
            },
        };
    }

    ClientRequest::ThreadStart {
        request_id,
        params: ThreadStartParams {
            model: request.model,
            model_provider: None,
            cwd,
            approval_policy: request.approval_policy,
            approvals_reviewer: request.approvals_reviewer,
            base_instructions: request.base_instructions,
            experimental_raw_events: false,
            ..Default::default()
        },
    }
}

pub(crate) fn build_turn_start_request(
    request_id: RequestId,
    thread_id: &str,
    request: TurnRequest,
) -> ClientRequest {
    ClientRequest::TurnStart {
        request_id,
        params: TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: request.text,
                text_elements: Vec::new(),
            }],
            prefixed_messages: None,
            responsesapi_client_metadata: None,
            cwd: request.cwd,
            approval_policy: request.approval_policy,
            approvals_reviewer: request.approvals_reviewer,
            sandbox_policy: None,
            model: request.model,
            service_tier: None,
            effort: None,
            summary: None,
            personality: None,
            output_schema: None,
            collaboration_mode: None,
            base_instructions: None,
            developer_instructions: None,
        },
    }
}

pub(crate) fn build_turn_steer_request(
    request_id: RequestId,
    thread_id: &str,
    expected_turn_id: String,
    text: String,
) -> ClientRequest {
    ClientRequest::TurnSteer {
        request_id,
        params: TurnSteerParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text,
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: None,
            expected_turn_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Turn;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_thread_request_uses_resume_when_thread_id_is_present() {
        let request = build_thread_request(
            RequestId::String("thread-resume".to_string()),
            ThreadSessionRequest {
                thread_id: Some("thread-123".to_string()),
                model: Some("gpt-5".to_string()),
                cwd: Some(PathBuf::from("/tmp/project")),
                approval_policy: Some(AskForApproval::OnRequest),
                approvals_reviewer: Some(ApprovalsReviewer::GuardianSubagent),
                base_instructions: None,
            },
        );

        match request {
            ClientRequest::ThreadResume { request_id, params } => {
                assert_eq!(request_id, RequestId::String("thread-resume".to_string()));
                assert_eq!(params.thread_id, "thread-123");
                assert_eq!(params.model, Some("gpt-5".to_string()));
                assert_eq!(params.cwd, Some("/tmp/project".to_string()));
                assert_eq!(params.approval_policy, Some(AskForApproval::OnRequest));
                assert_eq!(
                    params.approvals_reviewer,
                    Some(ApprovalsReviewer::GuardianSubagent)
                );
            }
            other => panic!("expected ThreadResume request, got {other:?}"),
        }
    }

    #[test]
    fn build_thread_request_sets_base_instructions_for_thread_start() {
        let request = build_thread_request(
            RequestId::String("thread-start".to_string()),
            ThreadSessionRequest {
                thread_id: None,
                model: None,
                cwd: None,
                approval_policy: None,
                approvals_reviewer: Some(ApprovalsReviewer::User),
                base_instructions: Some("system prompt".to_string()),
            },
        );

        match request {
            ClientRequest::ThreadStart { params, .. } => {
                assert_eq!(params.base_instructions, Some("system prompt".to_string()));
                assert_eq!(params.approvals_reviewer, Some(ApprovalsReviewer::User));
            }
            other => panic!("expected ThreadStart request, got {other:?}"),
        }
    }

    #[test]
    fn build_thread_request_sets_base_instructions_for_thread_resume() {
        let request = build_thread_request(
            RequestId::String("thread-resume".to_string()),
            ThreadSessionRequest {
                thread_id: Some("thread-123".to_string()),
                model: None,
                cwd: None,
                approval_policy: None,
                approvals_reviewer: Some(ApprovalsReviewer::User),
                base_instructions: Some("system prompt".to_string()),
            },
        );

        match request {
            ClientRequest::ThreadResume { params, .. } => {
                assert_eq!(params.base_instructions, Some("system prompt".to_string()));
                assert_eq!(params.approvals_reviewer, Some(ApprovalsReviewer::User));
            }
            other => panic!("expected ThreadResume request, got {other:?}"),
        }
    }

    #[test]
    fn build_turn_start_request_uses_session_thread_and_turn_overrides() {
        let request = build_turn_start_request(
            RequestId::String("turn-start-1".to_string()),
            "thread-1",
            TurnRequest {
                text: "hello".to_string(),
                model: Some("gpt-5".to_string()),
                cwd: Some(PathBuf::from("/tmp/project")),
                approval_policy: Some(AskForApproval::OnRequest),
                approvals_reviewer: Some(ApprovalsReviewer::GuardianSubagent),
            },
        );

        match request {
            ClientRequest::TurnStart { request_id, params } => {
                assert_eq!(request_id, RequestId::String("turn-start-1".to_string()));
                assert_eq!(params.thread_id, "thread-1");
                assert_eq!(params.model, Some("gpt-5".to_string()));
                assert_eq!(params.cwd, Some(PathBuf::from("/tmp/project")));
                assert_eq!(params.approval_policy, Some(AskForApproval::OnRequest));
                assert_eq!(
                    params.approvals_reviewer,
                    Some(ApprovalsReviewer::GuardianSubagent)
                );
                assert_eq!(
                    params.input,
                    vec![UserInput::Text {
                        text: "hello".to_string(),
                        text_elements: Vec::new(),
                    }]
                );
            }
            other => panic!("expected TurnStart request, got {other:?}"),
        }
    }

    #[test]
    fn build_turn_steer_request_uses_expected_turn_id() {
        let request = build_turn_steer_request(
            RequestId::String("turn-steer-1".to_string()),
            "thread-1",
            "turn-1".to_string(),
            "interrupt".to_string(),
        );

        match request {
            ClientRequest::TurnSteer { request_id, params } => {
                assert_eq!(request_id, RequestId::String("turn-steer-1".to_string()));
                assert_eq!(params.thread_id, "thread-1");
                assert_eq!(params.expected_turn_id, "turn-1");
                assert_eq!(
                    params.input,
                    vec![UserInput::Text {
                        text: "interrupt".to_string(),
                        text_elements: Vec::new(),
                    }]
                );
            }
            other => panic!("expected TurnSteer request, got {other:?}"),
        }
    }

    #[test]
    fn session_restores_latest_in_progress_turn_from_active_thread() {
        let session = CodexTurnSession::from_thread(&Thread {
            id: "thread-1".to_string(),
            turns: vec![
                Turn {
                    id: "turn-1".to_string(),
                    items: Vec::new(),
                    status: TurnStatus::Completed,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
                Turn {
                    id: "turn-2".to_string(),
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            ],
            status: ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            forked_from_id: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            path: None,
            cwd: AbsolutePathBuf::try_from("/tmp").expect("absolute path"),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
        });

        assert_eq!(session.thread_id(), "thread-1");
        assert_eq!(session.active_turn_id(), Some("turn-2"));
    }
}
