//! Background observation of Codex sessions, including sessions already running
//! when Atoll starts. The UI thread only merges the finished snapshots.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use atoll_core::codex::SessionCache;
use atoll_core::protocol::HookSource;
use atoll_core::state::{AgentTasks, SessionState, SessionTable};

const POLL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Update {
    pub changed: bool,
    pub last_seen: Option<u64>,
    pub new_activity: bool,
}

pub struct CodexWatcher {
    home: Option<PathBuf>,
    cache: Arc<Mutex<SessionCache>>,
    tx: mpsc::Sender<io::Result<Vec<SessionState>>>,
    rx: mpsc::Receiver<io::Result<Vec<SessionState>>>,
    scanning: Cell<bool>,
    last_scan: Cell<Option<Instant>>,
    counts: Cell<Option<AgentTasks>>,
    /// Reading pre-existing logs establishes a baseline; only later events
    /// release the display restored at startup.
    latest_event: Cell<u64>,
}

impl CodexWatcher {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            home: std::env::var_os("CODEX_HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .or_else(|| crate::util::home_dir().map(|home| home.join(".codex"))),
            cache: Arc::new(Mutex::new(SessionCache::default())),
            tx,
            rx,
            scanning: Cell::new(false),
            last_scan: Cell::new(None),
            counts: Cell::new(None),
            latest_event: Cell::new(atoll_core::now_unix_secs()),
        }
    }

    pub fn poll(&self, table: &mut SessionTable) -> Update {
        let now = atoll_core::now_unix_secs();
        let mut update = Update::default();
        while let Ok(result) = self.rx.try_recv() {
            self.scanning.set(false);
            if let Ok(sessions) = result {
                if let Some(at) = sessions.iter().map(|session| session.last_seen).max() {
                    update.last_seen = Some(update.last_seen.unwrap_or(0).max(at));
                    if at > self.latest_event.get() && at <= now {
                        self.latest_event.set(at);
                        update.new_activity = true;
                    }
                }
                update.changed |= table.sync_observed(HookSource::Codex, sessions, now);
                let counts = table.tasks(HookSource::Codex, now);
                if self.counts.replace(Some(counts)) != Some(counts) {
                    crate::util::debug_log(&format!(
                        "codex sessions: {} running, {} waiting, {} done",
                        counts.running, counts.pending, counts.done,
                    ));
                }
            }
        }
        if self.scanning.get() || self.last_scan.get().is_some_and(|at| at.elapsed() < POLL) {
            return update;
        }
        let Some(home) = self.home.clone() else {
            return update;
        };
        self.scanning.set(true);
        self.last_scan.set(Some(Instant::now()));
        let cache = Arc::clone(&self.cache);
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name("atoll-codex-sessions".into())
            .spawn(move || {
                let result = cache
                    .lock()
                    .unwrap_or_else(|held| held.into_inner())
                    .scan(&home, atoll_core::now_unix_secs());
                if tx.send(result).is_ok() {
                    let _ = slint::invoke_from_event_loop(super::pump);
                }
            });
        if spawned.is_err() {
            self.scanning.set(false);
        }
        update
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_new_events_release_the_saved_startup_display() {
        let now = atoll_core::now_unix_secs();
        let mut watcher = CodexWatcher::new();
        watcher.home = None; // Feed snapshots directly, without reading user logs.
        watcher.latest_event.set(now - 60);
        let mut table = SessionTable::new();
        let mut session = SessionState::new("existing", HookSource::Codex, now - 65);
        watcher.tx.send(Ok(vec![session.clone()])).unwrap();
        let baseline = watcher.poll(&mut table);
        assert!(baseline.changed);
        assert!(!baseline.new_activity);
        assert_eq!(baseline.last_seen, Some(now - 65));

        watcher.tx.send(Ok(vec![session.clone()])).unwrap();
        let repeated = watcher.poll(&mut table);
        assert!(!repeated.changed && !repeated.new_activity);

        session.last_seen = now - 1;
        watcher.tx.send(Ok(vec![session.clone()])).unwrap();
        let fresh = watcher.poll(&mut table);
        assert!(fresh.changed && fresh.new_activity);
        assert_eq!(fresh.last_seen, Some(now - 1));

        watcher.tx.send(Ok(vec![session])).unwrap();
        assert!(!watcher.poll(&mut table).new_activity);
        // Expiry/removal is not new activity either.
        watcher.tx.send(Ok(vec![])).unwrap();
        let removed = watcher.poll(&mut table);
        assert!(removed.changed && !removed.new_activity);
    }
}
