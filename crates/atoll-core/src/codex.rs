//! Observe Codex sessions from their local rollout events, independently of
//! hooks. No agent settings are changed and no approval replies are fabricated.
//!
//! Windows can leave directory metadata unchanged while a rollout is open, so
//! the cache queries each file's current length too. Liveness comes from event
//! timestamps, never from rescanning a file or copying an old file here.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::protocol::HookSource;
use crate::state::{Phase, STALE_AFTER_SECS, SessionState};
use crate::usage::parse_iso8601;

const READ_BUDGET: u64 = 4 * 1024 * 1024;
const INITIAL_TAIL: u64 = 512 * 1024;
const FILES_PER_SCAN: usize = 32;

#[derive(Default)]
struct Events {
    session: Option<SessionState>,
    turn: Option<String>,
    phase: Option<Phase>,
    last_seen: u64,
    excluded: bool,
}

impl Events {
    fn push(&mut self, line: &[u8]) -> bool {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return false;
        };
        let payload = &record["payload"];
        let timestamp = record["timestamp"].as_str().and_then(parse_iso8601);
        match record["type"].as_str() {
            Some("session_meta") => {
                self.excluded = payload["source"].get("subagent").is_some()
                    || payload["source"].as_str() == Some("subagent")
                    || payload["thread_source"].as_str() == Some("subagent");
                if let Some(id) = payload["id"]
                    .as_str()
                    .or_else(|| payload["session_id"].as_str())
                    .filter(|id| !id.is_empty())
                {
                    let mut session =
                        SessionState::new(id, HookSource::Codex, timestamp.unwrap_or(0));
                    session.cwd = payload["cwd"].as_str().map(str::to_string);
                    self.session = Some(session);
                }
            }
            Some("turn_context") => {
                if let Some(cwd) = payload["cwd"].as_str()
                    && let Some(session) = &mut self.session
                {
                    session.cwd = Some(cwd.to_string());
                }
            }
            Some("event_msg") => {
                let Some(timestamp) = timestamp else {
                    return true;
                };
                let kind = payload["type"].as_str().unwrap_or_default();
                let turn = payload["turn_id"].as_str();
                if kind != "task_started"
                    && let (Some(current), Some(incoming)) = (self.turn.as_deref(), turn)
                    && current != incoming
                {
                    return true;
                }
                match kind {
                    "task_started" | "user_message" => {
                        self.turn = turn.map(str::to_string);
                        self.phase = Some(Phase::Running);
                    }
                    "task_complete" | "turn_aborted" => self.phase = Some(Phase::Completed),
                    // Activity is useful when attaching to an older rollout or
                    // when a very long turn's start is outside the initial tail.
                    "agent_reasoning" | "agent_message" | "item_started" | "item_completed" => {
                        if self.phase.is_none() {
                            self.phase = Some(Phase::Running);
                            self.turn = turn.map(str::to_string);
                        }
                        if payload["phase"].as_str() == Some("final")
                            || payload["item"]["phase"].as_str() == Some("final")
                        {
                            self.phase = Some(Phase::Completed);
                        }
                    }
                    // Token accounting after a completed turn must not make it
                    // look busy again or keep an old session alive forever.
                    _ => return true,
                }
                self.last_seen = self.last_seen.max(timestamp);
                if let Some(session) = &mut self.session {
                    session.last_event = kind.to_string();
                }
            }
            _ => {}
        }
        true
    }

    fn snapshot(&self, path: &Path, now: u64) -> Option<SessionState> {
        if self.excluded
            || self.last_seen == 0
            || now.saturating_sub(self.last_seen) >= STALE_AFTER_SECS
        {
            return None;
        }
        let mut session = self.session.clone()?;
        session.phase = self.phase?;
        session.last_seen = self.last_seen;
        session.transcript_path = Some(path.to_string_lossy().into_owned());
        Some(session)
    }
}

#[derive(Default)]
struct Cursor {
    events: Events,
    offset: u64,
    length: u64,
    modified: Option<SystemTime>,
    skipping_line: bool,
}

impl Cursor {
    fn read(&mut self, path: &Path, length: u64, modified: Option<SystemTime>) -> io::Result<()> {
        if length < self.length
            || length < self.offset
            || (length == self.length && modified != self.modified)
        {
            *self = Self::default();
        }
        let mut file = File::open(path)?;
        if self.offset == 0 && length > READ_BUDGET {
            // Metadata is the first record. For a large existing session, start
            // with its recent events instead of replaying hours of tool output.
            let mut reader = BufReader::new((&mut file).take(READ_BUDGET));
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line)?;
            self.events.push(&line);
            self.offset = length - INITIAL_TAIL;
            self.skipping_line = true;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file.take(READ_BUDGET.min(length - self.offset)));
        let mut line = Vec::new();
        let start = self.offset;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            let terminated = line.ends_with(b"\n");
            if self.skipping_line {
                self.offset += read as u64;
                self.skipping_line = !terminated;
                continue;
            }
            if !terminated && !self.events.push(&line) {
                // Retry a half-written JSON record on the next scan. A record
                // larger than the read budget is skipped until its newline.
                if self.offset + read as u64 - start >= READ_BUDGET {
                    self.offset += read as u64;
                    self.skipping_line = true;
                }
                break;
            }
            if terminated {
                self.events.push(&line);
            }
            self.offset += read as u64;
        }
        self.length = length;
        self.modified = modified;
        Ok(())
    }
}

/// Cursors into append-only rollouts. Only the worker thread owns this cache.
#[derive(Default)]
pub struct SessionCache {
    files: HashMap<PathBuf, Cursor>,
}

impl SessionCache {
    /// `codex_home` is CODEX_HOME, or `<user home>/.codex`. Old files are also
    /// checked for growth: a resumed conversation can live in an old directory.
    pub fn scan(&mut self, codex_home: &Path, now: u64) -> io::Result<Vec<SessionState>> {
        let mut files = Vec::new();
        collect(&codex_home.join("sessions"), &mut files)?;
        let seen: HashSet<_> = files.iter().map(|(path, _, _)| path.clone()).collect();
        files.sort_by_key(|(path, length, modified)| {
            let changed = self
                .files
                .get(path)
                .is_some_and(|old| old.length != *length || old.modified != *modified);
            (std::cmp::Reverse(changed), std::cmp::Reverse(*modified))
        });
        let mut opened = 0;
        for (path, length, modified) in files {
            let cached = self.files.entry(path.clone()).or_default();
            if cached.offset == length && cached.length == length && cached.modified == modified {
                continue;
            }
            if opened >= FILES_PER_SCAN {
                continue;
            }
            opened += 1;
            // A transient sharing error keeps the last good state until its
            // event timestamp ages out, and is retried on the next scan.
            let _ = cached.read(&path, length, modified);
        }
        self.files.retain(|path, _| seen.contains(path));
        Ok(self
            .files
            .iter()
            .filter_map(|(path, cursor)| cursor.events.snapshot(path, now))
            .collect())
    }
}

type RolloutFile = (PathBuf, u64, Option<SystemTime>);

fn collect(dir: &Path, files: &mut Vec<RolloutFile>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            // DirEntry metadata can contain a stale size on Windows while
            // Codex holds the writer open. Query the file itself each time.
            let metadata = fs::metadata(entry.path())?;
            files.push((entry.path(), metadata.len(), metadata.modified().ok()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
