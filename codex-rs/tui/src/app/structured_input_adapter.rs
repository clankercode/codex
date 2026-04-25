use super::App;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ThreadSessionState;
use crate::chatwidget::ThreadInputState;
use crate::structured_input::StructuredInputAction;
use crate::structured_input::StructuredInputReaderEvent;
use codex_app_server_client::TypedRequestError;
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum StructuredInputSteerFailure {
    Requeue,
    StartTurn,
    RetryWithTurnId { turn_id: String },
    UpdateActiveTurnAndFail { turn_id: String },
}

fn structured_input_steer_failure(
    error: &TypedRequestError,
    attempted_turn_id: &str,
    retried_after_turn_mismatch: bool,
) -> Option<StructuredInputSteerFailure> {
    if super::active_turn_not_steerable_turn_error(error).is_some() {
        return Some(StructuredInputSteerFailure::Requeue);
    }

    match super::active_turn_steer_race(error) {
        Some(super::ActiveTurnSteerRace::Missing) => Some(StructuredInputSteerFailure::StartTurn),
        Some(super::ActiveTurnSteerRace::ExpectedTurnMismatch { actual_turn_id })
            if !retried_after_turn_mismatch && actual_turn_id != attempted_turn_id =>
        {
            Some(StructuredInputSteerFailure::RetryWithTurnId {
                turn_id: actual_turn_id,
            })
        }
        Some(super::ActiveTurnSteerRace::ExpectedTurnMismatch { actual_turn_id }) => {
            Some(StructuredInputSteerFailure::UpdateActiveTurnAndFail {
                turn_id: actual_turn_id,
            })
        }
        None => None,
    }
}

impl App {
    pub(super) async fn handle_structured_input_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: StructuredInputReaderEvent,
    ) -> Result<()> {
        let current_thread_id = self.current_displayed_thread_id();
        let active_turn_id = match current_thread_id {
            Some(thread_id) => self.active_turn_id_for_thread(thread_id).await,
            None => None,
        };
        let Some(runtime) = self.structured_input.as_mut() else {
            return Ok(());
        };
        let mut actions = Vec::new();
        if let (Some(thread_id), Some(turn_id)) = (current_thread_id, active_turn_id) {
            actions.extend(runtime.sync_active_turn(thread_id, turn_id));
        }
        actions.extend(runtime.handle_reader_event(event, current_thread_id));
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
        let active_turn_id = self.active_turn_id_for_thread(thread_id).await;
        let Some(runtime) = self.structured_input.as_mut() else {
            return Ok(());
        };
        let mut actions = Vec::new();
        if let Some(turn_id) = active_turn_id {
            actions.extend(runtime.sync_active_turn(thread_id, turn_id));
        }
        actions.extend(runtime.bind_unbound_messages(thread_id));
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
                let mut steer_turn_id = turn_id;
                let mut retried_after_turn_mismatch = false;
                loop {
                    let result = app_server
                        .turn_steer(thread_id, steer_turn_id.clone(), items.clone())
                        .await;
                    match result {
                        Ok(response) => {
                            break Ok(self
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
                                .unwrap_or_default());
                        }
                        Err(error) => match structured_input_steer_failure(
                            &error,
                            &steer_turn_id,
                            retried_after_turn_mismatch,
                        ) {
                            Some(StructuredInputSteerFailure::Requeue) => {
                                break Ok(self
                                    .structured_input
                                    .as_mut()
                                    .map(|runtime| {
                                        runtime.handle_controller_event(
                                            thread_id,
                                            ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                                                message: decision.message.clone(),
                                            },
                                        )
                                    })
                                    .unwrap_or_default());
                            }
                            Some(StructuredInputSteerFailure::StartTurn) => {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.clear_active_turn_id();
                                }
                                break Ok(self
                                    .structured_input
                                    .as_mut()
                                    .map(|runtime| {
                                        runtime.recover_missing_active_turn(
                                            thread_id,
                                            decision.message.clone(),
                                            decision.reason,
                                        )
                                    })
                                    .unwrap_or_default());
                            }
                            Some(StructuredInputSteerFailure::RetryWithTurnId { turn_id }) => {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.active_turn_id = Some(turn_id.clone());
                                }
                                if let Some(runtime) = self.structured_input.as_mut() {
                                    let _ =
                                        runtime.reconcile_active_turn(thread_id, turn_id.clone());
                                }
                                steer_turn_id = turn_id;
                                retried_after_turn_mismatch = true;
                            }
                            Some(StructuredInputSteerFailure::UpdateActiveTurnAndFail {
                                turn_id,
                            }) => {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.active_turn_id = Some(turn_id.clone());
                                }
                                if let Some(runtime) = self.structured_input.as_mut() {
                                    let _ = runtime.reconcile_active_turn(thread_id, turn_id);
                                }
                                break Ok(vec![StructuredInputAction::Error(format!(
                                    "Structured input turn/steer failed for thread {thread_id}: {error}"
                                ))]);
                            }
                            None => {
                                break Ok(vec![StructuredInputAction::Error(format!(
                                    "Structured input turn/steer failed for thread {thread_id}: {error}"
                                ))]);
                            }
                        },
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::StructuredInputSteerFailure;
    use super::structured_input_steer_failure;
    use crate::app::AppServerCodexErrorInfo;
    use crate::app::AppServerTurnError;
    use crate::app::TypedRequestError;
    use codex_app_server_protocol::JSONRPCErrorError;
    use codex_app_server_protocol::NonSteerableTurnKind as AppServerNonSteerableTurnKind;
    use pretty_assertions::assert_eq;

    #[test]
    fn structured_input_steer_failure_treats_typed_non_steerable_error_as_requeueable() {
        let turn_error = AppServerTurnError {
            message: "cannot steer a review turn".to_string(),
            codex_error_info: Some(AppServerCodexErrorInfo::ActiveTurnNotSteerable {
                turn_kind: AppServerNonSteerableTurnKind::Review,
            }),
            additional_details: None,
        };
        let error = TypedRequestError::Server {
            method: "turn/steer".to_string(),
            source: JSONRPCErrorError {
                code: -32602,
                message: turn_error.message.clone(),
                data: Some(serde_json::to_value(&turn_error).expect("turn error should serialize")),
            },
        };

        assert_eq!(
            structured_input_steer_failure(&error, "turn-attempted", false),
            Some(StructuredInputSteerFailure::Requeue)
        );
    }

    #[test]
    fn structured_input_steer_failure_treats_missing_active_turn_as_start_turn() {
        let error = TypedRequestError::Server {
            method: "turn/steer".to_string(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "no active turn to steer".to_string(),
                data: None,
            },
        };

        assert_eq!(
            structured_input_steer_failure(&error, "turn-attempted", false),
            Some(StructuredInputSteerFailure::StartTurn)
        );
    }

    #[test]
    fn structured_input_steer_failure_retries_expected_turn_mismatch_once() {
        let error = TypedRequestError::Server {
            method: "turn/steer".to_string(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "expected active turn id `turn-attempted` but found `turn-actual`"
                    .to_string(),
                data: None,
            },
        };

        assert_eq!(
            structured_input_steer_failure(&error, "turn-attempted", false),
            Some(StructuredInputSteerFailure::RetryWithTurnId {
                turn_id: "turn-actual".to_string(),
            })
        );
    }

    #[test]
    fn structured_input_steer_failure_fails_on_repeated_expected_turn_mismatch() {
        let error = TypedRequestError::Server {
            method: "turn/steer".to_string(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "expected active turn id `turn-attempted` but found `turn-actual`"
                    .to_string(),
                data: None,
            },
        };

        assert_eq!(
            structured_input_steer_failure(&error, "turn-attempted", true),
            Some(StructuredInputSteerFailure::UpdateActiveTurnAndFail {
                turn_id: "turn-actual".to_string(),
            })
        );
    }
}
