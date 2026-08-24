//! Reading Claude Code's session transcripts off disk.
//!
//! Hooks tell Atoll what is happening *now*. The transcripts under
//! `<home>/.claude/projects/**/*.jsonl` tell it what happened before Atoll
//! started, and cover sessions whose hooks never fired at all. Together they are
//! what makes the session list survive an Atoll restart.
//!
//! # Cost control
//!
//! A busy machine accumulates thousands of transcripts, and a single one can run
//! to tens of megabytes. Three things keep a scan cheap:
//!
//! 1. **File-level filtering first.** `mtime` is read from the directory entry;
//!    files older than [`ScanOptions::max_age`] are never opened, and only the
//!    newest [`ScanOptions::max_files`] are read.
//! 2. **Streaming, never slurping.** Each file is read in [`CHUNK_SIZE`] blocks
//!    with a carry buffer for the line straddling the boundary, so peak memory
//!    is one chunk plus one line regardless of file size.
//! 3. **Cheap per line.** Only lines that could matter are parsed as JSON.
//!
//! # Path exclusion
//!
//! Paths containing `subagents` are skipped: those transcripts belong to
//! subagent turns inside a parent session, and surfacing them would show one
//! logical session several times over.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Read granularity. Large enough that a big transcript is a handful of reads,
/// small enough that memory stays flat.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Any path component containing this is a subagent transcript, not a session.
const SUBAGENT_MARKER: &str = "subagents";

/// Default window: a session nobody has touched in a day is not "current".
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Default cap on how many transcripts one scan opens.
pub const DEFAULT_MAX_FILES: usize = 40;

/// How much of an assistant message to keep as a title.
const TITLE_LIMIT: usize = 160;

/// What one transcript file says about its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSummary {
    /// The transcript's `sessionId` — camelCase here, unlike the hook payload's
    /// snake_case `session_id`. The two name the same thing and are the join key
    /// between [`crate::state::SessionTable`] and this module.
    pub session_id: String,
    /// The session's working directory. A transcript without one is discarded:
    /// with no `cwd` there is nothing to label the session with and no directory
    /// to jump back to.
    pub cwd: String,
    pub path: PathBuf,
    /// The last `timestamp` in the file, as the transcript spells it (ISO 8601).
    pub last_timestamp: Option<String>,
    /// The model from the most recent assistant message.
    pub model: Option<String>,
    /// A one-line label: the last assistant text, or the transcript's own
    /// `summary` when there is no assistant text to use.
    pub title: Option<String>,
    /// The file's modification time, which is what the scan sorts on.
    pub modified: SystemTime,
}

/// Where and how far to scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// The home directory to scan under. Injected rather than resolved so tests
    /// can point at a temporary directory.
    pub home: PathBuf,
    /// Skip files not modified within this window.
    pub max_age: Duration,
    /// Open at most this many files, newest first.
    pub max_files: usize,
}

impl ScanOptions {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            max_age: DEFAULT_MAX_AGE,
            max_files: DEFAULT_MAX_FILES,
        }
    }

    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }
}

/// `<home>/.claude/projects`.
pub fn claude_projects_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("projects")
}

/// Transcripts already read, keyed by path, so a repeated scan only opens the
/// files that have changed since the last one.
///
/// A transcript is a whole-file read of something that grows all day. Forty of
/// them is most of a second on a warm cache and considerably more on a cold one,
/// and the scan runs every time somebody opens the detail panel — so almost all
/// of that work is the same work, done again, for files nobody has written to.
pub type TranscriptCache = std::collections::HashMap<PathBuf, TranscriptSummary>;

/// Scan every recent transcript under `options.home`, newest first.
///
/// A missing projects directory is not an error — it just means Claude Code has
/// never run here — and neither is an individual file that cannot be read or
/// yields nothing usable. Only a scan that cannot proceed at all fails.
pub fn scan_claude(options: &ScanOptions) -> io::Result<Vec<TranscriptSummary>> {
    scan_claude_cached(options, &mut TranscriptCache::new())
}

/// The same scan, reusing what a previous one read.
///
/// A file whose modification time has not moved since it was last read is taken
/// from `cache` rather than opened again. Entries for files that have since gone
/// away are dropped, so the cache tracks the directory rather than growing
/// forever.
pub fn scan_claude_cached(
    options: &ScanOptions,
    cache: &mut TranscriptCache,
) -> io::Result<Vec<TranscriptSummary>> {
    let root = claude_projects_dir(&options.home);
    let mut candidates = Vec::new();
    collect_jsonl(&root, &mut candidates)?;

    // Newest first, so the `max_files` cap keeps the interesting end.
    candidates.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));

    let now = SystemTime::now();
    let mut summaries = Vec::new();
    let mut seen = std::collections::HashSet::with_capacity(candidates.len());
    for (path, modified) in candidates {
        seen.insert(path.clone());
        if summaries.len() >= options.max_files {
            continue;
        }
        // `duration_since` errors when `modified` is in the future (a clock
        // skew, or a file copied from another machine). Treat that as "brand
        // new" rather than "unreadably old".
        let age = now.duration_since(modified).unwrap_or_default();
        if age > options.max_age {
            // Sorted by mtime, so everything after this is older too — but the
            // loop runs on so that `seen` covers the whole directory and the
            // sweep below does not drop entries it simply stopped early of.
            continue;
        }
        if let Some(hit) = cache.get(&path).filter(|hit| hit.modified == modified) {
            summaries.push(hit.clone());
            continue;
        }
        match read_transcript(&path, modified) {
            Ok(Some(summary)) => {
                cache.insert(path, summary.clone());
                summaries.push(summary);
            }
            // It changed, and now it yields nothing — truncated, rewritten,
            // briefly unreadable. Whatever the cache remembers about it is about
            // a file that no longer exists in that form.
            _ => {
                cache.remove(&path);
            }
        }
    }
    cache.retain(|path, _| seen.contains(path));
    Ok(summaries)
}

/// Walk `dir` recursively, collecting `(path, mtime)` for every `.jsonl` file
/// outside a subagents directory.
fn collect_jsonl(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // No projects directory at all, or one we may not read. Either way there
        // is nothing to report and nothing the user can do about it here.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if is_subagent_path(&path) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_jsonl(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(modified) = entry.metadata().and_then(|meta| meta.modified())
        {
            out.push((path, modified));
        }
    }
    Ok(())
}

/// Whether any component of `path` marks it as a subagent transcript.
fn is_subagent_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(SUBAGENT_MARKER))
    })
}

/// Read one transcript, or `None` if it carries no usable session.
pub fn read_transcript(path: &Path, modified: SystemTime) -> io::Result<Option<TranscriptSummary>> {
    let mut accumulator = Accumulator::default();
    for_each_line(path, |line| accumulator.push_line(line))?;
    Ok(accumulator.finish(path, modified))
}

/// Stream `path` in [`CHUNK_SIZE`] blocks, handing `sink` one complete line at a
/// time. The line spanning a chunk boundary is carried into the next read.
///
/// Shared with [`crate::usage`], which reads Codex's rollout files the same way
/// and for the same reason.
pub(crate) fn for_each_line(path: &Path, mut sink: impl FnMut(&str)) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut chunk = vec![0u8; CHUNK_SIZE];
    let mut carry: Vec<u8> = Vec::new();

    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let mut start = 0;
        for index in 0..read {
            if chunk[index] != b'\n' {
                continue;
            }
            // The completed line is whatever we carried plus this chunk's share.
            if carry.is_empty() {
                emit(&chunk[start..index], &mut sink);
            } else {
                carry.extend_from_slice(&chunk[start..index]);
                emit(&carry, &mut sink);
                carry.clear();
            }
            start = index + 1;
        }
        carry.extend_from_slice(&chunk[start..read]);
    }

    // A final line with no trailing newline — common on a transcript still being
    // written — is still a line.
    if !carry.is_empty() {
        emit(&carry, &mut sink);
    }
    Ok(())
}

/// Hand `bytes` to `sink` as text, skipping anything that is not valid UTF-8 or
/// is blank. A `\r` from a CRLF-written file is trimmed here.
fn emit(bytes: &[u8], sink: &mut impl FnMut(&str)) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let text = text.trim_end_matches('\r').trim();
    if !text.is_empty() {
        sink(text);
    }
}

/// The running state of a single-pass scan over one transcript.
#[derive(Default)]
struct Accumulator {
    session_id: Option<String>,
    cwd: Option<String>,
    last_timestamp: Option<String>,
    model: Option<String>,
    last_assistant_text: Option<String>,
    summary: Option<String>,
}

impl Accumulator {
    fn push_line(&mut self, line: &str) {
        // A transcript is one JSON object per line; anything else is not ours.
        if !line.starts_with('{') {
            return;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };

        if let Some(value) = string_field(&record, "sessionId") {
            self.session_id = Some(value);
        }
        if let Some(value) = string_field(&record, "cwd") {
            self.cwd = Some(value);
        }
        if let Some(value) = string_field(&record, "timestamp") {
            self.last_timestamp = Some(value);
        }
        // `summary` records carry the conversation's own title.
        if let Some(value) = string_field(&record, "summary") {
            self.summary = Some(value);
        }

        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(message) = record.get("message") else {
            return;
        };
        if let Some(model) = string_field(message, "model") {
            self.model = Some(model);
        }
        if let Some(text) = assistant_text(message) {
            self.last_assistant_text = Some(text);
        }
    }

    fn finish(self, path: &Path, modified: SystemTime) -> Option<TranscriptSummary> {
        // Both are required: a transcript with no session id cannot be joined to
        // anything, and one with no cwd cannot be labeled or jumped to.
        let session_id = self.session_id?;
        let cwd = self.cwd?;

        // The last thing the assistant said beats the conversation summary,
        // which Claude Code writes once and rarely refreshes.
        let title = self
            .last_assistant_text
            .or(self.summary)
            .map(|text| truncate(&text, TITLE_LIMIT));

        Some(TranscriptSummary {
            session_id,
            cwd,
            path: path.to_path_buf(),
            last_timestamp: self.last_timestamp,
            model: self.model,
            title,
            modified,
        })
    }
}

/// A non-empty string field, trimmed.
fn string_field(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The text of an assistant message. `content` is either a bare string or the
/// block array Claude Code normally writes; in the array case the text blocks
/// are joined so a message split across blocks reads as one line.
fn assistant_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        let text = text.trim();
        return (!text.is_empty()).then(|| text.to_string());
    }

    let blocks = content.as_array()?;
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            let text = text.trim();
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Collapse to one line and cap the length, counting characters rather than
/// bytes so a multi-byte character is never split.
fn truncate(text: &str, limit: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flattened = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= limit {
        return flattened;
    }
    let kept: String = flattened.chars().take(limit).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `lines` as a `.jsonl` under `<home>/.claude/projects/<project>/`.
    fn write_transcript(home: &Path, project: &str, name: &str, lines: &[Value]) -> PathBuf {
        let dir = claude_projects_dir(home).join(project);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    /// A plausible transcript: a summary record, a user turn, an assistant turn.
    fn sample_lines(session: &str) -> Vec<Value> {
        vec![
            serde_json::json!({"type": "summary", "summary": "Wiring up the parser"}),
            serde_json::json!({
                "type": "user",
                "sessionId": session,
                "cwd": r"C:\synthetic\project",
                "timestamp": "2026-08-23T10:00:00.000Z",
                "message": {"role": "user", "content": "hello"},
            }),
            serde_json::json!({
                "type": "assistant",
                "sessionId": session,
                "cwd": r"C:\synthetic\project",
                "timestamp": "2026-08-23T10:00:05.000Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-5",
                    "content": [
                        {"type": "thinking", "thinking": "not a title"},
                        {"type": "text", "text": "Parser is wired up."},
                    ],
                },
            }),
        ]
    }

    fn scan(home: &Path) -> Vec<TranscriptSummary> {
        scan_claude(&ScanOptions::new(home)).unwrap()
    }

    #[test]
    fn a_transcript_yields_its_session() {
        let home = tempfile::tempdir().unwrap();
        write_transcript(home.path(), "proj-a", "s1.jsonl", &sample_lines("sess-1"));

        let found = scan(home.path());
        assert_eq!(found.len(), 1);
        let summary = &found[0];
        assert_eq!(summary.session_id, "sess-1");
        assert_eq!(summary.cwd, r"C:\synthetic\project");
        assert_eq!(
            summary.last_timestamp.as_deref(),
            Some("2026-08-23T10:00:05.000Z")
        );
        assert_eq!(summary.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(summary.title.as_deref(), Some("Parser is wired up."));
    }

    #[test]
    fn the_summary_record_is_the_fallback_title() {
        let home = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            "proj-a",
            "s1.jsonl",
            &[
                serde_json::json!({"type": "summary", "summary": "Wiring up the parser"}),
                serde_json::json!({
                    "type": "user",
                    "sessionId": "sess-1",
                    "cwd": r"C:\synthetic\project",
                    "timestamp": "2026-08-23T10:00:00.000Z",
                }),
            ],
        );

        let found = scan(home.path());
        assert_eq!(found[0].title.as_deref(), Some("Wiring up the parser"));
        assert!(found[0].model.is_none());
    }

    #[test]
    fn the_last_assistant_message_wins() {
        let home = tempfile::tempdir().unwrap();
        let mut lines = sample_lines("sess-1");
        lines.push(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "2026-08-23T10:01:00.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "And now the scanner."}],
            },
        }));
        write_transcript(home.path(), "proj-a", "s1.jsonl", &lines);

        let found = scan(home.path());
        assert_eq!(found[0].title.as_deref(), Some("And now the scanner."));
        assert_eq!(found[0].model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            found[0].last_timestamp.as_deref(),
            Some("2026-08-23T10:01:00.000Z")
        );
    }

    #[test]
    fn a_transcript_without_a_cwd_is_discarded() {
        let home = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            "proj-a",
            "no-cwd.jsonl",
            &[serde_json::json!({
                "type": "user",
                "sessionId": "sess-1",
                "timestamp": "2026-08-23T10:00:00.000Z",
            })],
        );
        assert!(scan(home.path()).is_empty());
    }

    #[test]
    fn a_transcript_without_a_session_id_is_discarded() {
        let home = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            "proj-a",
            "no-id.jsonl",
            &[serde_json::json!({"type": "user", "cwd": r"C:\synthetic\project"})],
        );
        assert!(scan(home.path()).is_empty());
    }

    #[test]
    fn subagent_transcripts_are_excluded() {
        let home = tempfile::tempdir().unwrap();
        write_transcript(home.path(), "proj-a", "s1.jsonl", &sample_lines("sess-1"));
        write_transcript(
            home.path(),
            "proj-a/subagents",
            "s2.jsonl",
            &sample_lines("sess-2"),
        );

        let found = scan(home.path());
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].session_id, "sess-1");
    }

    #[test]
    fn non_jsonl_files_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = claude_projects_dir(home.path()).join("proj-a");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "not a transcript").unwrap();
        fs::write(dir.join("config.json"), r#"{"cwd":"x","sessionId":"y"}"#).unwrap();

        assert!(scan(home.path()).is_empty());
    }

    #[test]
    fn a_missing_projects_directory_is_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        assert!(scan(home.path()).is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let home = tempfile::tempdir().unwrap();
        let dir = claude_projects_dir(home.path()).join("proj-a");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mixed.jsonl");
        fs::write(
            &path,
            concat!(
                "this is not json\n",
                "{ broken\n",
                "\n",
                r#"{"type":"user","sessionId":"sess-1","cwd":"C:\\synthetic\\project","timestamp":"2026-08-23T10:00:00.000Z"}"#,
                "\n",
            ),
        )
        .unwrap();

        let found = scan(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "sess-1");
    }

    #[test]
    fn the_file_cap_keeps_the_newest() {
        let home = tempfile::tempdir().unwrap();
        for index in 0..5 {
            let path = write_transcript(
                home.path(),
                "proj-a",
                &format!("s{index}.jsonl"),
                &sample_lines(&format!("sess-{index}")),
            );
            // Stamp descending mtimes so "newest" is deterministic: sess-0 is
            // the newest, sess-4 the oldest.
            let when = SystemTime::now() - Duration::from_secs(60 * (index as u64 + 1));
            set_mtime(&path, when);
        }

        let options = ScanOptions::new(home.path()).with_max_files(2);
        let found = scan_claude(&options).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].session_id, "sess-0");
        assert_eq!(found[1].session_id, "sess-1");
    }

    /// The scan runs every time somebody opens the detail panel, over files
    /// that mostly have not changed. Reading them again is the difference
    /// between a panel that appears and a panel that arrives.
    #[test]
    fn a_second_scan_only_opens_what_has_changed() {
        let home = tempfile::tempdir().unwrap();
        let stable = write_transcript(
            home.path(),
            "proj-a",
            "stable.jsonl",
            &sample_lines("sess-stable"),
        );
        let busy = write_transcript(
            home.path(),
            "proj-a",
            "busy.jsonl",
            &sample_lines("sess-busy"),
        );

        let options = ScanOptions::new(home.path());
        let mut cache = TranscriptCache::new();
        let first = scan_claude_cached(&options, &mut cache).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(cache.len(), 2);

        // Nothing has changed: both come back, and both come from the cache.
        // Proved by making the file unreadable-as-a-transcript and seeing the
        // old answer survive.
        let cached_mtime = cache[&stable].modified;
        std::fs::write(&stable, "not a transcript at all").unwrap();
        set_mtime(&stable, cached_mtime);
        let second = scan_claude_cached(&options, &mut cache).unwrap();
        assert_eq!(second.len(), 2, "the untouched file was not re-read");

        // Touch it, and it is read again — now yielding nothing, because it is
        // no longer a transcript.
        set_mtime(&stable, SystemTime::now());
        let third = scan_claude_cached(&options, &mut cache).unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].session_id, "sess-busy");

        // A file that has gone away leaves no entry behind.
        std::fs::remove_file(&busy).unwrap();
        let fourth = scan_claude_cached(&options, &mut cache).unwrap();
        assert!(fourth.is_empty());
        assert!(cache.is_empty(), "the cache tracks the directory");
    }

    #[test]
    fn files_older_than_the_window_are_never_opened() {
        let home = tempfile::tempdir().unwrap();
        let fresh = write_transcript(home.path(), "proj-a", "new.jsonl", &sample_lines("fresh"));
        let stale = write_transcript(home.path(), "proj-a", "old.jsonl", &sample_lines("stale"));
        set_mtime(&fresh, SystemTime::now() - Duration::from_secs(60));
        set_mtime(
            &stale,
            SystemTime::now() - Duration::from_secs(48 * 60 * 60),
        );

        let found = scan(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "fresh");
    }

    #[test]
    fn results_come_back_newest_first() {
        let home = tempfile::tempdir().unwrap();
        let older = write_transcript(home.path(), "proj-a", "a.jsonl", &sample_lines("older"));
        let newer = write_transcript(home.path(), "proj-b", "b.jsonl", &sample_lines("newer"));
        set_mtime(&older, SystemTime::now() - Duration::from_secs(600));
        set_mtime(&newer, SystemTime::now() - Duration::from_secs(10));

        let found = scan(home.path());
        assert_eq!(
            found
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
    }

    #[test]
    fn a_line_spanning_chunk_boundaries_survives() {
        // One record padded well past CHUNK_SIZE, so the reader must stitch it
        // back together from several reads.
        let filler = "x".repeat(CHUNK_SIZE * 2 + 7);
        let home = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            "proj-a",
            "big.jsonl",
            &[
                serde_json::json!({"type": "user", "padding": filler}),
                serde_json::json!({
                    "type": "assistant",
                    "sessionId": "sess-big",
                    "cwd": r"C:\synthetic\project",
                    "timestamp": "2026-08-23T10:00:00.000Z",
                    "message": {
                        "role": "assistant",
                        "model": "claude-opus-5",
                        "content": [{"type": "text", "text": "after the giant line"}],
                    },
                }),
            ],
        );

        let found = scan(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "sess-big");
        assert_eq!(found[0].title.as_deref(), Some("after the giant line"));
    }

    #[test]
    fn a_final_line_without_a_newline_is_still_read() {
        let home = tempfile::tempdir().unwrap();
        let dir = claude_projects_dir(home.path()).join("proj-a");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("partial.jsonl"),
            r#"{"type":"user","sessionId":"sess-1","cwd":"C:\\synthetic\\project"}"#,
        )
        .unwrap();

        assert_eq!(scan(home.path())[0].session_id, "sess-1");
    }

    #[test]
    fn titles_are_flattened_and_capped() {
        assert_eq!(truncate("  a\n b\tc  ", 100), "a b c");
        let long = "w".repeat(TITLE_LIMIT + 20);
        let capped = truncate(&long, TITLE_LIMIT);
        assert_eq!(capped.chars().count(), TITLE_LIMIT + 3);
        assert!(capped.ends_with("..."));
        // Multi-byte characters are counted, not sliced.
        let wide = "字".repeat(TITLE_LIMIT + 5);
        assert!(truncate(&wide, TITLE_LIMIT).starts_with('字'));
    }

    /// Set a file's mtime without taking on a dependency, by reopening it and
    /// asking the OS through `File::set_modified`.
    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }
}
