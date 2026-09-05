use super::*;
use serde_json::json;
use std::io::Write;

fn now(second: u64) -> u64 {
    parse_iso8601("2026-09-05T00:00:00Z").unwrap() + second
}

fn event(second: u64, kind: &str, turn: &str) -> Value {
    json!({"timestamp": format!("2026-09-05T00:00:{second:02}Z"), "type": "event_msg",
        "payload": {"type": kind, "turn_id": turn}})
}

fn append(path: &Path, rows: &[Value]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

fn fixture(source: Value) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions/2026/09/05");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("rollout-test.jsonl");
    let meta = json!({"timestamp": "2026-09-05T00:00:00Z", "type": "session_meta", "payload": {
        "id": "s-1", "cwd": "C:/synthetic/atoll", "source": source}});
    fs::write(&path, format!("{meta}\n")).unwrap();
    (dir, path)
}

#[test]
fn recovers_an_already_running_session_and_follows_completion_and_the_next_turn() {
    let (dir, path) = fixture(json!("cli"));
    append(&path, &[event(1, "task_started", "t1")]);
    let mut cache = SessionCache::default();
    let running = cache.scan(dir.path(), now(2)).unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].phase, Phase::Running);
    assert_eq!(running[0].cwd.as_deref(), Some("C:/synthetic/atoll"));
    assert_eq!(
        SessionCache::default().scan(dir.path(), now(2)).unwrap(),
        running
    );

    append(
        &path,
        &[
            event(3, "task_complete", "t1"),
            event(4, "token_count", "t1"),
        ],
    );
    let done = cache.scan(dir.path(), now(5)).unwrap();
    assert_eq!(done[0].phase, Phase::Completed);
    assert_eq!(done[0].last_seen, now(3));
    append(
        &path,
        &[
            event(6, "task_started", "t2"),
            event(7, "task_complete", "t1"),
        ],
    );
    assert_eq!(
        cache.scan(dir.path(), now(8)).unwrap()[0].phase,
        Phase::Running
    );
    append(&path, &[event(9, "turn_aborted", "t2")]);
    assert_eq!(
        cache.scan(dir.path(), now(10)).unwrap()[0].phase,
        Phase::Completed
    );
}

#[test]
fn notices_appends_even_when_windows_keeps_the_original_mtime() {
    let (dir, path) = fixture(json!("cli"));
    append(&path, &[event(1, "task_started", "t1")]);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut cache = SessionCache::default();
    cache.scan(dir.path(), now(2)).unwrap();
    append(&path, &[event(3, "task_complete", "t1")]);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        cache.scan(dir.path(), now(4)).unwrap()[0].phase,
        Phase::Completed
    );
}

#[test]
fn retries_a_half_written_record_and_ignores_malformed_lines() {
    let (dir, path) = fixture(json!("cli"));
    append(&path, &[event(1, "task_started", "t1")]);
    let mut cache = SessionCache::default();
    cache.scan(dir.path(), now(2)).unwrap();
    let record = event(3, "task_complete", "t1").to_string();
    let half = record.len() / 2;
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(&record.as_bytes()[..half]).unwrap();
    assert_eq!(
        cache.scan(dir.path(), now(4)).unwrap()[0].phase,
        Phase::Running
    );
    writer.write_all(&record.as_bytes()[half..]).unwrap();
    writer.write_all(b"\nnot json\n").unwrap();
    assert_eq!(
        cache.scan(dir.path(), now(5)).unwrap()[0].phase,
        Phase::Completed
    );
}

#[test]
fn old_files_and_repeated_scans_do_not_manufacture_active_sessions() {
    let (dir, path) = fixture(json!("cli"));
    let mut cache = SessionCache::default();
    assert!(cache.scan(dir.path(), now(1)).unwrap().is_empty());
    append(&path, &[event(1, "task_started", "t1")]);
    assert_eq!(cache.scan(dir.path(), now(2)).unwrap().len(), 1);
    assert!(
        cache
            .scan(dir.path(), now(1) + STALE_AFTER_SECS)
            .unwrap()
            .is_empty()
    );
    assert!(
        SessionCache::default()
            .scan(dir.path(), now(1) + STALE_AFTER_SECS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn skips_subagents_and_removes_deleted_or_truncated_rollouts() {
    let (subdir, subpath) =
        fixture(json!({"subagent": {"thread_spawn": {"parent_thread_id": "parent"}}}));
    append(&subpath, &[event(1, "task_started", "t1")]);
    assert!(
        SessionCache::default()
            .scan(subdir.path(), now(2))
            .unwrap()
            .is_empty()
    );

    let (dir, path) = fixture(json!("vscode"));
    append(&path, &[event(1, "task_started", "t1")]);
    let mut cache = SessionCache::default();
    assert_eq!(cache.scan(dir.path(), now(2)).unwrap().len(), 1);
    fs::write(&path, b"").unwrap();
    assert!(cache.scan(dir.path(), now(3)).unwrap().is_empty());
    fs::remove_file(&path).unwrap();
    assert!(cache.scan(dir.path(), now(4)).unwrap().is_empty());
    assert!(cache.files.is_empty());
}

#[test]
fn a_large_rollout_starts_with_its_recent_events() {
    let (dir, path) = fixture(json!("cli"));
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer
        .write_all(&vec![b'x'; READ_BUDGET as usize + 100])
        .unwrap();
    writer.write_all(b"\n").unwrap();
    append(&path, &[event(1, "item_completed", "t1")]);
    let sessions = SessionCache::default().scan(dir.path(), now(2)).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].phase, Phase::Running);
}

#[test]
fn observed_sessions_update_counts_but_never_overwrite_live_hook_approvals() {
    use crate::protocol::HookPayload;
    use crate::state::SessionTable;
    let (dir, path) = fixture(json!("cli"));
    append(&path, &[event(1, "task_started", "t1")]);
    let observations = SessionCache::default().scan(dir.path(), now(2)).unwrap();
    let mut table = SessionTable::new();
    assert!(table.sync_observed(HookSource::Codex, observations.clone(), now(2)));
    assert_eq!(table.tasks(HookSource::Codex, now(2)).running, 1);
    assert!(!table.sync_observed(HookSource::Codex, observations.clone(), now(3)));
    let approval: HookPayload = serde_json::from_value(json!({"session_id": "s-1",
        "hook_event_name": "PermissionRequest", "tool_name": "Bash"}))
    .unwrap();
    table.apply(&approval, HookSource::Codex, now(4));
    assert!(!table.sync_observed(HookSource::Codex, observations, now(5)));
    assert_eq!(table.get("s-1").unwrap().phase, Phase::WaitingForApproval);
    assert!(!table.sync_observed(HookSource::Codex, vec![], now(6)));
    assert_eq!(table.get("s-1").unwrap().pending.len(), 1);

    let mut observed_only = SessionTable::new();
    let observations = SessionCache::default().scan(dir.path(), now(2)).unwrap();
    observed_only.sync_observed(HookSource::Codex, observations, now(2));
    assert!(observed_only.sync_observed(HookSource::Codex, vec![], now(3)));
    assert!(observed_only.is_empty());
}

#[test]
#[ignore = "reads this machine's real Codex session logs"]
fn inspect_local_sessions() {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").unwrap()).join(".codex"));
    let sessions = SessionCache::default()
        .scan(&home, crate::now_unix_secs())
        .unwrap();
    for session in &sessions {
        eprintln!(
            "{} {} {}",
            session.session_id,
            session.phase.as_str(),
            session.last_event
        );
    }
    eprintln!("{} detected Codex sessions", sessions.len());
}
