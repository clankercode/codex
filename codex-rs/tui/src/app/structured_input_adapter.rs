use super::App;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ThreadSessionState;
use crate::chatwidget::ThreadInputState;
use crate::structured_input::StructuredInputAction;
use crate::structured_input::StructuredInputReaderEvent;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::user_input::UserInput;
use codex_turn_start_bridge_core::ControllerEvent;
use codex_turn_start_bridge_core::ReleaseAction;
use codex_turn_start_bridge_core::ReleaseDecision;
use color_eyre::eyre::Result;
use std::collections::VecDeque;

fn is_active_turn_not_steerable_message(message: &str) -> bool {
    message.contains("active turn") && message.contains("not steerable")
}

impl App {
    pub(super) async fn handle_structured_input_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: StructuredInputReaderEvent,
    ) -> Result<()> {
        let current_thread_id = self.current_displayed_thread_id();
        let Some(runtime) = self.structured_input.as_mut() else {
            return Ok(());
        };
        let actions = runtime.handle_reader_event(event, current_thread_id);
        self.apply_structured_input_actions(app_server, actions)
            .await
    }

    pub(super) async fn bind_structured_input_for_current_thread(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> Result<()> {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            self.refresh_structured_input_preview();
            return Ok(());
        };
        let Some(runtime) = self.structured_input.as_mut() else {
            return Ok(());
        };
        let actions = runtime.bind_unbound_messages(thread_id);
        self.apply_structured_input_actions(app_server, actions)
            .await
    }

    pub(super) fn disable_structured_input(&mut self) {
        self.structured_input = None;
        self.refresh_structured_input_preview();
    }

    pub(super) fn refresh_structured_input_preview(&mut self) {
        let preview = self
            .structured_input
            .as_ref()
            .map(|runtime| runtime.preview_for_thread(self.current_displayed_thread_id()))
            .unwrap_or_default();
        self.chat_widget.set_structured_input_preview(preview);
    }

    pub(super) async fn apply_structured_input_actions(
        &mut self,
        app_server: &mut AppServerSession,
        actions: Vec<StructuredInputAction>,
    ) -> Result<()> {
        let mut pending = VecDeque::from(actions);
        while let Some(action) = pending.pop_front() {
            match action {
                StructuredInputAction::Release {
                    thread_id,
                    decision,
                } => {
                    let follow_up = self
                        .execute_structured_input_release(app_server, thread_id, decision)
                        .await?;
                    pending.extend(follow_up);
                }
                StructuredInputAction::Info(message) => {
                    self.chat_widget.add_info_message(message, /*hint*/ None);
                }
                StructuredInputAction::Error(message) => {
                    self.chat_widget.add_error_message(message);
                }
                StructuredInputAction::RefreshPreview => {
                    self.refresh_structured_input_preview();
                }
            }
        }
        Ok(())
    }

    async fn execute_structured_input_release(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        decision: ReleaseDecision,
    ) -> Result<Vec<StructuredInputAction>> {
        let Some((session, collaboration_mode, personality)) =
            self.structured_input_session_state(thread_id).await
        else {
            return Ok(vec![StructuredInputAction::Error(format!(
                "No configured thread session is available for structured input on thread {thread_id}."
            ))]);
        };
        let items = vec![UserInput::Text {
            text: decision.message.text.clone(),
            text_elements: Vec::new(),
        }];

        match decision.action.clone() {
            ReleaseAction::StartTurn => {
                let result = app_server
                    .turn_start(
                        thread_id,
                        items,
                        /*prefixed_messages*/ None,
                        session.cwd.to_path_buf(),
                        session.approval_policy,
                        session.approvals_reviewer,
                        session.sandbox_policy.clone(),
                        session.model,
                        session.reasoning_effort,
                        /*summary*/ None,
                        Some(session.service_tier),
                        collaboration_mode,
                        personality,
                        /*output_schema*/ None,
                    )
                    .await;
                Ok(match result {
                    Ok(response) => self
                        .structured_input
                        .as_mut()
                        .map(|runtime| {
                            runtime.handle_controller_event(
                                thread_id,
                                ControllerEvent::TurnStartAcceptedWithTurnId {
                                    turn_id: response.turn.id,
                                },
                            )
                        })
                        .unwrap_or_default(),
                    Err(error) if is_active_turn_not_steerable_message(&error.to_string()) => self
                        .structured_input
                        .as_mut()
                        .map(|runtime| {
                            runtime.handle_controller_event(
                                thread_id,
                                ControllerEvent::TurnStartRejectedActiveTurnNotSteerable,
                            )
                        })
                        .unwrap_or_default(),
                    Err(error) => vec![StructuredInputAction::Error(format!(
                        "Structured input turn/start failed for thread {thread_id}: {error}"
                    ))],
                })
            }
            ReleaseAction::SteerTurn { turn_id } => {
                let result = app_server.turn_steer(thread_id, turn_id, items).await;
                Ok(match result {
                    Ok(response) => self
                        .structured_input
                        .as_mut()
                        .map(|runtime| {
                            runtime.handle_controller_event(
                                thread_id,
                                ControllerEvent::SteerAccepted {
                                    turn_id: response.turn_id,
                                },
                            )
                        })
                        .unwrap_or_default(),
                    Err(error) if super::active_turn_steer_race(&error).is_some() => self
                        .structured_input
                        .as_mut()
                        .map(|runtime| {
                            runtime.handle_controller_event(
                                thread_id,
                                ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                                    message: decision.message,
                                },
                            )
                        })
                        .unwrap_or_default(),
                    Err(error) => vec![StructuredInputAction::Error(format!(
                        "Structured input turn/steer failed for thread {thread_id}: {error}"
                    ))],
                })
            }
        }
    }

    pub(super) async fn structured_input_session_state(
        &self,
        thread_id: ThreadId,
    ) -> Option<(
        ThreadSessionState,
        Option<CollaborationMode>,
        Option<Personality>,
    )> {
        if self.primary_thread_id == Some(thread_id) {
            let session = self.primary_session_configured.clone()?;
            let (collaboration_mode, personality) =
                self.structured_input_turn_context(thread_id, &session, None);
            return Some((session, collaboration_mode, personality));
        }

        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        let session = store.session.clone()?;
        let (collaboration_mode, personality) =
            self.structured_input_turn_context(thread_id, &session, store.input_state.as_ref());
        Some((session, collaboration_mode, personality))
    }

    fn structured_input_turn_context(
        &self,
        thread_id: ThreadId,
        session: &ThreadSessionState,
        input_state: Option<&ThreadInputState>,
    ) -> (Option<CollaborationMode>, Option<Personality>) {
        if self.current_displayed_thread_id() == Some(thread_id) {
            return self.chat_widget.structured_input_turn_context();
        }

        let collaboration_mode =
            input_state.and_then(ThreadInputState::submission_collaboration_mode);
        let target_model = collaboration_mode
            .as_ref()
            .map_or_else(|| session.model.as_str(), CollaborationMode::model);
        let personality = input_state
            .and_then(ThreadInputState::personality)
            .filter(|_| {
                self.config
                    .features
                    .enabled(codex_features::Feature::Personality)
            })
            .filter(|_| self.model_supports_personality(target_model));
        (collaboration_mode, personality)
    }

    fn model_supports_personality(&self, model: &str) -> bool {
        self.model_catalog
            .try_list_models()
            .ok()
            .and_then(|presets| {
                presets
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.supports_personality)
            })
            .unwrap_or(false)
    }
}
