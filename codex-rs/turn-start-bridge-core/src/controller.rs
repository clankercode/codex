use crate::QueueMode;
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    pub queue_mode: QueueMode,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ControllerQueueSnapshot {
    pub pending_immediate: Vec<QueuedMessage>,
    pub steer_pending: Vec<QueuedMessage>,
    pub after_tool_call: Vec<QueuedMessage>,
    pub after_any_item: Vec<QueuedMessage>,
    pub next_turn: Vec<QueuedMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    TurnStartPending { reserved_turn_id: Option<String> },
    Running { turn_id: String },
    BusyUnknownTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionSignal {
    ReleasesAfterAnyItem,
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerEvent {
    MessageReceived(QueuedMessage),
    TurnStartAccepted,
    TurnStartAcceptedWithTurnId {
        turn_id: String,
    },
    TurnStartRejectedActiveTurnNotSteerable,
    TurnStarted {
        thread_id: String,
        turn_id: String,
    },
    ActiveTurnReconciled {
        thread_id: String,
        turn_id: String,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
    },
    TerminalInteraction {
        thread_id: String,
        turn_id: String,
    },
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        signal: CompletionSignal,
    },
    SteerRejectedActiveTurnNotSteerable {
        message: QueuedMessage,
    },
    SteerAccepted {
        turn_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleaseAction {
    StartTurn,
    SteerTurn { turn_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    Idle,
    Immediate,
    AfterToolCall,
    AfterAnyItem,
    NextTurn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseDecision {
    pub action: ReleaseAction,
    pub reason: ReleaseReason,
    pub message: QueuedMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeController {
    thread_id: String,
    turn_state: TurnState,
    pending_turn_message: Option<QueuedMessage>,
    pending_turn_completed_id: Option<String>,
    pending_steer_turn_id: Option<String>,
    pending_immediate_queue: VecDeque<QueuedMessage>,
    steer_pending_queue: VecDeque<QueuedMessage>,
    validation_error: Option<String>,
    after_tool_call_queue: VecDeque<QueuedMessage>,
    after_any_item_queue: VecDeque<QueuedMessage>,
    next_turn_queue: VecDeque<QueuedMessage>,
}

impl BridgeController {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_state: TurnState::Idle,
            pending_turn_message: None,
            pending_turn_completed_id: None,
            pending_steer_turn_id: None,
            pending_immediate_queue: VecDeque::new(),
            steer_pending_queue: VecDeque::new(),
            validation_error: None,
            after_tool_call_queue: VecDeque::new(),
            after_any_item_queue: VecDeque::new(),
            next_turn_queue: VecDeque::new(),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        match &self.turn_state {
            TurnState::Running { turn_id } => Some(turn_id.as_str()),
            _ => None,
        }
    }

    pub fn turn_state(&self) -> &TurnState {
        &self.turn_state
    }

    pub fn restore_busy_unknown_turn(&mut self) {
        self.turn_state = TurnState::BusyUnknownTurn;
    }

    pub fn recover_missing_active_turn(
        &mut self,
        message: QueuedMessage,
        reason: ReleaseReason,
    ) -> Option<ReleaseDecision> {
        self.pending_steer_turn_id = None;
        self.release_start(message, reason)
    }

    pub fn pending_turn_message(&self) -> Option<&QueuedMessage> {
        self.pending_turn_message.as_ref()
    }

    pub fn take_validation_error(&mut self) -> Option<String> {
        self.validation_error.take()
    }

    pub fn queued_counts(&self) -> (usize, usize, usize) {
        (
            self.after_tool_call_queue.len(),
            self.after_any_item_queue.len(),
            self.next_turn_queue.len(),
        )
    }

    pub fn queue_snapshot(&self) -> ControllerQueueSnapshot {
        ControllerQueueSnapshot {
            pending_immediate: self.pending_immediate_queue.iter().cloned().collect(),
            steer_pending: self.steer_pending_queue.iter().cloned().collect(),
            after_tool_call: self.after_tool_call_queue.iter().cloned().collect(),
            after_any_item: self.after_any_item_queue.iter().cloned().collect(),
            next_turn: self.next_turn_queue.iter().cloned().collect(),
        }
    }

    pub fn on_event(&mut self, event: ControllerEvent) -> Option<ReleaseDecision> {
        match event {
            ControllerEvent::MessageReceived(message) => self.on_message_received(message),
            ControllerEvent::TurnStartAccepted => self.on_turn_start_accepted(),
            ControllerEvent::TurnStartAcceptedWithTurnId { turn_id } => {
                self.on_turn_start_accepted_with_turn_id(turn_id)
            }
            ControllerEvent::TurnStartRejectedActiveTurnNotSteerable => {
                self.on_turn_start_rejected_active_turn_not_steerable()
            }
            ControllerEvent::TurnStarted { thread_id, turn_id } => {
                self.on_turn_started(thread_id, turn_id)
            }
            ControllerEvent::ActiveTurnReconciled { thread_id, turn_id } => {
                self.on_active_turn_reconciled(thread_id, turn_id)
            }
            ControllerEvent::TurnCompleted { thread_id, turn_id } => {
                self.on_turn_completed(thread_id, turn_id)
            }
            ControllerEvent::TerminalInteraction { thread_id, turn_id } => {
                self.on_terminal_interaction(thread_id, turn_id)
            }
            ControllerEvent::ItemCompleted {
                thread_id,
                turn_id,
                signal,
            } => self.on_item_completed(thread_id, turn_id, signal),
            ControllerEvent::SteerRejectedActiveTurnNotSteerable { message } => {
                self.on_steer_rejected_active_turn_not_steerable(message)
            }
            ControllerEvent::SteerAccepted { turn_id } => self.on_steer_accepted(turn_id),
        }
    }

    fn on_message_received(&mut self, message: QueuedMessage) -> Option<ReleaseDecision> {
        if matches!(self.turn_state, TurnState::Idle) {
            return self.release_start(message, ReleaseReason::Idle);
        }

        if matches!(message.queue_mode, QueueMode::Immediate)
            && matches!(
                self.turn_state,
                TurnState::TurnStartPending { .. } | TurnState::BusyUnknownTurn
            )
        {
            self.pending_immediate_queue.push_back(message);
            return None;
        }

        if self.pending_steer_turn_id.is_some() {
            self.steer_pending_queue.push_back(message);
            return None;
        }

        match message.queue_mode {
            QueueMode::Default | QueueMode::AfterToolCall => {
                self.after_tool_call_queue
                    .push_back(as_after_tool_call(message));
                None
            }
            QueueMode::AfterAnyItem => {
                self.after_any_item_queue.push_back(message);
                None
            }
            QueueMode::NextTurn => {
                self.next_turn_queue.push_back(message);
                None
            }
            QueueMode::Immediate => self.release_immediate_or_queue(message),
        }
    }

    fn on_turn_start_accepted(&mut self) -> Option<ReleaseDecision> {
        let TurnState::TurnStartPending { reserved_turn_id } = &self.turn_state else {
            return None;
        };

        if let Some(turn_id) = reserved_turn_id.clone() {
            if self.pending_turn_completed_id.as_deref() == Some(turn_id.as_str()) {
                self.pending_turn_message = None;
                self.pending_turn_completed_id = None;
                self.downgrade_pending_immediates_to_retry_queue();
                self.turn_state = TurnState::Idle;
                return self.release_from_turn_completed();
            }

            self.turn_state = TurnState::Running { turn_id };
            self.pending_turn_message = None;
            self.pending_steer_turn_id = None;
            if let Some(release) = self.flush_steer_pending_queue_for_current_state() {
                return Some(release);
            }

            return self.release_pending_immediate();
        }

        None
    }

    fn on_turn_start_accepted_with_turn_id(&mut self, turn_id: String) -> Option<ReleaseDecision> {
        let TurnState::TurnStartPending { reserved_turn_id } = &self.turn_state else {
            return None;
        };

        if let Some(reserved_turn_id) = reserved_turn_id
            && reserved_turn_id != &turn_id
        {
            self.validation_error = Some(format!(
                "turn/start response turn id `{turn_id}` did not match reserved turn id `{reserved_turn_id}`"
            ));
            return None;
        }

        let turn_id = reserved_turn_id.clone().unwrap_or(turn_id);

        if self.pending_turn_completed_id.as_deref() == Some(turn_id.as_str()) {
            self.pending_turn_message = None;
            self.pending_turn_completed_id = None;
            self.downgrade_pending_immediates_to_retry_queue();
            self.turn_state = TurnState::Idle;
            return self.release_from_turn_completed();
        }

        self.turn_state = TurnState::Running { turn_id };
        self.pending_turn_message = None;
        self.pending_turn_completed_id = None;
        self.pending_steer_turn_id = None;
        if let Some(release) = self.flush_steer_pending_queue_for_current_state() {
            return Some(release);
        }

        self.release_pending_immediate()
    }

    fn on_turn_start_rejected_active_turn_not_steerable(&mut self) -> Option<ReleaseDecision> {
        let TurnState::TurnStartPending { reserved_turn_id } = &self.turn_state else {
            return None;
        };
        let reserved_turn_id = reserved_turn_id.clone();

        if let Some(message) = self.pending_turn_message.take() {
            self.next_turn_queue.push_front(as_next_turn(message));
        }
        self.pending_steer_turn_id = None;

        if self.pending_turn_completed_id.is_some() {
            self.pending_turn_completed_id = None;
            self.downgrade_pending_immediates_to_retry_queue();
            self.turn_state = TurnState::Idle;
            return self.release_from_turn_completed();
        }

        if let Some(turn_id) = reserved_turn_id {
            self.turn_state = TurnState::Running { turn_id };
            return self.release_pending_immediate();
        } else {
            self.turn_state = TurnState::BusyUnknownTurn;
        }

        None
    }

    fn on_turn_started(&mut self, thread_id: String, turn_id: String) -> Option<ReleaseDecision> {
        if thread_id != self.thread_id {
            return None;
        }

        match &mut self.turn_state {
            TurnState::TurnStartPending {
                reserved_turn_id, ..
            } => {
                if reserved_turn_id.is_none() {
                    *reserved_turn_id = Some(turn_id);
                } else if reserved_turn_id.as_deref() != Some(turn_id.as_str()) {
                    self.validation_error = Some(format!(
                        "turn/started turn id `{turn_id}` did not match reserved turn id `{}`",
                        reserved_turn_id.as_deref().unwrap_or_default()
                    ));
                }
            }
            TurnState::BusyUnknownTurn => {
                self.turn_state = TurnState::Running { turn_id };
                return self.release_pending_immediate();
            }
            TurnState::Running {
                turn_id: active_turn_id,
            } => {
                if active_turn_id != &turn_id {
                    self.validation_error = Some(format!(
                        "turn/started turn id `{turn_id}` did not match active turn id `{active_turn_id}`"
                    ));
                }
            }
            TurnState::Idle => {
                self.turn_state = TurnState::Running { turn_id };
            }
        }

        None
    }

    fn on_active_turn_reconciled(
        &mut self,
        thread_id: String,
        turn_id: String,
    ) -> Option<ReleaseDecision> {
        if thread_id != self.thread_id {
            return None;
        }

        if self
            .pending_turn_completed_id
            .as_deref()
            .is_some_and(|id| id != turn_id)
        {
            self.pending_turn_completed_id = None;
        }

        match &mut self.turn_state {
            TurnState::Running {
                turn_id: active_turn_id,
            } => {
                *active_turn_id = turn_id.clone();
            }
            TurnState::BusyUnknownTurn => {
                self.turn_state = TurnState::Running {
                    turn_id: turn_id.clone(),
                };
            }
            TurnState::TurnStartPending { reserved_turn_id } => {
                *reserved_turn_id = Some(turn_id.clone());
            }
            TurnState::Idle => {
                self.turn_state = TurnState::Running {
                    turn_id: turn_id.clone(),
                };
            }
        }

        if self.pending_steer_turn_id.is_some() {
            self.pending_steer_turn_id = Some(turn_id);
        }

        None
    }

    fn on_turn_completed(&mut self, thread_id: String, turn_id: String) -> Option<ReleaseDecision> {
        if thread_id != self.thread_id {
            return None;
        }

        match &self.turn_state {
            TurnState::Running {
                turn_id: active_turn_id,
            } if active_turn_id != &turn_id => return None,
            TurnState::BusyUnknownTurn | TurnState::Running { .. } => {}
            TurnState::TurnStartPending { reserved_turn_id } => {
                if reserved_turn_id
                    .as_deref()
                    .is_none_or(|reserved_turn_id| reserved_turn_id == turn_id)
                {
                    self.pending_turn_completed_id = Some(turn_id);
                }
                return None;
            }
            _ => return None,
        }

        if self.pending_steer_turn_id.is_some() {
            self.pending_turn_completed_id = Some(turn_id);
            return None;
        }

        self.downgrade_pending_immediates_to_retry_queue();
        self.turn_state = TurnState::Idle;

        self.release_from_turn_completed()
    }

    fn on_steer_rejected_active_turn_not_steerable(
        &mut self,
        message: QueuedMessage,
    ) -> Option<ReleaseDecision> {
        self.pending_steer_turn_id = None;
        let message = as_next_turn(message);
        let completion_already_observed = self.pending_turn_completed_id.is_some();
        if completion_already_observed {
            self.next_turn_queue.push_front(message);
        } else {
            self.next_turn_queue.push_back(message);
        }
        self.flush_steer_pending_queue_as_buffered();
        self.downgrade_pending_immediates_to_retry_queue();

        if self.pending_turn_completed_id.take().is_some()
            && matches!(self.turn_state, TurnState::Running { .. })
        {
            self.turn_state = TurnState::Idle;
            return self.release_from_turn_completed();
        }

        if matches!(self.turn_state, TurnState::Idle) {
            self.pending_turn_completed_id = None;
            return self.release_from_turn_completed();
        }

        None
    }

    fn on_steer_accepted(&mut self, turn_id: String) -> Option<ReleaseDecision> {
        if self.pending_steer_turn_id.as_deref() != Some(turn_id.as_str()) {
            self.validation_error = Some(format!(
                "turn/steer response turn id `{turn_id}` did not match pending steer turn id `{}`",
                self.pending_steer_turn_id.as_deref().unwrap_or_default()
            ));
            self.pending_steer_turn_id = None;
            return None;
        }

        self.pending_steer_turn_id = None;

        if self.pending_turn_completed_id.take().is_some() {
            self.flush_steer_pending_queue_as_buffered();
            self.downgrade_pending_immediates_to_retry_queue();
            self.turn_state = TurnState::Idle;
            return self.release_from_turn_completed();
        }

        if let Some(release) = self.flush_steer_pending_queue_for_current_state() {
            return Some(release);
        }

        self.release_pending_immediate()
    }

    fn on_terminal_interaction(
        &mut self,
        thread_id: String,
        turn_id: String,
    ) -> Option<ReleaseDecision> {
        if !self.matches_active_turn(&thread_id, &turn_id) || self.pending_steer_turn_id.is_some() {
            return None;
        }

        if let Some(message) = self.after_tool_call_queue.pop_front() {
            self.pending_steer_turn_id = Some(turn_id.clone());
            return Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn { turn_id },
                reason: ReleaseReason::AfterToolCall,
                message,
            });
        }

        self.after_any_item_queue.pop_front().map(|message| {
            self.pending_steer_turn_id = Some(turn_id.clone());
            ReleaseDecision {
                action: ReleaseAction::SteerTurn { turn_id },
                reason: ReleaseReason::AfterAnyItem,
                message,
            }
        })
    }

    fn on_item_completed(
        &mut self,
        thread_id: String,
        turn_id: String,
        signal: CompletionSignal,
    ) -> Option<ReleaseDecision> {
        if signal != CompletionSignal::ReleasesAfterAnyItem
            || !self.matches_active_turn(&thread_id, &turn_id)
            || self.pending_steer_turn_id.is_some()
        {
            return None;
        }

        self.after_any_item_queue.pop_front().map(|message| {
            self.pending_steer_turn_id = Some(turn_id.clone());
            ReleaseDecision {
                action: ReleaseAction::SteerTurn { turn_id },
                reason: ReleaseReason::AfterAnyItem,
                message,
            }
        })
    }

    fn release_immediate_or_queue(&mut self, message: QueuedMessage) -> Option<ReleaseDecision> {
        let Some(turn_id) = self.active_turn_id().map(str::to_string) else {
            self.next_turn_queue.push_back(as_next_turn(message));
            return None;
        };

        if !matches!(self.turn_state, TurnState::Running { .. }) {
            self.next_turn_queue.push_back(as_next_turn(message));
            return None;
        }

        self.pending_steer_turn_id = Some(turn_id.clone());

        Some(ReleaseDecision {
            action: ReleaseAction::SteerTurn { turn_id },
            reason: ReleaseReason::Immediate,
            message,
        })
    }

    fn release_start(
        &mut self,
        message: QueuedMessage,
        reason: ReleaseReason,
    ) -> Option<ReleaseDecision> {
        let message = if matches!(reason, ReleaseReason::AfterToolCall) {
            as_after_tool_call(message)
        } else if matches!(reason, ReleaseReason::NextTurn) {
            as_next_turn(message)
        } else {
            message
        };

        self.pending_turn_message = Some(message.clone());
        self.pending_turn_completed_id = None;
        self.pending_steer_turn_id = None;
        self.turn_state = TurnState::TurnStartPending {
            reserved_turn_id: None,
        };
        Some(ReleaseDecision {
            action: ReleaseAction::StartTurn,
            reason,
            message,
        })
    }

    fn release_from_turn_completed(&mut self) -> Option<ReleaseDecision> {
        while let Some(message) = self.steer_pending_queue.pop_front() {
            self.route_buffered_message(message);
        }

        if let Some(message) = self.after_tool_call_queue.pop_front() {
            return self.release_start(message, ReleaseReason::AfterToolCall);
        }

        if let Some(message) = self.after_any_item_queue.pop_front() {
            return self.release_start(message, ReleaseReason::AfterAnyItem);
        }

        self.next_turn_queue
            .pop_front()
            .and_then(|message| self.release_start(message, ReleaseReason::NextTurn))
    }

    fn release_pending_immediate(&mut self) -> Option<ReleaseDecision> {
        if self.pending_steer_turn_id.is_some() {
            return None;
        }

        let turn_id = self.active_turn_id().map(str::to_string)?;
        let message = self.pending_immediate_queue.pop_front()?;

        self.pending_steer_turn_id = Some(turn_id.clone());
        Some(ReleaseDecision {
            action: ReleaseAction::SteerTurn { turn_id },
            reason: ReleaseReason::Immediate,
            message,
        })
    }

    fn downgrade_pending_immediates_to_retry_queue(&mut self) {
        while let Some(message) = self.pending_immediate_queue.pop_back() {
            self.next_turn_queue.push_front(as_next_turn(message));
        }
    }

    fn flush_steer_pending_queue_as_buffered(&mut self) {
        while let Some(message) = self.steer_pending_queue.pop_front() {
            self.route_buffered_message(message);
        }
    }

    fn flush_steer_pending_queue_for_current_state(&mut self) -> Option<ReleaseDecision> {
        let mut buffered = std::mem::take(&mut self.steer_pending_queue);
        let mut release = None;
        while let Some(message) = buffered.pop_front() {
            if release.is_none() {
                release = self.on_message_received(message);
            } else {
                let _ = self.on_message_received(message);
            }
        }

        release
    }

    fn route_buffered_message(&mut self, message: QueuedMessage) {
        match message.queue_mode {
            QueueMode::Default | QueueMode::AfterToolCall => {
                self.after_tool_call_queue
                    .push_back(as_after_tool_call(message));
            }
            QueueMode::AfterAnyItem => {
                self.after_any_item_queue.push_back(message);
            }
            QueueMode::NextTurn => {
                self.next_turn_queue.push_back(message);
            }
            QueueMode::Immediate => {
                self.next_turn_queue.push_back(as_next_turn(message));
            }
        }
    }

    fn matches_active_turn(&self, thread_id: &str, turn_id: &str) -> bool {
        thread_id == self.thread_id
            && matches!(&self.turn_state, TurnState::Running { turn_id: active_turn_id } if active_turn_id == turn_id)
    }
}

fn as_after_tool_call(mut message: QueuedMessage) -> QueuedMessage {
    message.queue_mode = QueueMode::AfterToolCall;
    message
}

fn as_next_turn(mut message: QueuedMessage) -> QueuedMessage {
    message.queue_mode = QueueMode::NextTurn;
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn queued(queue_mode: QueueMode, text: &str) -> QueuedMessage {
        QueuedMessage {
            queue_mode,
            text: text.to_string(),
        }
    }

    fn running_controller() -> BridgeController {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        controller
    }

    #[test]
    fn idle_messages_release_immediately_regardless_of_queue_mode() {
        for mode in [
            QueueMode::Default,
            QueueMode::Immediate,
            QueueMode::AfterToolCall,
            QueueMode::AfterAnyItem,
            QueueMode::NextTurn,
        ] {
            let mut controller = BridgeController::new("thread-1");

            assert_eq!(
                controller.on_event(ControllerEvent::MessageReceived(queued(mode, "hello"))),
                Some(ReleaseDecision {
                    action: ReleaseAction::StartTurn,
                    reason: ReleaseReason::Idle,
                    message: queued(mode, "hello"),
                })
            );
            assert_eq!(
                controller.turn_state(),
                &TurnState::TurnStartPending {
                    reserved_turn_id: None,
                }
            );
            assert_eq!(
                controller.pending_turn_message(),
                Some(&queued(mode, "hello"))
            );
        }
    }

    #[test]
    fn turn_start_pending_binds_reserved_turn_before_request_succeeds() {
        let mut controller = BridgeController::new("thread-1");
        let start = controller.on_event(ControllerEvent::MessageReceived(queued(
            QueueMode::AfterToolCall,
            "hello",
        )));

        assert_eq!(
            start,
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: Some("turn-1".to_string()),
            }
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAccepted),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::Running {
                turn_id: "turn-1".to_string(),
            }
        );
        assert_eq!(controller.active_turn_id(), Some("turn-1"));
        assert_eq!(controller.pending_turn_message(), None);
    }

    #[test]
    fn terminal_interaction_releases_exactly_one_message_and_prefers_after_tool_call() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool-1",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool-2",
            ))),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::AfterToolCall,
                message: queued(QueueMode::AfterToolCall, "tool-1"),
            })
        );
        assert_eq!(controller.queued_counts(), (1, 1, 0));
    }

    #[test]
    fn terminal_interaction_does_not_release_another_message_while_steer_is_pending() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool-1",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool-2",
            ))),
            None
        );

        assert!(matches!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                reason: ReleaseReason::AfterToolCall,
                ..
            })
        ));
        assert_eq!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(controller.queued_counts(), (1, 0, 0));
    }

    #[test]
    fn item_completed_releases_after_any_item_but_not_terminal_interaction_only_items() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any",
            ))),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                signal: CompletionSignal::Ignore,
            }),
            None
        );
        assert_eq!(controller.queued_counts(), (1, 1, 0));
        assert_eq!(
            controller.on_event(ControllerEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                signal: CompletionSignal::ReleasesAfterAnyItem,
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::AfterAnyItem,
                message: queued(QueueMode::AfterAnyItem, "any"),
            })
        );
        assert_eq!(controller.queued_counts(), (1, 0, 0));
    }

    #[test]
    fn item_completed_does_not_release_another_message_while_steer_is_pending() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any-1",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any-2",
            ))),
            None
        );

        assert!(matches!(
            controller.on_event(ControllerEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                signal: CompletionSignal::ReleasesAfterAnyItem,
            }),
            Some(ReleaseDecision {
                reason: ReleaseReason::AfterAnyItem,
                ..
            })
        ));
        assert_eq!(
            controller.on_event(ControllerEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                signal: CompletionSignal::ReleasesAfterAnyItem,
            }),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 1, 0));
    }

    #[test]
    fn after_any_item_includes_terminal_interaction_fallback() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any",
            ))),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::AfterAnyItem,
                message: queued(QueueMode::AfterAnyItem, "any"),
            })
        );
    }

    #[test]
    fn immediate_steers_same_turn_and_downgrades_only_on_explicit_failure() {
        let mut controller = running_controller();

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "interrupt",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "interrupt"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));

        assert_eq!(
            controller.on_event(ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                message: queued(QueueMode::Immediate, "interrupt"),
            }),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 0, 1));
    }

    #[test]
    fn immediate_during_turn_start_pending_is_retained_for_same_turn_release() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "interrupt",
            ))),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAccepted),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "interrupt"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));
    }

    #[test]
    fn immediate_during_turn_start_pending_is_retained_across_start_rejection_when_turn_id_is_known()
     {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "interrupt",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartRejectedActiveTurnNotSteerable),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "interrupt"),
            })
        );
    }

    #[test]
    fn immediate_during_busy_unknown_turn_is_retained_until_turn_id_is_known() {
        let mut controller = BridgeController::new("thread-1");
        controller.restore_busy_unknown_turn();

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "interrupt",
            ))),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "interrupt"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));
    }

    #[test]
    fn turn_completed_uses_fallback_priority_after_tool_call_then_after_any_item_then_next_turn() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::NextTurn,
                "next",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterAnyItem,
                "any",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool",
            ))),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::AfterToolCall,
                message: queued(QueueMode::AfterToolCall, "tool"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 1, 1));
    }

    #[test]
    fn running_controller_reports_mismatched_turn_started_notifications() {
        let mut controller = running_controller();

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.take_validation_error(),
            Some("turn/started turn id `turn-2` did not match active turn id `turn-1`".to_string())
        );
    }

    #[test]
    fn reconciled_active_turn_updates_pending_steer_target_and_avoids_later_mismatch() {
        let mut controller = running_controller();

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "interrupt",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "interrupt"),
            })
        );

        assert_eq!(
            controller.on_event(ControllerEvent::ActiveTurnReconciled {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::Running {
                turn_id: "turn-2".to_string(),
            }
        );

        assert_eq!(
            controller.on_event(ControllerEvent::SteerAccepted {
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(controller.take_validation_error(), None);
    }

    #[test]
    fn completion_before_steer_error_keeps_older_message_ahead_of_later_next_turn_work() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::NextTurn,
                "later-next",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "older-immediate",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "older-immediate"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::Running {
                turn_id: "turn-1".to_string()
            }
        );

        assert_eq!(
            controller.on_event(ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                message: queued(QueueMode::Immediate, "older-immediate"),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "older-immediate"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 0, 1));
    }

    #[test]
    fn completion_before_pending_steer_response_does_not_treat_new_input_as_idle() {
        let mut controller = running_controller();
        assert!(matches!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "older-immediate",
            ))),
            Some(ReleaseDecision {
                reason: ReleaseReason::Immediate,
                ..
            })
        ));
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "later-tool",
            ))),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::Running {
                turn_id: "turn-1".to_string()
            }
        );
        assert_eq!(controller.pending_turn_message(), None);
    }

    #[test]
    fn completion_before_after_tool_call_steer_error_keeps_older_message_ahead_of_later_next_turn_work()
     {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "older-tool",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::NextTurn,
                "later-next",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::AfterToolCall,
                message: queued(QueueMode::AfterToolCall, "older-tool"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                message: queued(QueueMode::AfterToolCall, "older-tool"),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "older-tool"),
            })
        );
        assert_eq!(controller.queued_counts(), (0, 0, 1));
    }

    #[test]
    fn immediate_during_pending_steer_retries_same_turn_after_prior_steer_succeeds() {
        let mut controller = running_controller();
        assert!(matches!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "first",
            ))),
            Some(ReleaseDecision {
                reason: ReleaseReason::Immediate,
                ..
            })
        ));

        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "second",
            ))),
            None
        );
        assert_eq!(controller.queued_counts(), (0, 0, 0));

        assert_eq!(
            controller.on_event(ControllerEvent::SteerAccepted {
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "second"),
            })
        );
    }

    #[test]
    fn downgrade_of_multiple_retained_immediates_preserves_fifo_order() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "start",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "start"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "first",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "second",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAcceptedWithTurnId {
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "first"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAcceptedWithTurnId {
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "second"),
            })
        );
    }

    #[test]
    fn turn_start_accept_response_reports_mismatched_reserved_turn_id() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAcceptedWithTurnId {
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.take_validation_error(),
            Some(
                "turn/start response turn id `turn-2` did not match reserved turn id `turn-1`"
                    .to_string()
            )
        );
    }

    #[test]
    fn active_turn_not_steerable_during_start_moves_retained_message_to_next_turn_queue() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::Immediate, "hello"),
            })
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartRejectedActiveTurnNotSteerable),
            None
        );
        assert_eq!(controller.turn_state(), &TurnState::BusyUnknownTurn);
        assert_eq!(controller.pending_turn_message(), None);
        assert_eq!(controller.queued_counts(), (0, 0, 1));
    }

    #[test]
    fn pending_turn_completion_before_start_acceptance_releases_next_queued_message() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::NextTurn,
                "next",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAccepted),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "next"),
            })
        );
    }

    #[test]
    fn steer_rejection_after_turn_completion_starts_next_turn_immediately() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            None
        );
        assert_eq!(controller.turn_state(), &TurnState::Idle);

        assert_eq!(
            controller.on_event(ControllerEvent::SteerRejectedActiveTurnNotSteerable {
                message: queued(QueueMode::Immediate, "retry-me"),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::NextTurn,
                message: queued(QueueMode::NextTurn, "retry-me"),
            })
        );
    }

    #[test]
    fn start_rejection_preserves_queue_priority_and_next_turn_order() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "retry-me",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::Immediate, "retry-me"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::NextTurn,
                "older-next",
            ))),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "tool",
            ))),
            None
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartRejectedActiveTurnNotSteerable),
            None
        );
        assert_eq!(controller.queued_counts(), (1, 0, 2));

        let _ = controller.on_event(ControllerEvent::TurnStarted {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        });

        assert_eq!(
            controller.on_event(ControllerEvent::TurnCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::AfterToolCall,
                message: queued(QueueMode::AfterToolCall, "tool"),
            })
        );

        assert_eq!(controller.queued_counts(), (0, 0, 2));
    }

    #[test]
    fn turn_start_accepted_without_reserved_turn_keeps_pending_state() {
        let mut controller = BridgeController::new("thread-1");
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::AfterToolCall,
                "hello",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Idle,
                message: queued(QueueMode::AfterToolCall, "hello"),
            })
        );

        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAccepted),
            None
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: None,
            }
        );
        assert_eq!(
            controller.pending_turn_message(),
            Some(&queued(QueueMode::AfterToolCall, "hello"))
        );
    }

    #[test]
    fn recover_missing_active_turn_releases_start_turn_for_inflight_steer_message() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "steer-me",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            })
        );

        let release = controller
            .recover_missing_active_turn(
                queued(QueueMode::Immediate, "steer-me"),
                ReleaseReason::Immediate,
            )
            .expect("missing active turn should start a new turn");

        assert_eq!(
            release,
            ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            }
        );
        assert_eq!(
            controller.turn_state(),
            &TurnState::TurnStartPending {
                reserved_turn_id: None,
            }
        );
    }

    #[test]
    fn recover_missing_active_turn_releases_buffered_immediate_after_fresh_turn_starts() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "steer-me",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "follow-up",
            ))),
            None
        );

        assert_eq!(
            controller.recover_missing_active_turn(
                queued(QueueMode::Immediate, "steer-me"),
                ReleaseReason::Immediate,
            ),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAcceptedWithTurnId {
                turn_id: "turn-2".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-2".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "follow-up"),
            })
        );
    }

    #[test]
    fn recover_missing_active_turn_routes_buffered_default_into_fresh_turn() {
        let mut controller = running_controller();
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Immediate,
                "steer-me",
            ))),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-1".to_string(),
                },
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::MessageReceived(queued(
                QueueMode::Default,
                "buffered-default",
            ))),
            None
        );

        assert_eq!(
            controller.recover_missing_active_turn(
                queued(QueueMode::Immediate, "steer-me"),
                ReleaseReason::Immediate,
            ),
            Some(ReleaseDecision {
                action: ReleaseAction::StartTurn,
                reason: ReleaseReason::Immediate,
                message: queued(QueueMode::Immediate, "steer-me"),
            })
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TurnStartAcceptedWithTurnId {
                turn_id: "turn-2".to_string(),
            }),
            None
        );
        assert_eq!(
            controller.on_event(ControllerEvent::TerminalInteraction {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-2".to_string(),
            }),
            Some(ReleaseDecision {
                action: ReleaseAction::SteerTurn {
                    turn_id: "turn-2".to_string(),
                },
                reason: ReleaseReason::AfterToolCall,
                message: queued(QueueMode::AfterToolCall, "buffered-default"),
            })
        );
    }
}
