use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Local;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

use crate::status_indicator_widget::fmt_elapsed_compact;

const IDLE_NOTE_THRESHOLD: Duration = Duration::from_secs(10);
const STATUS_LINE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdleTimingState {
    injection_enabled: bool,
    last_model_turn_completed_at: Option<DateTime<Local>>,
    last_turn_duration: Option<Duration>,
    model_at_last_model_turn: Option<String>,
    in_flight_turn_started_at: Option<Instant>,
    last_steer_user_message_at: Option<Instant>,
    current_turn_started_by_user_message: bool,
}

impl Default for IdleTimingState {
    fn default() -> Self {
        Self {
            injection_enabled: true,
            last_model_turn_completed_at: None,
            last_turn_duration: None,
            model_at_last_model_turn: None,
            in_flight_turn_started_at: None,
            last_steer_user_message_at: None,
            current_turn_started_by_user_message: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedIdleTimingSubmission {
    pub(crate) developer_message: String,
    pub(crate) resume_note: Option<String>,
}

impl PreparedIdleTimingSubmission {
    pub(crate) fn developer_message_item(&self) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: self.developer_message.clone(),
            }],
            end_turn: None,
            phase: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdleStatusLineValue {
    pub(crate) text: String,
    pub(crate) refresh_in: Duration,
}

impl IdleTimingState {
    pub(crate) fn injection_enabled(&self) -> bool {
        self.injection_enabled
    }

    pub(crate) fn set_injection_enabled(&mut self, enabled: bool) {
        self.injection_enabled = enabled;
    }

    pub(crate) fn prepare_turn_start_submission(
        &self,
        now: DateTime<Local>,
    ) -> Option<PreparedIdleTimingSubmission> {
        if !self.injection_enabled || self.in_flight_turn_started_at.is_some() {
            return None;
        }

        let idle_since_last_turn = self.idle_since_last_model_turn(now);
        idle_since_last_turn?;
        Some(PreparedIdleTimingSubmission {
            developer_message: format_timing_block(
                now,
                idle_since_last_turn,
                self.last_turn_duration,
            ),
            resume_note: idle_since_last_turn.and_then(format_idle_resume_note),
        })
    }

    pub(crate) fn begin_turn(&mut self, started_at: Instant) {
        if self.in_flight_turn_started_at.is_none() {
            self.in_flight_turn_started_at = Some(started_at);
            self.current_turn_started_by_user_message = false;
        }
    }

    pub(crate) fn record_turn_start_user_message(&mut self, submitted_at: Instant) {
        self.in_flight_turn_started_at = Some(submitted_at);
        self.last_steer_user_message_at = None;
        self.current_turn_started_by_user_message = true;
    }

    pub(crate) fn record_steer_user_message(&mut self, _submitted_at: Instant) {
        let injected_at = Instant::now();
        if self.in_flight_turn_started_at.is_none() {
            self.in_flight_turn_started_at = Some(injected_at);
        }
        self.last_steer_user_message_at = Some(injected_at);
        self.current_turn_started_by_user_message = true;
    }

    pub(crate) fn complete_turn(
        &mut self,
        model: &str,
        completed_at: DateTime<Local>,
    ) -> Option<Duration> {
        let duration = self
            .in_flight_turn_started_at
            .map(|started_at| started_at.elapsed());
        self.last_turn_duration = duration;
        self.last_model_turn_completed_at = Some(completed_at);
        self.model_at_last_model_turn = Some(model.to_string());
        self.in_flight_turn_started_at = None;
        self.last_steer_user_message_at = None;
        let turn_started_by_user_message = self.current_turn_started_by_user_message;
        self.current_turn_started_by_user_message = false;
        turn_started_by_user_message.then_some(duration).flatten()
    }

    pub(crate) fn reset_for_compaction(&mut self, now: DateTime<Local>) {
        self.last_model_turn_completed_at = Some(now);
        self.model_at_last_model_turn = None;
        self.in_flight_turn_started_at = None;
        self.last_steer_user_message_at = None;
        self.current_turn_started_by_user_message = false;
    }

    pub(crate) fn status_line_value(
        &self,
        current_model: &str,
        now: DateTime<Local>,
    ) -> Option<IdleStatusLineValue> {
        self.status_line_value_at(current_model, now, Instant::now())
    }

    pub(crate) fn status_line_value_at(
        &self,
        current_model: &str,
        now: DateTime<Local>,
        now_instant: Instant,
    ) -> Option<IdleStatusLineValue> {
        if let Some(started_at) = self.in_flight_turn_started_at {
            let runtime = now_instant.saturating_duration_since(started_at);
            let mut text = format!("Run {}", fmt_elapsed_compact(runtime.as_secs()));
            if let Some(steer_at) = self.last_steer_user_message_at {
                let steer_age = now_instant.saturating_duration_since(steer_at);
                text.push_str(&format!(
                    " · Steer {}",
                    fmt_elapsed_compact(steer_age.as_secs())
                ));
            }
            return Some(IdleStatusLineValue {
                text,
                refresh_in: STATUS_LINE_REFRESH_INTERVAL,
            });
        }

        if self.current_turn_started_by_user_message {
            return None;
        }

        let idle = self.idle_since_last_model_turn(now)?;
        let text = if let Some(model) = self.model_at_last_model_turn.as_deref() {
            if model != current_model {
                "Idle ---".to_string()
            } else {
                format!("Idle {}", fmt_elapsed_compact(idle.as_secs()))
            }
        } else {
            format!("Idle {}", fmt_elapsed_compact(idle.as_secs()))
        };
        Some(IdleStatusLineValue {
            text,
            refresh_in: STATUS_LINE_REFRESH_INTERVAL,
        })
    }

    fn idle_since_last_model_turn(&self, now: DateTime<Local>) -> Option<Duration> {
        let last = self.last_model_turn_completed_at?;
        now.signed_duration_since(last).to_std().ok()
    }
}

fn format_timing_block(
    now: DateTime<Local>,
    idle_since_last_turn: Option<Duration>,
    last_turn_duration: Option<Duration>,
) -> String {
    let mut lines = vec![
        "[timing]".to_string(),
        format!("time={}", now.format("%Y-%m-%dT%H:%M:%S%:z")),
    ];
    push_duration_line(&mut lines, "idle_for", idle_since_last_turn);
    push_duration_line(&mut lines, "last_turn", last_turn_duration);
    lines.push("[/timing]".to_string());
    lines.join("\n")
}

fn push_duration_line(lines: &mut Vec<String>, key: &str, duration: Option<Duration>) {
    if let Some(duration) = duration {
        lines.push(format!("{key}={:.1}s", duration.as_secs_f64()));
    }
}

fn format_idle_resume_note(idle: Duration) -> Option<String> {
    if idle <= IDLE_NOTE_THRESHOLD {
        return None;
    }

    let total_seconds = idle.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 || hours > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    Some(format!("[After {}]", parts.join(" ")))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn local_ts(ts: &str) -> DateTime<Local> {
        chrono::DateTime::parse_from_rfc3339(ts)
            .expect("timestamp")
            .with_timezone(&Local)
    }

    #[test]
    fn disabled_injection_skips_submission_payload() {
        let mut state = IdleTimingState::default();
        state.set_injection_enabled(false);

        assert_eq!(
            state.prepare_turn_start_submission(local_ts("2026-04-18T12:00:00+10:00")),
            None
        );
    }

    #[test]
    fn first_turn_skips_submission_payload_without_prior_completion() {
        let state = IdleTimingState::default();

        assert_eq!(
            state.prepare_turn_start_submission(local_ts("2026-04-18T12:00:00+10:00")),
            None
        );
    }

    #[test]
    fn timing_block_includes_idle_and_last_turn_when_available() {
        let state = IdleTimingState {
            last_model_turn_completed_at: Some(local_ts("2026-04-18T12:00:00+10:00")),
            last_turn_duration: Some(Duration::from_millis(4_321)),
            ..Default::default()
        };

        let submission = state
            .prepare_turn_start_submission(local_ts("2026-04-18T12:00:14.900+10:00"))
            .expect("submission");

        assert_eq!(
            submission.developer_message,
            [
                "[timing]",
                "time=2026-04-18T12:00:14+10:00",
                "idle_for=14.9s",
                "last_turn=4.3s",
                "[/timing]",
            ]
            .join("\n")
        );
        assert_eq!(submission.resume_note, Some("[After 14s]".to_string()));
    }

    #[test]
    fn active_turn_skips_idle_timing_submission() {
        let mut state = IdleTimingState {
            last_model_turn_completed_at: Some(local_ts("2026-04-18T12:00:00+10:00")),
            last_turn_duration: Some(Duration::from_millis(4_321)),
            ..Default::default()
        };
        state.record_turn_start_user_message(Instant::now());

        assert_eq!(
            state.prepare_turn_start_submission(local_ts("2026-04-18T12:00:14.900+10:00")),
            None
        );
    }

    #[test]
    fn status_line_shows_running_turn_and_recent_steer() {
        let mut state = IdleTimingState::default();
        let base = Instant::now();
        state.record_turn_start_user_message(base - Duration::from_secs(95));
        state.record_steer_user_message(base - Duration::from_secs(7));

        let display = state
            .status_line_value_at("gpt-5.4", local_ts("2026-04-18T12:00:00+10:00"), base)
            .expect("display");

        assert_eq!(display.text, "Run 1m 35s · Steer 0s");
        assert_eq!(display.refresh_in, Duration::from_secs(1));
    }

    #[test]
    fn completed_turn_resets_steer_status_line_state() {
        let mut state = IdleTimingState::default();
        let now = Instant::now();
        state.record_turn_start_user_message(now - Duration::from_secs(95));
        state.record_steer_user_message(now - Duration::from_secs(7));

        state.complete_turn("gpt-5.4", local_ts("2026-04-18T12:00:00+10:00"));

        let display = state
            .status_line_value_at("gpt-5.4", local_ts("2026-04-18T12:00:12+10:00"), now)
            .expect("display");

        assert_eq!(display.text, "Idle 12s");
    }

    #[test]
    fn status_line_blanks_when_model_changes() {
        let state = IdleTimingState {
            last_model_turn_completed_at: Some(local_ts("2026-04-18T12:00:00+10:00")),
            model_at_last_model_turn: Some("gpt-5.4".to_string()),
            ..Default::default()
        };

        let display = state
            .status_line_value("gpt-5.4-mini", local_ts("2026-04-18T12:01:05+10:00"))
            .expect("display");

        assert_eq!(display.text, "Idle ---");
        assert_eq!(display.refresh_in, Duration::from_secs(1));
    }

    #[test]
    fn compaction_resets_idle_baseline_without_clearing_last_turn_duration() {
        let mut state = IdleTimingState {
            last_turn_duration: Some(Duration::from_millis(4_321)),
            model_at_last_model_turn: Some("gpt-5.4".to_string()),
            ..Default::default()
        };

        let now = local_ts("2026-04-18T12:00:00+10:00");
        state.reset_for_compaction(now);

        assert_eq!(state.model_at_last_model_turn, None);
        assert_eq!(state.last_turn_duration, Some(Duration::from_millis(4_321)));
        assert_eq!(
            state
                .status_line_value("gpt-5.4-mini", local_ts("2026-04-18T12:00:45+10:00"))
                .expect("display")
                .text,
            "Idle 45s"
        );
    }

    #[test]
    fn complete_turn_records_duration_and_model() {
        let mut state = IdleTimingState::default();
        let started_at = Instant::now() - Duration::from_secs(8);
        state.begin_turn(started_at);

        state.complete_turn("gpt-5.4", local_ts("2026-04-18T12:00:00+10:00"));

        assert_eq!(state.model_at_last_model_turn, Some("gpt-5.4".to_string()));
        assert!(state.last_turn_duration.is_some());
        assert!(state.in_flight_turn_started_at.is_none());
    }
}
