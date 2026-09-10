use super::*;
use crate::codex::SessionCache;

const ID: &str = "01a08965-575f-7870-b001-3698447df18c";
const NOW: u64 = 1_789_025_000;

#[test]
fn history_merge_keeps_the_cli_rollout_needed_to_find_its_terminal() {
    let fixture = Fixture::new();
    fixture
        .state
        .execute("UPDATE threads SET source = 'cli'", [])
        .unwrap();
    for last_seen in [NOW - 11, NOW] {
        let mut rollout = SessionState::new(ID, HookSource::Codex, NOW - 20);
        rollout.last_seen = last_seen;
        rollout.codex_client = CodexClient::Cli;
        rollout.transcript_path = Some("C:/synthetic/rollout-cli.jsonl".into());
        let mut sessions = vec![rollout];
        Cache::default().merge(fixture.dir.path(), NOW, &mut sessions);
        assert_eq!(sessions[0].codex_client, CodexClient::Cli);
        assert_eq!(
            sessions[0].transcript_path.as_deref(),
            Some("C:/synthetic/rollout-cli.jsonl")
        );
    }
}

#[test]
fn history_without_client_details_preserves_explicit_desktop_metadata() {
    let fixture = Fixture::new();
    let mut rollout = SessionState::new(ID, HookSource::Codex, NOW - 20);
    rollout.codex_client = CodexClient::Desktop;
    let mut sessions = vec![rollout];
    Cache::default().merge(fixture.dir.path(), NOW, &mut sessions);
    assert_eq!(sessions[0].codex_client, CodexClient::Desktop);
}

struct Fixture {
    dir: tempfile::TempDir,
    state: Connection,
    history: Connection,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = Connection::open(dir.path().join("state_5.sqlite")).unwrap();
        state
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             CREATE TABLE threads (id TEXT, cwd TEXT, name TEXT, title TEXT, archived INTEGER,
                                   source TEXT, history_mode TEXT, updated_at INTEGER);",
            )
            .unwrap();
        state.execute("INSERT INTO threads VALUES (?1, 'C:/project', 'Desktop work', 'old title', 0, 'vscode', 'paginated', ?2)", (ID, NOW as i64)).unwrap();
        let history = Connection::open(dir.path().join("thread_history_1.sqlite")).unwrap();
        history
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, rollout_ordinal INTEGER,
                 status TEXT, started_at INTEGER, completed_at INTEGER);
             CREATE TABLE thread_items (thread_id TEXT, turn_id TEXT, created_at_ms INTEGER);",
            )
            .unwrap();
        history
            .execute(
                "INSERT INTO thread_turns VALUES (?1, 'turn-1', 1, 'inProgress', ?2, NULL)",
                (ID, (NOW - 10) as i64),
            )
            .unwrap();
        Self {
            dir,
            state,
            history,
        }
    }

    fn scan(&self, now: u64) -> Vec<SessionState> {
        SessionCache::default().scan(self.dir.path(), now).unwrap()
    }
}

#[test]
fn desktop_history_works_without_rollouts_and_reads_wal_updates() {
    let fixture = Fixture::new();
    let running = fixture.scan(NOW);
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].phase, Phase::Running);
    assert_eq!(running[0].display_name.as_deref(), Some("Desktop work"));
    fixture
        .history
        .execute(
            "UPDATE thread_turns SET status = 'completed', completed_at = ?1",
            [NOW as i64],
        )
        .unwrap();
    let done = fixture.scan(NOW + 1);
    assert_eq!(done[0].phase, Phase::Completed);
    assert_eq!(done[0].last_seen, NOW);
    fixture
        .history
        .execute(
            "INSERT INTO thread_turns VALUES (?1, 'turn-2', 2, 'inProgress', ?2, NULL)",
            (ID, (NOW + 2) as i64),
        )
        .unwrap();
    assert_eq!(fixture.scan(NOW + 3)[0].phase, Phase::Running);
    assert_eq!(fixture.scan(NOW + 3)[0].first_seen, NOW + 2);
}

#[test]
fn cancelled_and_failed_turns_are_distinct_and_archiving_removes_stale_rollouts() {
    let fixture = Fixture::new();
    for (status, event) in [("interrupted", "turn_aborted"), ("failed", "turn_failed")] {
        fixture
            .history
            .execute(
                "UPDATE thread_turns SET status = ?1, completed_at = ?2",
                (status, NOW as i64),
            )
            .unwrap();
        assert_eq!(fixture.scan(NOW + 1)[0].last_event, event);
    }
    let mut log_sessions = fixture.scan(NOW + 1);
    fixture
        .state
        .execute("UPDATE threads SET archived = 1", [])
        .unwrap();
    Cache::default().merge(fixture.dir.path(), NOW + 1, &mut log_sessions);
    assert!(log_sessions.is_empty());
    fixture
        .state
        .execute("UPDATE threads SET archived = 0, name = 'Renamed'", [])
        .unwrap();
    assert_eq!(
        fixture.scan(NOW + 1)[0].display_name.as_deref(),
        Some("Renamed")
    );
}

#[test]
fn a_live_writer_keeps_a_quiet_turn_and_a_closed_writer_cannot_keep_it_alive() {
    let fixture = Fixture::new();
    let locks = fixture.dir.path().join("thread-writer-locks");
    std::fs::create_dir(&locks).unwrap();
    let writer = File::create(locks.join(format!("{ID}.lock"))).unwrap();
    writer.lock().unwrap();
    let quiet = fixture.scan(NOW + STALE_AFTER_SECS);
    assert_eq!(quiet.len(), 1);
    assert!(quiet[0].observed_alive);
    assert_eq!(quiet[0].last_seen, NOW - 10);
    drop(writer);
    let disconnected = fixture.scan(NOW);
    assert_eq!(disconnected[0].last_event, "session_disconnected");
    assert!(!disconnected[0].observed_alive);
    assert!(fixture.scan(NOW + STALE_AFTER_SECS).is_empty());
}

#[test]
fn missing_or_incompatible_storage_is_read_only_and_preserves_the_log_fallback() {
    let absent = tempfile::tempdir().unwrap();
    assert!(read(absent.path(), NOW).is_err());
    assert!(std::fs::read_dir(absent.path()).unwrap().next().is_none());
    let fixture = Fixture::new();
    assert!(
        read_only(&fixture.dir.path().join("state_5.sqlite"))
            .unwrap()
            .execute("DELETE FROM threads", [])
            .is_err()
    );
    let mut from_logs = fixture.scan(NOW);
    let original = from_logs.clone();
    fixture
        .history
        .execute("DROP TABLE thread_turns", [])
        .unwrap();
    Cache::default().merge(fixture.dir.path(), NOW, &mut from_logs);
    assert_eq!(from_logs, original);
}

#[test]
fn subagents_unknown_status_and_path_ids_do_not_become_desktop_sessions() {
    let fixture = Fixture::new();
    fixture
        .state
        .execute("UPDATE threads SET source = '{\"subagent\":{}}'", [])
        .unwrap();
    assert!(fixture.scan(NOW).is_empty());
    fixture
        .state
        .execute(
            "UPDATE threads SET source = 'vscode', id = '../outside'",
            [],
        )
        .unwrap();
    assert!(fixture.scan(NOW).is_empty());
    fixture
        .state
        .execute("UPDATE threads SET id = ?1", [ID])
        .unwrap();
    fixture
        .history
        .execute("UPDATE thread_turns SET status = 'futureStatus'", [])
        .unwrap();
    assert!(fixture.scan(NOW).is_empty());
}

#[test]
fn a_temporary_database_failure_retains_state_for_a_bounded_interval() {
    let fixture = Fixture::new();
    let mut cache = SessionCache::default();
    let baseline = cache.scan(fixture.dir.path(), NOW).unwrap();
    fixture
        .history
        .execute("DROP TABLE thread_turns", [])
        .unwrap();
    assert_eq!(cache.scan(fixture.dir.path(), NOW + 2).unwrap(), baseline);
    assert!(cache.scan(fixture.dir.path(), NOW + 31).unwrap().is_empty());
}

#[test]
#[ignore = "reads the installed Codex history without changing any thread"]
fn native_paginated_history_is_readable() {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap()).join(".codex")
        });
    let snapshot = read(&home, crate::now_unix_secs()).expect("compatible local desktop history");
    eprintln!(
        "Codex history: {} current sessions, {} archived, {} live writers",
        snapshot.sessions.len(),
        snapshot.archived.len(),
        snapshot
            .sessions
            .iter()
            .filter(|state| state.observed_alive)
            .count()
    );
}
