use std::collections::HashMap;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_turn_start_bridge_core::BridgeController;
use codex_turn_start_bridge_core::CompletionSignal;
use codex_turn_start_bridge_core::ControllerEvent;
use codex_turn_start_bridge_core::ParsedMessage;
use codex_turn_start_bridge_core::ParsedXmlInput;
use codex_turn_start_bridge_core::QueuedMessage;
use codex_turn_start_bridge_core::ReleaseDecision;
use codex_turn_start_bridge_core::ReleaseReason;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::bottom_pane::StructuredInputPreviewEntry;

#[cfg(test)]
use codex_turn_start_bridge_core::QueueMode;

#[cfg(unix)]
mod unix;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StructuredInputReaderEvent {
    Parsed(ParsedXmlInput),
    StartupDrainComplete,
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
    reader_task: Option<JoinHandle<()>>,
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
            let (receiver, reader_task) = unix::spawn_xml_input_reader(xml_input_fd)
                .map_err(color_eyre::eyre::Report::new)?;
            Ok(Some(Self {
                receiver,
                reader_task: Some(reader_task),
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
        let mut actions = Vec::new();
        while let Some(event) = self.receiver.recv().await {
            let startup_drain_complete =
                matches!(event, StructuredInputReaderEvent::StartupDrainComplete);
            let reader_done = matches!(
                event,
                StructuredInputReaderEvent::Eof | StructuredInputReaderEvent::ReadError(_)
            );
            actions.extend(self.handle_reader_event(event, /*current_thread_id*/ None));

            while let Ok(event) = self.receiver.try_recv() {
                let startup_drain_complete =
                    matches!(event, StructuredInputReaderEvent::StartupDrainComplete);
                let reader_done = matches!(
                    event,
                    StructuredInputReaderEvent::Eof | StructuredInputReaderEvent::ReadError(_)
                );
                actions.extend(self.handle_reader_event(event, /*current_thread_id*/ None));
                if startup_drain_complete || reader_done {
                    return actions;
                }
            }

            if startup_drain_complete || reader_done {
                return actions;
            }
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
            StructuredInputReaderEvent::StartupDrainComplete => Vec::new(),
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

    pub(crate) fn recover_missing_active_turn(
        &mut self,
        thread_id: ThreadId,
        message: QueuedMessage,
        reason: ReleaseReason,
    ) -> Vec<StructuredInputAction> {
        let Some(controller) = self.controllers.get_mut(&thread_id) else {
            return Vec::new();
        };
        let decision = controller.recover_missing_active_turn(message, reason);
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

    pub(crate) fn reconcile_active_turn(
        &mut self,
        thread_id: ThreadId,
        turn_id: String,
    ) -> Vec<StructuredInputAction> {
        let Some(controller) = self.controllers.get_mut(&thread_id) else {
            return Vec::new();
        };
        let decision = controller.on_event(ControllerEvent::ActiveTurnReconciled {
            thread_id: thread_id.to_string(),
            turn_id,
        });
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

    pub(crate) fn sync_active_turn(
        &mut self,
        thread_id: ThreadId,
        turn_id: String,
    ) -> Vec<StructuredInputAction> {
        self.controllers
            .entry(thread_id)
            .or_insert_with(|| BridgeController::new(thread_id.to_string()));
        self.reconcile_active_turn(thread_id, turn_id)
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

impl Drop for StructuredInputRuntime {
    fn drop(&mut self) {
        if let Some(handle) = self.reader_task.take() {
            handle.abort();
        }
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
                item_key: Some(notification.item.id().to_string()),
            },
        )),
        ServerNotification::RawResponseItemCompleted(notification) => {
            let (signal, item_key) = classify_raw_response_item_completed(&notification.item);
            Some((
                ThreadId::from_string(&notification.thread_id).ok()?,
                ControllerEvent::ItemCompleted {
                    thread_id: notification.thread_id.clone(),
                    turn_id: notification.turn_id.clone(),
                    signal,
                    item_key,
                },
            ))
        }
        _ => None,
    }
}

fn classify_item_completed(item: &ThreadItem) -> CompletionSignal {
    match item {
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

fn classify_raw_response_item_completed(item: &ResponseItem) -> (CompletionSignal, Option<String>) {
    let signal = if raw_response_item_releases_after_any_item(item) {
        CompletionSignal::ReleasesAfterAnyItem
    } else {
        CompletionSignal::Ignore
    };
    (signal, raw_response_item_key(item))
}

fn raw_response_item_releases_after_any_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, content, .. } => {
            role == "assistant" && content.iter().any(content_item_has_text)
        }
        ResponseItem::Reasoning {
            summary, content, ..
        } => {
            summary.iter().any(reasoning_summary_has_text)
                || content
                    .as_ref()
                    .is_some_and(|content| content.iter().any(reasoning_content_has_text))
        }
        ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::GhostSnapshot { .. }
        | ResponseItem::Compaction { .. } => true,
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::Other => false,
    }
}

fn raw_response_item_key(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { id, .. } => id.clone(),
        ResponseItem::Reasoning { id, .. } => (!id.is_empty()).then(|| id.clone()),
        ResponseItem::LocalShellCall { id, call_id, .. } => call_id.clone().or_else(|| id.clone()),
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
        ResponseItem::ToolSearchCall { call_id, .. } => call_id.clone(),
        ResponseItem::WebSearchCall { id, .. } => id.clone(),
        ResponseItem::ImageGenerationCall { id, .. } => Some(id.clone()),
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.clone()),
        ResponseItem::ToolSearchOutput { call_id, .. } => call_id.clone(),
        ResponseItem::GhostSnapshot { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::Other => None,
    }
}

fn content_item_has_text(item: &ContentItem) -> bool {
    match item {
        ContentItem::OutputText { text } => !text.is_empty(),
        ContentItem::InputText { .. } | ContentItem::InputImage { .. } => false,
    }
}

fn reasoning_summary_has_text(item: &ReasoningItemReasoningSummary) -> bool {
    match item {
        ReasoningItemReasoningSummary::SummaryText { text } => !text.is_empty(),
    }
}

fn reasoning_content_has_text(item: &ReasoningItemContent) -> bool {
    match item {
        ReasoningItemContent::ReasoningText { text } | ReasoningItemContent::Text { text } => {
            !text.is_empty()
        }
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
    use codex_app_server_protocol::RawResponseItemCompletedNotification;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::Duration;
    use tokio::time::sleep;

    fn runtime() -> (
        StructuredInputRuntime,
        mpsc::UnboundedSender<StructuredInputReaderEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        (
            StructuredInputRuntime {
                receiver: rx,
                reader_task: None,
                reader_active: true,
                startup_system_prompt: None,
                startup_locked: false,
                unbound_messages: Vec::new(),
                controllers: HashMap::new(),
            },
            tx,
        )
    }

    #[test]
    fn first_bound_message_respects_existing_active_turn() {
        let thread_id = ThreadId::new();
        let (mut runtime, _tx) = runtime();

        let sync_actions = runtime.sync_active_turn(thread_id, "turn-1".to_string());
        let actions = runtime.handle_reader_event(
            StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "queued while busy".to_string(),
            })),
            Some(thread_id),
        );

        assert_eq!(sync_actions, vec![StructuredInputAction::RefreshPreview]);
        assert_eq!(actions, vec![StructuredInputAction::RefreshPreview]);
        assert_eq!(
            runtime.preview_for_thread(Some(thread_id)),
            vec![StructuredInputPreviewEntry {
                queue_mode: QueueMode::AfterToolCall,
                text: "queued while busy".to_string(),
            }]
        );
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
        tx.send(StructuredInputReaderEvent::StartupDrainComplete)
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

    #[tokio::test]
    async fn drain_startup_waits_for_initial_reader_drain_completion() {
        let (mut runtime, tx) = runtime();
        tokio::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            tx.send(StructuredInputReaderEvent::Parsed(
                ParsedXmlInput::SystemPrompt("be terse".to_string()),
            ))
            .unwrap();
            tx.send(StructuredInputReaderEvent::StartupDrainComplete)
                .unwrap();
        });

        let actions = runtime.drain_startup().await;

        assert_eq!(actions, Vec::<StructuredInputAction>::new());
        assert_eq!(
            runtime.startup_system_prompt(),
            Some("be terse".to_string())
        );
    }

    #[tokio::test]
    async fn drain_startup_returns_after_initial_reader_drain_without_events() {
        let (mut runtime, tx) = runtime();
        tokio::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            tx.send(StructuredInputReaderEvent::StartupDrainComplete)
                .unwrap();
        });

        let actions = runtime.drain_startup().await;

        assert_eq!(actions, Vec::<StructuredInputAction>::new());
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
    fn bind_unbound_after_any_item_message_starts_idle_thread() {
        let (mut runtime, _tx) = runtime();
        let thread_id = ThreadId::new();
        runtime.unbound_messages.push(QueuedMessage {
            queue_mode: QueueMode::AfterAnyItem,
            text: "hello".to_string(),
        });

        let actions = runtime.bind_unbound_messages(thread_id);

        assert_eq!(
            actions,
            vec![
                StructuredInputAction::RefreshPreview,
                StructuredInputAction::Release {
                    thread_id,
                    decision: ReleaseDecision {
                        action: codex_turn_start_bridge_core::ReleaseAction::StartTurn,
                        reason: ReleaseReason::Idle,
                        message: QueuedMessage {
                            queue_mode: QueueMode::AfterAnyItem,
                            text: "hello".to_string(),
                        },
                    },
                },
            ]
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

    #[test]
    fn reconcile_active_turn_updates_controller_state() {
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
        runtime.handle_controller_event(
            thread_id,
            ControllerEvent::MessageReceived(QueuedMessage {
                queue_mode: QueueMode::Immediate,
                text: "interrupt".to_string(),
            }),
        );

        let actions = runtime.reconcile_active_turn(thread_id, "turn-2".to_string());

        assert_eq!(actions, vec![StructuredInputAction::RefreshPreview]);
        assert_eq!(
            runtime.handle_controller_event(
                thread_id,
                ControllerEvent::SteerAccepted {
                    turn_id: "turn-2".to_string(),
                },
            ),
            vec![StructuredInputAction::RefreshPreview]
        );
        assert_eq!(
            runtime.handle_controller_event(
                thread_id,
                ControllerEvent::TurnStarted {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-2".to_string(),
                },
            ),
            vec![StructuredInputAction::RefreshPreview]
        );
    }

    #[test]
    fn classify_item_completed_releases_after_agent_message_items() {
        assert_eq!(
            classify_item_completed(&ThreadItem::AgentMessage {
                id: "item-1".to_string(),
                text: "hello".to_string(),
                phase: None,
                memory_citation: None,
            }),
            CompletionSignal::ReleasesAfterAnyItem
        );
    }

    #[test]
    fn classify_item_completed_ignores_user_message_items() {
        assert_eq!(
            classify_item_completed(&ThreadItem::UserMessage {
                id: "item-1".to_string(),
                content: Vec::new(),
            }),
            CompletionSignal::Ignore
        );
    }

    #[test]
    fn raw_assistant_message_completion_releases_after_any_item() {
        let thread_id = ThreadId::new();
        let mut runtime = running_runtime_with_after_any_item(thread_id);

        let actions = runtime.handle_server_notification(&raw_response_item_completed(
            thread_id,
            "turn-1",
            ResponseItem::Message {
                id: Some("msg-1".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ));

        assert!(has_after_any_item_release(&actions));
    }

    #[test]
    fn raw_reasoning_completion_releases_after_any_item() {
        let thread_id = ThreadId::new();
        let mut runtime = running_runtime_with_after_any_item(thread_id);

        let actions = runtime.handle_server_notification(&raw_response_item_completed(
            thread_id,
            "turn-1",
            ResponseItem::Reasoning {
                id: "reasoning-1".to_string(),
                summary: vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "thinking".to_string(),
                }],
                content: None,
                encrypted_content: None,
            },
        ));

        assert!(has_after_any_item_release(&actions));
    }

    #[test]
    fn raw_tool_call_completion_releases_after_any_item() {
        let thread_id = ThreadId::new();
        let mut runtime = running_runtime_with_after_any_item(thread_id);

        let actions = runtime.handle_server_notification(&raw_response_item_completed(
            thread_id,
            "turn-1",
            ResponseItem::FunctionCall {
                id: Some("call-item-1".to_string()),
                name: "shell".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call-1".to_string(),
            },
        ));

        assert!(has_after_any_item_release(&actions));
    }

    #[test]
    fn raw_user_message_completion_does_not_release_after_any_item() {
        let thread_id = ThreadId::new();
        let mut runtime = running_runtime_with_after_any_item(thread_id);

        let actions = runtime.handle_server_notification(&raw_response_item_completed(
            thread_id,
            "turn-1",
            ResponseItem::Message {
                id: Some("msg-1".to_string()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ));

        assert!(!has_after_any_item_release(&actions));
    }

    #[test]
    fn raw_tool_output_completion_does_not_release_after_any_item() {
        let thread_id = ThreadId::new();
        let mut runtime = running_runtime_with_after_any_item(thread_id);

        let actions = runtime.handle_server_notification(&raw_response_item_completed(
            thread_id,
            "turn-1",
            ResponseItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload::from_text("done".to_string()),
            },
        ));

        assert!(!has_after_any_item_release(&actions));
    }

    fn running_runtime_with_after_any_item(thread_id: ThreadId) -> StructuredInputRuntime {
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
                queue_mode: QueueMode::AfterAnyItem,
                text: "queued".to_string(),
            })),
            Some(thread_id),
        );
        runtime
    }

    fn raw_response_item_completed(
        thread_id: ThreadId,
        turn_id: &str,
        item: ResponseItem,
    ) -> ServerNotification {
        ServerNotification::RawResponseItemCompleted(RawResponseItemCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item,
        })
    }

    fn has_after_any_item_release(actions: &[StructuredInputAction]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                StructuredInputAction::Release {
                    decision: ReleaseDecision {
                        reason: ReleaseReason::AfterAnyItem,
                        ..
                    },
                    ..
                }
            )
        })
    }
}
