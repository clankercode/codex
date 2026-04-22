use std::collections::HashMap;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use codex_turn_start_bridge_core::BridgeController;
use codex_turn_start_bridge_core::CompletionSignal;
use codex_turn_start_bridge_core::ControllerEvent;
use codex_turn_start_bridge_core::ParsedMessage;
use codex_turn_start_bridge_core::ParsedXmlInput;
use codex_turn_start_bridge_core::QueuedMessage;
use codex_turn_start_bridge_core::ReleaseDecision;
use tokio::sync::mpsc;

use crate::bottom_pane::StructuredInputPreviewEntry;

#[cfg(test)]
use codex_turn_start_bridge_core::QueueMode;

#[cfg(unix)]
mod unix;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StructuredInputReaderEvent {
    Parsed(ParsedXmlInput),
    ParseError(String),
    Eof,
    ReadError(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StructuredInputAction {
    Release {
        thread_id: ThreadId,
        decision: ReleaseDecision,
    },
    Info(String),
    Error(String),
    RefreshPreview,
}

pub(crate) struct StructuredInputRuntime {
    receiver: mpsc::UnboundedReceiver<StructuredInputReaderEvent>,
    reader_active: bool,
    startup_system_prompt: Option<String>,
    startup_locked: bool,
    unbound_messages: Vec<QueuedMessage>,
    controllers: HashMap<ThreadId, BridgeController>,
}

impl StructuredInputRuntime {
    pub(crate) fn from_xml_input_fd(xml_input_fd: Option<i32>) -> color_eyre::Result<Option<Self>> {
        let Some(xml_input_fd) = xml_input_fd else {
            return Ok(None);
        };

        #[cfg(unix)]
        {
            let receiver = unix::spawn_xml_input_reader(xml_input_fd)
                .map_err(color_eyre::eyre::Report::new)?;
            Ok(Some(Self {
                receiver,
                reader_active: true,
                startup_system_prompt: None,
                startup_locked: false,
                unbound_messages: Vec::new(),
                controllers: HashMap::new(),
            }))
        }

        #[cfg(not(unix))]
        {
            let _ = xml_input_fd;
            color_eyre::eyre::bail!("--xml-input-fd is only supported on Unix targets");
        }
    }

    pub(crate) fn has_reader(&self) -> bool {
        self.reader_active
    }

    pub(crate) async fn recv(&mut self) -> Option<StructuredInputReaderEvent> {
        self.receiver.recv().await
    }

    pub(crate) async fn drain_startup(&mut self) -> Vec<StructuredInputAction> {
        tokio::task::yield_now().await;
        let mut actions = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            actions.extend(self.handle_reader_event(event, /*current_thread_id*/ None));
        }
        actions
    }

    pub(crate) fn startup_system_prompt(&self) -> Option<String> {
        self.startup_system_prompt.clone()
    }

    pub(crate) fn lock_startup(&mut self) {
        self.startup_locked = true;
    }

    pub(crate) fn bind_unbound_messages(
        &mut self,
        thread_id: ThreadId,
    ) -> Vec<StructuredInputAction> {
        if self.unbound_messages.is_empty() {
            return Vec::new();
        }

        let messages = std::mem::take(&mut self.unbound_messages);
        let mut actions = Vec::new();
        for message in messages {
            actions.extend(self.push_message(thread_id, message));
        }
        actions
    }

    pub(crate) fn handle_reader_event(
        &mut self,
        event: StructuredInputReaderEvent,
        current_thread_id: Option<ThreadId>,
    ) -> Vec<StructuredInputAction> {
        match event {
            StructuredInputReaderEvent::Parsed(ParsedXmlInput::SystemPrompt(prompt)) => {
                if self.startup_locked {
                    vec![StructuredInputAction::Error(
                        "Ignored late structured-input <system_prompt> fragment.".to_string(),
                    )]
                } else {
                    self.startup_system_prompt = Some(prompt);
                    Vec::new()
                }
            }
            StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(message)) => {
                let queued = QueuedMessage {
                    queue_mode: message.queue_mode,
                    text: message.text,
                };
                match current_thread_id {
                    Some(thread_id) => self.push_message(thread_id, queued),
                    None => {
                        self.unbound_messages.push(queued);
                        vec![StructuredInputAction::RefreshPreview]
                    }
                }
            }
            StructuredInputReaderEvent::ParseError(message) => {
                vec![StructuredInputAction::Error(message)]
            }
            StructuredInputReaderEvent::ReadError(message) => {
                self.reader_active = false;
                vec![StructuredInputAction::Error(message)]
            }
            StructuredInputReaderEvent::Eof => {
                self.reader_active = false;
                vec![StructuredInputAction::Info(
                    "Structured input channel closed; sideband XML input is now disabled."
                        .to_string(),
                )]
            }
        }
    }

    pub(crate) fn handle_server_notification(
        &mut self,
        notification: &ServerNotification,
    ) -> Vec<StructuredInputAction> {
        if let Some(thread_id) = thread_closed_notification_id(notification) {
            return self.drop_thread(thread_id);
        }

        let Some((thread_id, event)) = controller_event_from_notification(notification) else {
            return Vec::new();
        };
        self.handle_controller_event(thread_id, event)
    }

    pub(crate) fn handle_controller_event(
        &mut self,
        thread_id: ThreadId,
        event: ControllerEvent,
    ) -> Vec<StructuredInputAction> {
        let Some(controller) = self.controllers.get_mut(&thread_id) else {
            return Vec::new();
        };
        let decision = controller.on_event(event);
        let mut actions = vec![StructuredInputAction::RefreshPreview];
        if let Some(message) = controller.take_validation_error() {
            actions.push(StructuredInputAction::Error(format!(
                "Structured input controller validation error for thread {thread_id}: {message}"
            )));
        }
        if let Some(decision) = decision {
            actions.push(StructuredInputAction::Release {
                thread_id,
                decision,
            });
        }
        actions
    }

    pub(crate) fn preview_for_thread(
        &self,
        thread_id: Option<ThreadId>,
    ) -> Vec<StructuredInputPreviewEntry> {
        match thread_id {
            Some(thread_id) => self
                .controllers
                .get(&thread_id)
                .map(preview_entries_for_controller)
                .unwrap_or_default(),
            None => self
                .unbound_messages
                .iter()
                .cloned()
                .map(StructuredInputPreviewEntry::from)
                .collect(),
        }
    }

    fn push_message(
        &mut self,
        thread_id: ThreadId,
        message: QueuedMessage,
    ) -> Vec<StructuredInputAction> {
        let controller = self
            .controllers
            .entry(thread_id)
            .or_insert_with(|| BridgeController::new(thread_id.to_string()));
        let decision = controller.on_event(ControllerEvent::MessageReceived(message));
        let mut actions = vec![StructuredInputAction::RefreshPreview];
        if let Some(decision) = decision {
            actions.push(StructuredInputAction::Release {
                thread_id,
                decision,
            });
        }
        actions
    }

    fn drop_thread(&mut self, thread_id: ThreadId) -> Vec<StructuredInputAction> {
        let Some(controller) = self.controllers.remove(&thread_id) else {
            return Vec::new();
        };
        let snapshot = controller.queue_snapshot();
        let dropped_count = snapshot.pending_immediate.len()
            + snapshot.steer_pending.len()
            + snapshot.after_tool_call.len()
            + snapshot.after_any_item.len()
            + snapshot.next_turn.len();
        let mut actions = vec![StructuredInputAction::RefreshPreview];
        if dropped_count > 0 {
            actions.push(StructuredInputAction::Error(format!(
                "Dropped {dropped_count} queued structured-input message(s) for closed thread {thread_id}."
            )));
        }
        actions
    }
}

fn preview_entries_for_controller(
    controller: &BridgeController,
) -> Vec<StructuredInputPreviewEntry> {
    let snapshot = controller.queue_snapshot();
    let mut entries = Vec::new();
    entries.extend(snapshot.pending_immediate.into_iter().map(Into::into));
    entries.extend(snapshot.steer_pending.into_iter().map(Into::into));
    entries.extend(snapshot.after_tool_call.into_iter().map(Into::into));
    entries.extend(snapshot.after_any_item.into_iter().map(Into::into));
    entries.extend(snapshot.next_turn.into_iter().map(Into::into));
    entries
}

fn thread_closed_notification_id(notification: &ServerNotification) -> Option<ThreadId> {
    let ServerNotification::ThreadClosed(notification) = notification else {
        return None;
    };
    ThreadId::from_string(&notification.thread_id).ok()
}

fn controller_event_from_notification(
    notification: &ServerNotification,
) -> Option<(ThreadId, ControllerEvent)> {
    match notification {
        ServerNotification::TurnStarted(notification) => Some((
            ThreadId::from_string(&notification.thread_id).ok()?,
            ControllerEvent::TurnStarted {
                thread_id: notification.thread_id.clone(),
                turn_id: notification.turn.id.clone(),
            },
        )),
        ServerNotification::TurnCompleted(notification) => Some((
            ThreadId::from_string(&notification.thread_id).ok()?,
            ControllerEvent::TurnCompleted {
                thread_id: notification.thread_id.clone(),
                turn_id: notification.turn.id.clone(),
            },
        )),
        ServerNotification::TerminalInteraction(notification) => Some((
            ThreadId::from_string(&notification.thread_id).ok()?,
            ControllerEvent::TerminalInteraction {
                thread_id: notification.thread_id.clone(),
                turn_id: notification.turn_id.clone(),
            },
        )),
        ServerNotification::ItemCompleted(notification) => Some((
            ThreadId::from_string(&notification.thread_id).ok()?,
            ControllerEvent::ItemCompleted {
                thread_id: notification.thread_id.clone(),
                turn_id: notification.turn_id.clone(),
                signal: classify_item_completed(&notification.item),
            },
        )),
        _ => None,
    }
}

fn classify_item_completed(item: &ThreadItem) -> CompletionSignal {
    match item {
        ThreadItem::CommandExecution { .. }
        | ThreadItem::FileChange { .. }
        | ThreadItem::McpToolCall { .. }
        | ThreadItem::DynamicToolCall { .. }
        | ThreadItem::WebSearch { .. }
        | ThreadItem::ImageGeneration { .. }
        | ThreadItem::CollabAgentToolCall { .. } => CompletionSignal::ReleasesAfterAnyItem,
        _ => CompletionSignal::Ignore,
    }
}

impl From<ParsedMessage> for StructuredInputPreviewEntry {
    fn from(message: ParsedMessage) -> Self {
        Self {
            queue_mode: message.queue_mode,
            text: message.text,
        }
    }
}

impl From<QueuedMessage> for StructuredInputPreviewEntry {
    fn from(message: QueuedMessage) -> Self {
        Self {
            queue_mode: message.queue_mode,
            text: message.text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;

    fn runtime() -> (
        StructuredInputRuntime,
        mpsc::UnboundedSender<StructuredInputReaderEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        (
            StructuredInputRuntime {
                receiver: rx,
                reader_active: true,
                startup_system_prompt: None,
                startup_locked: false,
                unbound_messages: Vec::new(),
                controllers: HashMap::new(),
            },
            tx,
        )
    }

    #[tokio::test]
    async fn drain_startup_captures_system_prompt_and_unbound_message() {
        let (mut runtime, tx) = runtime();
        tx.send(StructuredInputReaderEvent::Parsed(
            ParsedXmlInput::SystemPrompt("be terse".to_string()),
        ))
        .unwrap();
        tx.send(StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(
            ParsedMessage {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            },
        )))
        .unwrap();

        let actions = runtime.drain_startup().await;

        assert_eq!(actions, vec![StructuredInputAction::RefreshPreview]);
        assert_eq!(
            runtime.startup_system_prompt(),
            Some("be terse".to_string())
        );
        assert_eq!(
            runtime.preview_for_thread(None),
            vec![StructuredInputPreviewEntry {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn eof_disables_reader_without_dropping_unbound_messages() {
        let (mut runtime, _tx) = runtime();
        runtime.unbound_messages.push(QueuedMessage {
            queue_mode: QueueMode::AfterToolCall,
            text: "hello".to_string(),
        });

        let actions = runtime.handle_reader_event(StructuredInputReaderEvent::Eof, None);

        assert_eq!(
            actions,
            vec![StructuredInputAction::Info(
                "Structured input channel closed; sideband XML input is now disabled.".to_string()
            )]
        );
        assert!(!runtime.has_reader());
        assert_eq!(
            runtime.preview_for_thread(None),
            vec![StructuredInputPreviewEntry {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn bind_unbound_messages_releases_first_message_for_first_thread() {
        let (mut runtime, _tx) = runtime();
        let thread_id = ThreadId::new();
        runtime.unbound_messages.push(QueuedMessage {
            queue_mode: QueueMode::AfterToolCall,
            text: "hello".to_string(),
        });

        let actions = runtime.bind_unbound_messages(thread_id);

        assert_eq!(runtime.unbound_messages, Vec::<QueuedMessage>::new());
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, StructuredInputAction::Release { .. }))
        );
    }

    #[test]
    fn late_system_prompt_is_rejected() {
        let (mut runtime, _tx) = runtime();
        runtime.lock_startup();

        let actions = runtime.handle_reader_event(
            StructuredInputReaderEvent::Parsed(ParsedXmlInput::SystemPrompt("late".to_string())),
            None,
        );

        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], StructuredInputAction::Error(_)));
    }

    #[test]
    fn thread_close_drops_queued_messages() {
        let thread_id = ThreadId::new();
        let (mut runtime, _tx) = runtime();
        runtime
            .controllers
            .insert(thread_id, BridgeController::new(thread_id.to_string()));
        runtime.handle_controller_event(
            thread_id,
            ControllerEvent::TurnStarted {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
            },
        );
        let _ = runtime.handle_reader_event(
            StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            })),
            Some(thread_id),
        );

        let actions = runtime.drop_thread(thread_id);

        assert!(
            actions
                .iter()
                .any(|action| matches!(action, StructuredInputAction::Error(_)))
        );
    }
}
