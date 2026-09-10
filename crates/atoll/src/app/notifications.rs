//! Completion policy is independent of the Windows delivery mechanism.

use std::collections::HashMap;

use atoll_core::state::{Phase, SessionState, SessionTable};

mod desktop;
pub use desktop::Notifier;

const MIN_RUN_SECS: u64 = 30;

#[derive(Clone, Debug)]
pub struct Completion {
    pub session_id: String,
    pub title: String,
    pub body: String,
}

struct Seen {
    phase: Phase,
    started: u64,
    last_event: String,
    last_seen: u64,
}

#[derive(Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
}

impl Tracker {
    /// Observe even when disabled or watched: turning notifications on must
    /// never replay old completions. A first snapshot only establishes a baseline.
    pub fn observe(
        &mut self,
        table: &SessionTable,
        now: u64,
        enabled: bool,
        mut watched: impl FnMut(&SessionState) -> bool,
    ) -> Vec<Completion> {
        let mut notices = Vec::new();
        self.seen.retain(|id, _| table.get(id).is_some());
        for state in table.sessions() {
            let old = self.seen.get(&state.session_id);
            let new_turn = old.is_some_and(|old| {
                state.phase != Phase::Completed
                    && (old.phase == Phase::Completed
                        || (state.last_seen > old.last_seen
                            && matches!(
                                state.last_event.as_str(),
                                "SessionStart"
                                    | "UserPromptSubmit"
                                    | "task_started"
                                    | "user_message"
                            )))
            });
            let started = if new_turn {
                now
            } else {
                old.map(|old| old.started).unwrap_or(now)
            };
            let just_finished = state.phase == Phase::Completed
                && old.is_some_and(|old| old.phase != Phase::Completed);
            let cancelled = matches!(
                state.last_event.as_str(),
                "Interrupt"
                    | "turn_aborted"
                    | "SessionEnd"
                    | "turn_failed"
                    | "session_disconnected"
            );
            if just_finished
                && !cancelled
                && enabled
                && now.saturating_sub(started) >= MIN_RUN_SECS
                && !watched(state)
            {
                let project = state
                    .cwd
                    .as_deref()
                    .map(crate::util::project_name)
                    .unwrap_or_else(|| "Session".into());
                notices.push(Completion {
                    session_id: state.session_id.clone(),
                    title: format!(
                        "{} · {}",
                        if state.source == atoll_core::protocol::HookSource::Claude {
                            "Claude Code"
                        } else {
                            "Codex"
                        },
                        crate::util::truncate(&crate::util::one_line(&project), 60)
                    ),
                    body: "Task finished. Click to view the session.".into(),
                });
            }
            // Ignore unchanged snapshots without allocating a new event string.
            if old.is_none_or(|old| {
                old.phase != state.phase
                    || old.last_seen != state.last_seen
                    || old.last_event != state.last_event
            }) {
                self.seen.insert(
                    state.session_id.clone(),
                    Seen {
                        phase: state.phase,
                        started,
                        last_event: state.last_event.clone(),
                        last_seen: state.last_seen,
                    },
                );
            }
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::protocol::{HookPayload, HookSource};
    fn event(table: &mut SessionTable, name: &str, at: u64) {
        let payload: HookPayload = serde_json::from_value(serde_json::json!({"session_id":"test", "hook_event_name":name,"cwd":"C:/projects/demo"})).unwrap();
        table.apply(&payload, HookSource::Codex, at);
    }
    #[test]
    fn only_long_unwatched_completions_notify_once() {
        let mut table = SessionTable::new();
        let mut tracker = Tracker::default();
        event(&mut table, "Stop", 1);
        assert!(tracker.observe(&table, 1, true, |_| false).is_empty());
        event(&mut table, "UserPromptSubmit", 10);
        tracker.observe(&table, 10, true, |_| false);
        event(&mut table, "Stop", 50);
        assert_eq!(tracker.observe(&table, 50, true, |_| false).len(), 1);
        assert!(tracker.observe(&table, 51, true, |_| false).is_empty());
        for (start, enabled, watched, end) in [
            (60, true, false, "Interrupt"),
            (110, true, true, "Stop"),
            (160, false, false, "Stop"),
        ] {
            event(&mut table, "UserPromptSubmit", start);
            tracker.observe(&table, start, enabled, |_| watched);
            event(&mut table, end, start + 40);
            assert!(
                tracker
                    .observe(&table, start + 40, enabled, |_| watched)
                    .is_empty()
            );
            assert!(
                tracker
                    .observe(&table, start + 41, true, |_| false)
                    .is_empty()
            );
        }
        event(&mut table, "UserPromptSubmit", 210);
        tracker.observe(&table, 210, true, |_| false);
        event(&mut table, "Stop", 211);
        assert!(tracker.observe(&table, 211, true, |_| false).is_empty());
    }
    #[test]
    fn new_turn_while_running_restarts_the_duration() {
        let mut table = SessionTable::new();
        let mut tracker = Tracker::default();
        event(&mut table, "UserPromptSubmit", 1);
        tracker.observe(&table, 1, true, |_| false);
        event(&mut table, "UserPromptSubmit", 50);
        tracker.observe(&table, 50, true, |_| false);
        event(&mut table, "Stop", 51);
        assert!(tracker.observe(&table, 51, true, |_| false).is_empty());
    }

    #[test]
    fn desktop_failure_and_disconnect_do_not_send_success_notifications() {
        for end in ["turn_failed", "session_disconnected"] {
            let mut table = SessionTable::new();
            let mut tracker = Tracker::default();
            event(&mut table, "UserPromptSubmit", 1);
            tracker.observe(&table, 1, true, |_| false);
            event(&mut table, "Stop", 40);
            table.get_mut("test").unwrap().last_event = end.into();
            assert!(tracker.observe(&table, 40, true, |_| false).is_empty());
        }
    }
}
