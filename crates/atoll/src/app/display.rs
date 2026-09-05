//! Persist the last display and resume it only when an agent produces activity.
//! Quota caches and network replies are readings, never signs of life.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use atoll_core::protocol::HookSource;
use atoll_core::state::{AgentTasks, STALE_AFTER_SECS};
use serde::{Deserialize, Serialize};

use super::{AGENTS, config, ui};
use crate::usage_cache::UsageSnapshot;

const SAVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AgentState {
    last_seen: Option<u64>,
    visible: bool,
    ended: bool,
}

/// Display text only. Approval connections and terminal targets must come from
/// live hooks, so neither is restored from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedSession {
    id: String,
    title: String,
    detail: String,
    phase: String,
    source: String,
}

impl From<ui::SessionRow> for SavedSession {
    fn from(row: ui::SessionRow) -> Self {
        Self {
            id: row.id.to_string(),
            title: row.title.to_string(),
            detail: row.detail.to_string(),
            phase: row.phase.to_string(),
            source: row.source.to_string(),
        }
    }
}

impl SavedSession {
    fn row(&self) -> ui::SessionRow {
        ui::SessionRow {
            id: self.id.clone().into(),
            title: self.title.clone().into(),
            detail: self.detail.clone().into(),
            phase: self.phase.clone().into(),
            source: self.source.clone().into(),
            jumpable: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Snapshot {
    claude: AgentState,
    codex: AgentState,
    usage: UsageSnapshot,
    sessions: Vec<SavedSession>,
    updated_at: u64,
}

pub struct DisplayState {
    snapshot: Snapshot,
    live: bool,
    dirty: bool,
    last_save: Option<Instant>,
}

impl DisplayState {
    pub fn load() -> Self {
        let snapshot = config::config_dir()
            .ok()
            .and_then(|dir| read_snapshot(&dir.join("display.json")))
            .unwrap_or_default();
        Self::restore(snapshot)
    }

    fn restore(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            live: false,
            dirty: false,
            last_save: None,
        }
    }

    fn agent(&self, source: HookSource) -> &AgentState {
        match source {
            HookSource::Claude => &self.snapshot.claude,
            HookSource::Codex => &self.snapshot.codex,
        }
    }

    fn agent_mut(&mut self, source: HookSource) -> &mut AgentState {
        match source {
            HookSource::Claude => &mut self.snapshot.claude,
            HookSource::Codex => &mut self.snapshot.codex,
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn visible(&self, source: HookSource) -> bool {
        self.agent(source).visible
    }

    pub fn visible_agents(&self) -> Vec<HookSource> {
        AGENTS
            .into_iter()
            .filter(|agent| self.visible(*agent))
            .collect()
    }

    pub fn usage(&self) -> UsageSnapshot {
        self.snapshot.usage.clone()
    }

    pub fn reading_time(&self, now: u64) -> u64 {
        if self.live {
            now
        } else {
            self.snapshot.updated_at
        }
    }

    pub fn saved_sessions(&self, limit: usize) -> Vec<ui::SessionRow> {
        self.snapshot
            .sessions
            .iter()
            .take(limit)
            .map(SavedSession::row)
            .collect()
    }

    pub fn saved_tasks(&self, source: HookSource) -> AgentTasks {
        let mut tasks = AgentTasks::default();
        for session in self
            .snapshot
            .sessions
            .iter()
            .filter(|row| row.source == source.as_str())
        {
            match session.phase.as_str() {
                "running" => tasks.running += 1,
                "waitingForApproval" | "waitingForAnswer" => tasks.pending += 1,
                "completed" => tasks.done += 1,
                _ => (),
            }
        }
        tasks
    }

    /// Existing log records can help validate an agent at the next hook. They
    /// keep their original timestamp and do not change the restored display.
    pub fn observe(&mut self, source: HookSource, at: u64) {
        let agent = self.agent_mut(source);
        if agent.last_seen.is_none_or(|previous| at > previous) {
            agent.last_seen = Some(at);
            self.dirty = true;
        }
    }

    /// Called only by a hook or a newly observed Codex event. Expiry is checked
    /// here, never at startup, on a timer, or because a quota fetch succeeded.
    pub fn activate(&mut self, source: HookSource, at: u64, now: u64) {
        self.observe(source, at);
        self.agent_mut(source).ended = false;
        self.live = true;
        for source in AGENTS {
            let agent = self.agent_mut(source);
            agent.visible = !agent.ended
                && agent
                    .last_seen
                    .is_some_and(|seen| now.saturating_sub(seen) < STALE_AFTER_SECS);
        }
        self.snapshot.updated_at = now;
        self.dirty = true;
    }

    pub fn end_agent(&mut self, source: HookSource) {
        *self.agent_mut(source) = AgentState {
            ended: true,
            ..Default::default()
        };
        self.dirty = true;
    }

    pub fn remember(&mut self, usage: UsageSnapshot, sessions: Vec<ui::SessionRow>, now: u64) {
        if !self.live {
            return;
        }
        self.snapshot.usage = usage;
        self.snapshot.sessions = sessions.into_iter().map(SavedSession::from).collect();
        self.snapshot.updated_at = now;
        self.dirty = true;
    }

    /// Coalesce rapid hooks; a clean shutdown always flushes the final state.
    pub fn save(&mut self, force: bool) {
        if !self.dirty
            || (!force
                && self
                    .last_save
                    .is_some_and(|at| at.elapsed() < SAVE_INTERVAL))
        {
            return;
        }
        self.last_save = Some(Instant::now());
        let result = config::config_dir()
            .and_then(|dir| write_snapshot(&dir.join("display.json"), &self.snapshot));
        if result.is_ok() {
            self.dirty = false;
        }
    }
}

fn read_snapshot(path: &Path) -> Option<Snapshot> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(snapshot).map_err(io::Error::other)?;
    std::fs::write(&temporary, body)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::usage::{ClaudeLimits, UsageLimit};

    const THEN: u64 = 1_787_000_000;
    const CLAUDE: HookSource = HookSource::Claude;
    const CODEX: HookSource = HookSource::Codex;

    fn previous_display() -> Snapshot {
        Snapshot {
            claude: AgentState {
                visible: true,
                last_seen: Some(THEN),
                ..Default::default()
            },
            usage: UsageSnapshot {
                claude: ClaudeLimits {
                    limits: vec![UsageLimit {
                        kind: "weekly_all".into(),
                        label: "Week".into(),
                        percent: 77.0,
                        resets_at: Some(THEN + 2 * 86_400),
                    }],
                    fetched_at: Some(THEN),
                },
                ..Default::default()
            },
            sessions: vec![SavedSession {
                id: "previous-session".into(),
                title: "project".into(),
                detail: "Working".into(),
                phase: "running".into(),
                source: "claude".into(),
            }],
            updated_at: THEN,
            ..Default::default()
        }
    }

    #[test]
    fn startup_preserves_the_last_display_until_new_activity() {
        let snapshot = previous_display();
        let mut display = DisplayState::restore(snapshot.clone());
        let now = THEN + 7 * 86_400;
        // Even a routine repaint or a baseline scan cannot replace the saved
        // reading, expire its reset label, or age its task counts on startup.
        display.remember(UsageSnapshot::default(), vec![], now);
        display.observe(CODEX, now - 10);
        assert!(!display.is_live());
        assert_eq!(display.visible_agents(), vec![CLAUDE]);
        assert_eq!(display.usage(), snapshot.usage);
        assert_eq!(display.reading_time(now), THEN);
        assert_eq!(display.saved_tasks(CLAUDE).running, 1);
        let row = &display.saved_sessions(1)[0];
        assert_eq!(
            (row.title.as_str(), row.detail.as_str()),
            ("project", "Working")
        );
        assert!(!row.jumpable, "old terminal targets are not restored");
    }

    #[test]
    fn activity_expires_other_agents_at_the_boundary_and_can_restore_them() {
        let mut display = DisplayState::restore(previous_display());
        let before_expiry = THEN + STALE_AFTER_SECS - 1;
        display.activate(CODEX, before_expiry, before_expiry);
        assert_eq!(display.visible_agents(), vec![CLAUDE, CODEX]);
        // Advancing the clock or reading the display does not hide anything.
        let expiry = THEN + STALE_AFTER_SECS;
        assert_eq!(display.reading_time(expiry), expiry);
        assert!(display.visible(CLAUDE));
        display.activate(CODEX, expiry, expiry);
        assert_eq!(display.visible_agents(), vec![CODEX]);
        // The old quota is kept for later reuse, but does not make Claude live.
        assert!(!display.usage().claude.is_empty());
        display.remember(previous_display().usage, vec![], expiry + 1);
        assert!(!display.visible(CLAUDE));
        display.activate(CLAUDE, expiry + 2, expiry + 2);
        assert_eq!(display.visible_agents(), vec![CLAUDE, CODEX]);
        display.end_agent(CLAUDE);
        assert_eq!(display.visible_agents(), vec![CODEX]);
    }

    #[test]
    fn rescanning_an_old_event_does_not_extend_its_lifetime() {
        let mut display = DisplayState::restore(previous_display());
        display.observe(CLAUDE, THEN - 1);
        display.observe(CLAUDE, THEN);
        let now = THEN + STALE_AFTER_SECS;
        display.activate(CODEX, now, now);
        assert!(!display.visible(CLAUDE));
        assert_eq!(display.agent(CLAUDE).last_seen, Some(THEN));
    }

    #[test]
    fn a_new_hook_keeps_another_recent_agent_visible_after_restart() {
        let mut display = DisplayState::restore(previous_display());
        display.activate(CODEX, THEN + 10, THEN + 10);
        assert!(display.is_live());
        assert_eq!(display.visible_agents(), vec![CLAUDE, CODEX]);
    }

    #[test]
    fn an_ended_agent_stays_hidden_when_old_logs_are_read_again() {
        let mut display = DisplayState::restore(previous_display());
        display.activate(CODEX, THEN, THEN);
        display.end_agent(CODEX);
        display.observe(CODEX, THEN);
        display.activate(CLAUDE, THEN + 1, THEN + 1);
        assert!(!display.visible(CODEX));
        display.activate(CODEX, THEN + 2, THEN + 2);
        assert!(display.visible(CODEX));
    }

    #[test]
    fn persistence_restores_hidden_agents_readings_and_session_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/display.json");
        assert!(read_snapshot(&path).is_none());
        let mut snapshot = previous_display();
        write_snapshot(&path, &snapshot).unwrap();
        assert_eq!(read_snapshot(&path), Some(snapshot.clone()));
        // Exercise replacement too: the previous complete snapshot must survive
        // until the new file is ready.
        snapshot.claude.visible = false;
        snapshot.sessions.clear();
        write_snapshot(&path, &snapshot).unwrap();
        let restored = DisplayState::restore(read_snapshot(&path).unwrap());
        assert!(!restored.is_live());
        assert!(!restored.visible(CLAUDE));
        assert_eq!(restored.usage(), snapshot.usage);
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::write(&path, b"{broken").unwrap();
        assert!(read_snapshot(&path).is_none());
        let empty = DisplayState::restore(Snapshot::default());
        assert!(empty.visible_agents().is_empty());
    }
}
