//! Read the desktop's paginated history without loading or resuming its threads.
//! This is a versioned local storage adapter, not a desktop control protocol.
//! Missing or incompatible databases leave rollout observation in charge.

use std::collections::HashSet;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::protocol::HookSource;
use crate::state::{Phase, STALE_AFTER_SECS, SessionState};

#[derive(Clone)]
struct Snapshot {
    sessions: Vec<SessionState>,
    archived: HashSet<String>,
}

#[derive(Default)]
pub(super) struct Cache {
    last: Option<(u64, Snapshot)>,
}

fn read_only(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(25))?;
    db.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")?;
    Ok(db)
}

/// Merge only a fully read snapshot. A busy or migrated database must not
/// clear the last log observation or leave a partially applied archive list.
impl Cache {
    pub(super) fn merge(&mut self, home: &Path, now: u64, sessions: &mut Vec<SessionState>) {
        if let Ok(snapshot) = read(home, now) {
            self.last = Some((now, snapshot));
        }
        let Some((at, snapshot)) = &self.last else {
            return;
        };
        if now.saturating_sub(*at) > 30 {
            return;
        }
        let mut snapshot = snapshot.clone();
        // A retained database result cannot retain proof that a process lives.
        for state in &mut snapshot.sessions {
            if state.observed_alive && writer_alive(home, &state.session_id) != Some(true) {
                state.observed_alive = false;
                state.phase = Phase::Completed;
                state.last_event = "session_disconnected".into();
            }
        }
        merge_snapshot(snapshot, sessions);
    }
}

fn merge_snapshot(snapshot: Snapshot, sessions: &mut Vec<SessionState>) {
    sessions.retain(|session| !snapshot.archived.contains(&session.session_id));
    for mut incoming in snapshot.sessions {
        if let Some(index) = sessions
            .iter()
            .position(|old| old.session_id == incoming.session_id)
        {
            let old = &sessions[index];
            // The rollout and history projections can be a write apart.
            if old.last_seen > incoming.last_seen && incoming.last_event != "session_disconnected" {
                sessions[index].observed_alive = incoming.observed_alive;
                sessions[index].display_name = incoming.display_name;
                continue;
            }
            if incoming.phase == Phase::Running
                && old.phase.is_waiting()
                && old.last_seen >= incoming.first_seen
            {
                incoming.phase = old.phase;
            }
            sessions[index] = incoming;
        } else {
            sessions.push(incoming);
        }
    }
}

fn read(home: &Path, now: u64) -> rusqlite::Result<Snapshot> {
    let state = read_only(&home.join("state_5.sqlite"))?;
    let history = read_only(&home.join("thread_history_1.sqlite"))?;
    let mut threads = state.prepare(
        "SELECT id, cwd, COALESCE(NULLIF(name, ''), title), archived
         FROM threads
         WHERE history_mode = 'paginated' AND source IN ('cli', 'vscode', 'appServer')
         ORDER BY updated_at DESC LIMIT 256",
    )?;
    let mut turns = history.prepare(
        "SELECT turn_id, status, started_at, completed_at
         FROM thread_turns WHERE thread_id = ?1 ORDER BY rollout_ordinal DESC LIMIT 1",
    )?;
    let mut items = history.prepare(
        "SELECT MAX(created_at_ms) / 1000 FROM thread_items WHERE thread_id = ?1 AND turn_id = ?2",
    )?;
    let mut snapshot = Snapshot {
        sessions: Vec::new(),
        archived: HashSet::new(),
    };
    let mut rows = threads.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        // The ID also names a writer lock. Never turn stored data into a path.
        if !valid_thread_id(&id) {
            continue;
        }
        if row.get::<_, bool>(3)? {
            snapshot.archived.insert(id);
            continue;
        }
        let turn = turns
            .query_row([&id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?
                        .and_then(|at| u64::try_from(at).ok()),
                    row.get::<_, Option<i64>>(3)?
                        .and_then(|at| u64::try_from(at).ok()),
                ))
            })
            .optional()?;
        let Some((turn, status, Some(started), completed)) = turn else {
            continue;
        };
        let (phase, event) = match status.as_str() {
            "inProgress" => (Phase::Running, "desktop_running"),
            "completed" => (Phase::Completed, "task_complete"),
            "interrupted" => (Phase::Completed, "turn_aborted"),
            "failed" => (Phase::Completed, "turn_failed"),
            _ => continue,
        };
        let at = match completed {
            Some(at) => at,
            None => items
                .query_row([&id, &turn], |row| row.get::<_, Option<i64>>(0))?
                .and_then(|at| u64::try_from(at).ok())
                .unwrap_or(started)
                .max(started),
        };
        if at > now || started > now {
            continue;
        }
        let mut session = SessionState::new(&id, HookSource::Codex, started);
        session.last_seen = at;
        session.phase = phase;
        session.last_event = event.into();
        session.cwd = row.get(1)?;
        session.display_name = row.get(2)?;
        if phase == Phase::Running {
            match writer_alive(home, &id) {
                Some(true) => session.observed_alive = true,
                Some(false) => {
                    session.phase = Phase::Completed;
                    session.last_event = "session_disconnected".into();
                }
                None => {} // Older clients have no writer lock; use event age.
            }
        }
        if !session.is_stale(now, STALE_AFTER_SECS) {
            snapshot.sessions.push(session);
        }
    }
    Ok(snapshot)
}

fn valid_thread_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn writer_alive(home: &Path, id: &str) -> Option<bool> {
    let file = File::open(home.join("thread-writer-locks").join(format!("{id}.lock"))).ok()?;
    match file.try_lock_shared() {
        Ok(()) => Some(false), // Dropping the handle immediately releases our probe.
        Err(TryLockError::WouldBlock) => Some(true),
        Err(TryLockError::Error(_)) => None,
    }
}

#[cfg(test)]
mod tests;
