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

pub struct CodexWatcher {
    home: Option<PathBuf>,
    cache: Arc<Mutex<SessionCache>>,
    tx: mpsc::Sender<io::Result<Vec<SessionState>>>,
    rx: mpsc::Receiver<io::Result<Vec<SessionState>>>,
    scanning: Cell<bool>,
    last_scan: Cell<Option<Instant>>,
    counts: Cell<Option<AgentTasks>>,
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
        }
    }

    pub fn poll(&self, table: &mut SessionTable) -> bool {
        let now = atoll_core::now_unix_secs();
        let mut changed = false;
        while let Ok(result) = self.rx.try_recv() {
            self.scanning.set(false);
            if let Ok(sessions) = result {
                changed |= table.sync_observed(HookSource::Codex, sessions, now);
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
            return changed;
        }
        let Some(home) = self.home.clone() else {
            return changed;
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
        changed
    }
}
