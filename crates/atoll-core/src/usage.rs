//! Rate-limit usage for both agents, from the only places each one exposes it.
//!
//! # Claude Code
//!
//! Claude Code hands its status line a `rate_limits` object on stdin and puts it
//! nowhere else. So Atoll's status line bridge ([`crate::install`] installs it,
//! `atoll statusline` runs it) copies that object into a small cache file,
//! [`rl_cache_path`], and everything else reads the cache. The cache is written
//! atomically — a temporary file plus a rename — because the status line fires
//! on every turn while the UI polls the file, and a half-written JSON document
//! would be read exactly as often as it was written.
//!
//! # Codex
//!
//! Codex writes its rate limits into every session's rollout log. The newest
//! `rollout-*.jsonl` under `<home>/.codex/sessions` holds the freshest numbers,
//! in the last record with `type == "event_msg"` and `payload.type ==
//! "token_count"`.
//!
//! ## Field names, verified against real rollout files
//!
//! Checked read-only against this machine's own `~/.codex/sessions` before
//! writing the parser, because the shapes have drifted between Codex builds:
//!
//! - `rate_limits` sits at `payload.rate_limits`, a **sibling of `payload.info`**
//!   — not inside it. `info` is sometimes `null` while `rate_limits` is present.
//! - `plan_type` is **inside `rate_limits`**, not at the payload's top level as
//!   the design note assumed. Recent records read
//!   `{"limit_id":…,"limit_name":…,"primary":…,"secondary":…,"credits":…,
//!   "individual_limit":…,"plan_type":"pro","rate_limit_reached_type":…}`.
//! - `primary` and `secondary` are each either a window object or `null`.
//! - A window carries `used_percent` and `window_minutes`, plus **either**
//!   `resets_at` (absolute Unix seconds, current builds) **or**
//!   `resets_in_seconds` (relative, older builds). Both spellings are still on
//!   disk here, so both are parsed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A byte-order mark, which `serde_json` will not parse past.
pub const BOM: char = '\u{feff}';

/// Parse JSON that another program wrote.
///
/// Windows tools — PowerShell above all, whose `Set-Content` does it by default
/// — write UTF-8 with a byte-order mark, and `serde_json` rejects a document
/// that starts with one. Atoll reads several files it did not write, so every
/// one of those reads comes through here.
///
/// This one cost real time to find: the user's own usage cache parsed as empty
/// and the readout said "usage unavailable" with perfectly good numbers sitting
/// on disk three bytes away.
pub fn parse_foreign_json(text: &str) -> Option<Value> {
    serde_json::from_str(text.trim_start_matches(BOM)).ok()
}

/// Prefix of every Codex rollout log.
const ROLLOUT_PREFIX: &str = "rollout-";

/// How many rollout files a Codex scan will open before giving up.
///
/// The freshest numbers are in the newest file, but a session that ended before
/// its first `token_count` leaves a newer file with nothing in it. Falling
/// through a few keeps a just-closed session from blanking the display.
pub const CODEX_MAX_FILES: usize = 5;

/// Keys in the status line cache. `rateLimits` and `cachedAt` are the original
/// pair; the rest were added later and are absent from caches written by an
/// older Atoll, which is why every read of them is optional.
const CACHE_RATE_LIMITS_KEY: &str = "rateLimits";
const CACHE_AT_KEY: &str = "cachedAt";
const CACHE_MODEL_KEY: &str = "model";
const CACHE_CONTEXT_KEY: &str = "contextPercent";
const CACHE_SESSION_KEY: &str = "sessionId";

/// One rate-limit window, in whichever agent's spelling it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowUsage {
    /// How much of the window is consumed, 0–100.
    pub used_percent: f64,
    /// When the window rolls over, in Unix seconds.
    pub resets_at: Option<u64>,
    /// The window's length. Claude Code does not send this; Codex does.
    pub window_minutes: Option<u64>,
}

impl WindowUsage {
    /// Seconds until the window resets, relative to `now`. `None` when the
    /// agent did not say, and `Some(0)` once the reset time has passed.
    pub fn resets_in(&self, now: u64) -> Option<u64> {
        Some(self.resets_at?.saturating_sub(now))
    }

    /// `used_percent` rounded for display, e.g. `23`.
    pub fn rounded(&self) -> i64 {
        self.used_percent.round() as i64
    }
}

/// Claude Code's two windows, as cached by the status line bridge.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClaudeUsage {
    pub five_hour: Option<WindowUsage>,
    pub seven_day: Option<WindowUsage>,
    /// When the bridge last wrote the cache, in Unix seconds.
    pub cached_at: Option<u64>,
}

impl ClaudeUsage {
    /// Whether either window carried a number.
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }
}

/// Everything the status line bridge caches about Claude Code, which is
/// everything Atoll knows about it between hook events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeStatus {
    pub usage: ClaudeUsage,
    /// `model.display_name`, e.g. `Opus 5`.
    pub model: Option<String>,
    /// `context_window.used_percentage`, 0–100.
    pub context_percent: Option<f64>,
    /// The session whose turn last wrote the cache.
    pub session_id: Option<String>,
    /// When the bridge last wrote, in Unix seconds.
    pub cached_at: Option<u64>,
}

impl ClaudeStatus {
    /// Whether the cache has told us anything at all yet.
    pub fn is_empty(&self) -> bool {
        self.usage.is_empty() && self.model.is_none() && self.context_percent.is_none()
    }
}

/// What one status line payload contributes to the cache.
///
/// Kept apart from [`ClaudeStatus`] because a write is a *merge*: a turn whose
/// payload carried no `rate_limits` must not erase the ones the previous turn
/// did, so absent means "leave alone" here and "unknown" there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusFields {
    pub rate_limits: Option<Value>,
    pub model: Option<String>,
    pub context_percent: Option<f64>,
    pub session_id: Option<String>,
}

impl StatusFields {
    /// Whether this payload said anything worth writing down.
    pub fn is_empty(&self) -> bool {
        self.rate_limits.is_none()
            && self.model.is_none()
            && self.context_percent.is_none()
            && self.session_id.is_none()
    }
}

// ------------------------------------------------- Claude's OAuth usage API

/// `GET` here with the OAuth token and the beta header to read Claude Code's
/// rate-limit windows.
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta the endpoint is gated behind.
pub const CLAUDE_USAGE_BETA: &str = "oauth-2025-04-20";

/// How long a reading from the endpoint is reused before asking again.
///
/// The endpoint rate-limits **per token**, and the token is shared with
/// whatever else the user runs against it — their own status line script,
/// and Claude Code itself — so Atoll does not ask more often than this, and
/// mostly does not ask at all: a reading any of those others already fetched
/// counts (see `usage_cache::fetch_claude_limits` in the app), and only its
/// absence sends Atoll to the network. How soon a *failed* ask is retried is
/// likewise the caller's business.
pub const CLAUDE_USAGE_TTL_SECS: u64 = 60;

const CACHE_LIMITS_KEY: &str = "limits";
const CACHE_FETCHED_AT_KEY: &str = "fetchedAt";

/// One rate-limit window, as `/api/oauth/usage` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// `session`, `weekly_all`, or `weekly_scoped`.
    pub kind: String,
    /// What to call it on screen: `5h`, `7d`, or the scoped model's name.
    pub label: String,
    /// How much of the window is gone, 0–100.
    pub percent: f64,
    /// When it rolls over, in Unix seconds.
    pub resets_at: Option<u64>,
}

/// Every window the endpoint reported, in the order it reported them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeLimits {
    pub limits: Vec<UsageLimit>,
    /// When this reading was taken, in Unix seconds.
    pub fetched_at: Option<u64>,
}

impl ClaudeLimits {
    pub fn is_empty(&self) -> bool {
        self.limits.is_empty()
    }

    /// Whether the reading is old enough to be worth replacing.
    pub fn is_stale(&self, now: u64, ttl_secs: u64) -> bool {
        match self.fetched_at {
            Some(at) => now.saturating_sub(at) >= ttl_secs,
            None => true,
        }
    }
}

/// The user's home directory, resolved at runtime. Never a compiled-in path.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `~/.claude/.credentials.json`, resolved at runtime and never written.
pub fn claude_credentials_path() -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the home directory",
        )
    })?;
    Ok(home.join(".claude").join(".credentials.json"))
}

/// Read the OAuth access token Claude Code stores for itself.
///
/// This is a **secret**. It is read into memory, handed to one HTTPS request,
/// and dropped. It is never logged, never written anywhere, and never included
/// in an error message — which is why the failure case here is a bare
/// `Option` rather than an error carrying context about what it found.
pub fn read_claude_oauth_token(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let root = parse_foreign_json(&raw)?;
    root.get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// `%LOCALAPPDATA%\Atoll\claude-usage.json`.
///
/// Deliberately not the file the user's own status line script caches into:
/// two processes writing one path is a corruption waiting to happen, and that
/// file is theirs.
pub fn claude_usage_cache_path() -> io::Result<PathBuf> {
    Ok(atoll_data_dir()?.join("claude-usage.json"))
}

/// The user's own status line cache, read-only.
///
/// Not a last resort but a first one: every reading in it is a request the
/// user's own tooling already spent against the shared rate limit, and a
/// reading Atoll gets for free.
pub fn foreign_usage_cache_path() -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the home directory",
        )
    })?;
    Ok(home.join(".claude").join("statusline-usage-cache.json"))
}

/// Parse the endpoint's response, or a cache of one.
///
/// Accepts both the bare response (`{"limits": [...]}`) and Atoll's wrapper
/// around it, so the same function reads the network and the disk.
pub fn parse_claude_limits(value: &Value) -> ClaudeLimits {
    let limits = value
        .get(CACHE_LIMITS_KEY)
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(parse_limit).collect())
        .unwrap_or_default();
    ClaudeLimits {
        limits,
        fetched_at: number(value.get(CACHE_FETCHED_AT_KEY)).map(|seconds| seconds as u64),
    }
}

fn parse_limit(entry: &Value) -> Option<UsageLimit> {
    let kind = entry
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let percent = number(entry.get("percent"))?;

    // A scoped weekly window is per-model, and the model's name is the only
    // thing that tells two of them apart.
    let scoped = entry
        .get("scope")
        .and_then(|scope| scope.get("model"))
        .and_then(|model| model.get("display_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());

    // The names the readout shows. A scoped weekly window goes by its model,
    // which is the only thing that tells two of them apart.
    let label = match kind {
        "session" => "Session".to_string(),
        "weekly_all" => "Week".to_string(),
        _ => scoped.unwrap_or("Week").to_string(),
    };

    Some(UsageLimit {
        kind: kind.to_string(),
        label,
        percent,
        resets_at: entry
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_iso8601),
    })
}

/// Cache a response body, stamped with when it was taken.
pub fn write_claude_usage_cache(path: &Path, body: &Value, now: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut wrapper = match body {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    wrapper.insert(CACHE_FETCHED_AT_KEY.to_string(), Value::from(now));

    let mut text = serde_json::to_string(&Value::Object(wrapper)).map_err(io::Error::other)?;
    text.push('\n');
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temporary, text)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Read a cached response. `Ok(None)` for a file that is missing or unreadable.
pub fn read_claude_usage_cache(path: &Path) -> io::Result<Option<ClaudeLimits>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(root) = parse_foreign_json(&text) else {
        return Ok(None);
    };
    let limits = parse_claude_limits(&root);
    Ok((!limits.is_empty()).then_some(limits))
}

/// Seconds since the Unix epoch for an RFC 3339 timestamp.
///
/// Written out rather than pulled in: the shapes that actually arrive are
/// `2026-08-24T00:39:59.848967+08:00` and its `Z` variant, and a date-time crate
/// would be a large dependency for one field.
pub fn parse_iso8601(text: &str) -> Option<u64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    // Split the offset off the end before touching the clock.
    let (clock, offset_secs) = match rest.rfind(['+', '-']) {
        Some(index) => {
            let (clock, offset) = rest.split_at(index);
            (clock, parse_offset(offset)?)
        }
        None => (rest.trim_end_matches(['Z', 'z']), 0),
    };

    let clock = clock.split('.').next()?;
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next().unwrap_or("0").parse().ok()?;

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_secs;
    u64::try_from(seconds).ok()
}

fn parse_offset(text: &str) -> Option<i64> {
    let sign = match text.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits = &text[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if digits.len() == 4 => digits.split_at(2),
        None => (digits, "0"),
    };
    Some(sign * (hours.parse::<i64>().ok()? * 3_600 + minutes.parse::<i64>().ok()? * 60))
}

/// Days from 1970-01-01 to a proleptic Gregorian date. Howard Hinnant's
/// `days_from_civil`, which is the shortest correct way to do this.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Codex's two windows, plus the plan they belong to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexUsage {
    /// The short window — five hours on current plans.
    pub primary: Option<WindowUsage>,
    /// The long window — a week on current plans.
    pub secondary: Option<WindowUsage>,
    /// `rate_limits.plan_type`, e.g. `pro`.
    pub plan_type: Option<String>,
    /// The rollout file these numbers came from.
    pub source: Option<PathBuf>,
}

impl CodexUsage {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none()
    }
}

// ------------------------------------------------------------------- Claude

/// `%LOCALAPPDATA%\Atoll\rl.json`.
pub fn rl_cache_path() -> io::Result<PathBuf> {
    Ok(atoll_data_dir()?.join("rl.json"))
}

/// `%LOCALAPPDATA%\Atoll`, falling back to `<home>\.atoll` on a machine where
/// `LOCALAPPDATA` is not set (a service account, or a non-Windows test host).
pub fn atoll_data_dir() -> io::Result<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(local).join("Atoll"));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither LOCALAPPDATA nor a home directory is set",
            )
        })?;
    Ok(PathBuf::from(home).join(".atoll"))
}

/// Pull everything worth caching out of a status line payload.
///
/// Every field is optional, because this is another program's output and a
/// missing key is a display that says a little less rather than a bridge that
/// fails.
pub fn status_fields(payload: &Value) -> StatusFields {
    StatusFields {
        rate_limits: payload
            .get("rate_limits")
            .filter(|value| !value.is_null())
            .cloned(),
        model: payload
            .get("model")
            .and_then(|model| model.get("display_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        context_percent: number(
            payload
                .get("context_window")
                .and_then(|window| window.get("used_percentage")),
        ),
        session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    }
}

/// Merge one status line payload's fields into the cache.
///
/// A *merge*, not a rewrite: Claude Code sends `rate_limits` on some turns and
/// not others, and a turn that only carried a model name must not blank out the
/// percentages the UI is showing.
///
/// Written through a temporary file and renamed into place, so a reader either
/// sees the previous cache or the new one and never a partial write. On Windows
/// `fs::rename` maps to `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`, which is
/// what makes replacing the existing cache legal.
pub fn write_status_cache(path: &Path, fields: &StatusFields, now: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Start from whatever is already there — but only if it is recognisably one
    // of ours. A hand-written bare `rate_limits` object is a debugging
    // affordance the reader accepts; merging into it would produce a hybrid
    // that is neither shape.
    let mut wrapper = fs::read_to_string(path)
        .ok()
        .and_then(|text| parse_foreign_json(&text))
        .and_then(|value| match value {
            Value::Object(map)
                if map.contains_key(CACHE_AT_KEY) || map.contains_key(CACHE_RATE_LIMITS_KEY) =>
            {
                Some(map)
            }
            _ => None,
        })
        .unwrap_or_default();

    wrapper.insert(CACHE_AT_KEY.to_string(), Value::from(now));
    if let Some(rate_limits) = &fields.rate_limits {
        wrapper.insert(CACHE_RATE_LIMITS_KEY.to_string(), rate_limits.clone());
    }
    if let Some(model) = &fields.model {
        wrapper.insert(CACHE_MODEL_KEY.to_string(), Value::String(model.clone()));
    }
    if let Some(percent) = fields.context_percent {
        wrapper.insert(CACHE_CONTEXT_KEY.to_string(), Value::from(percent));
    }
    if let Some(session_id) = &fields.session_id {
        wrapper.insert(
            CACHE_SESSION_KEY.to_string(),
            Value::String(session_id.clone()),
        );
    }

    let mut text = serde_json::to_string(&Value::Object(wrapper)).map_err(io::Error::other)?;
    text.push('\n');

    // The pid keeps two status lines racing on the same turn from clobbering
    // each other's temporary file; the rename settles which one wins.
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temporary, text)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Read the cache. `Ok(None)` when it has never been written; a cache that
/// exists but does not parse is treated the same way, because a stale display
/// beats a status line that errors out.
///
/// Reads three generations of the file: the current wrapper, the older one that
/// carried only `cachedAt` and `rateLimits`, and a bare `rate_limits` object
/// hand-written for debugging. Missing keys simply come back as `None`.
pub fn read_status_cache(path: &Path) -> io::Result<Option<ClaudeStatus>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(root) = parse_foreign_json(&text) else {
        return Ok(None);
    };

    let wrapped = root.get(CACHE_RATE_LIMITS_KEY);
    let rate_limits = wrapped.unwrap_or(&root);
    let cached_at = number(root.get(CACHE_AT_KEY)).map(|seconds| seconds as u64);

    let mut usage = parse_claude_rate_limits(rate_limits);
    usage.cached_at = cached_at;

    Ok(Some(ClaudeStatus {
        usage,
        model: root
            .get(CACHE_MODEL_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        context_percent: number(root.get(CACHE_CONTEXT_KEY)),
        session_id: root
            .get(CACHE_SESSION_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        cached_at,
    }))
}

/// Just the rate-limit half of [`read_status_cache`].
pub fn read_rl_cache(path: &Path) -> io::Result<Option<ClaudeUsage>> {
    Ok(read_status_cache(path)?.map(|status| status.usage))
}

/// Parse Claude Code's `rate_limits` object.
///
/// Every field is optional and every number is accepted as either a JSON number
/// or a string: this object is read from another program's output, so being
/// strict about it would trade a working display for a correct complaint.
pub fn parse_claude_rate_limits(value: &Value) -> ClaudeUsage {
    ClaudeUsage {
        five_hour: parse_window(value.get("five_hour").or_else(|| value.get("fiveHour"))),
        seven_day: parse_window(value.get("seven_day").or_else(|| value.get("sevenDay"))),
        cached_at: None,
    }
}

// -------------------------------------------------------------------- Codex

/// `<home>/.codex/sessions`.
pub fn codex_sessions_dir(home: &Path) -> PathBuf {
    home.join(".codex").join("sessions")
}

/// The freshest rate limits Codex has written under `home`.
///
/// `Ok(None)` when Codex has never run here, or when none of the files examined
/// carried a `token_count`. Never an error for an unreadable individual file.
pub fn scan_codex_usage(home: &Path) -> io::Result<Option<CodexUsage>> {
    let mut rollouts = Vec::new();
    collect_rollouts(&codex_sessions_dir(home), &mut rollouts)?;
    rollouts.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));

    for (path, _) in rollouts.into_iter().take(CODEX_MAX_FILES) {
        if let Some(mut usage) = read_codex_rollout(&path)? {
            usage.source = Some(path);
            return Ok(Some(usage));
        }
    }
    Ok(None)
}

/// The last `token_count` event in one rollout file, or `None` if it has none.
pub fn read_codex_rollout(path: &Path) -> io::Result<Option<CodexUsage>> {
    let mut latest = None;
    crate::transcript::for_each_line(path, |line| {
        // Cheap reject before parsing: the vast majority of rollout lines are
        // message records that cannot possibly match.
        if !line.contains("token_count") {
            return;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            return;
        }
        let Some(payload) = record.get("payload") else {
            return;
        };
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            return;
        }
        // `rate_limits` is a sibling of `payload.info`, and `info` is sometimes
        // null while the limits are present. See this module's header.
        let Some(rate_limits) = payload.get("rate_limits") else {
            return;
        };
        let usage = parse_codex_rate_limits(rate_limits);
        if !usage.is_empty() {
            latest = Some(usage);
        }
    })?;
    Ok(latest)
}

/// Parse Codex's `rate_limits` object.
pub fn parse_codex_rate_limits(value: &Value) -> CodexUsage {
    CodexUsage {
        primary: parse_window(value.get("primary")),
        secondary: parse_window(value.get("secondary")),
        plan_type: value
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty() && *plan != "unknown")
            .map(str::to_string),
        source: None,
    }
}

fn collect_rollouts(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
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
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rollouts(&path, out)?;
            continue;
        }
        let is_rollout = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(ROLLOUT_PREFIX) && name.ends_with(".jsonl"));
        if is_rollout && let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
            out.push((path, modified));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ parsing

/// One window in either agent's spelling, or `None` when the value is absent,
/// `null`, or carries no percentage.
fn parse_window(value: Option<&Value>) -> Option<WindowUsage> {
    let value = value?;
    if value.is_null() {
        return None;
    }

    // Claude Code says `used_percentage`; Codex says `used_percent`. Some
    // builds of both report the same thing as `utilization`.
    let used_percent = number(value.get("used_percentage"))
        .or_else(|| number(value.get("usedPercentage")))
        .or_else(|| number(value.get("used_percent")))
        .or_else(|| number(value.get("usedPercent")))
        .or_else(|| number(value.get("utilization")))?;

    // Absolute reset time where available; older Codex builds only give the
    // remaining seconds, which is useless without knowing when it was written —
    // so it is resolved against the current clock at parse time.
    let resets_at = number(value.get("resets_at"))
        .or_else(|| number(value.get("resetsAt")))
        .map(|seconds| seconds as u64)
        .or_else(|| {
            let remaining = number(value.get("resets_in_seconds"))
                .or_else(|| number(value.get("resetsInSeconds")))?;
            Some(crate::now_unix_secs().saturating_add(remaining.max(0.0) as u64))
        });

    Some(WindowUsage {
        used_percent: used_percent.clamp(0.0, 100.0),
        resets_at,
        window_minutes: number(value.get("window_minutes"))
            .or_else(|| number(value.get("windowMinutes")))
            .map(|minutes| minutes as u64),
    })
}

/// A number, whether it arrived as one or as a string like `"23.5"` or `"23%"`.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(text) => text.trim().trim_end_matches('%').trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const NOW: u64 = 1_787_000_000;

    // ------------------------------------------------------------- Claude

    #[test]
    fn both_claude_windows_parse() {
        let usage = parse_claude_rate_limits(&json!({
            "five_hour": {"used_percentage": 23.5, "resets_at": NOW + 3_600},
            "seven_day": {"used_percentage": 61.0, "resets_at": NOW + 86_400},
        }));

        let five = usage.five_hour.unwrap();
        assert_eq!(five.used_percent, 23.5);
        assert_eq!(five.rounded(), 24);
        assert_eq!(five.resets_at, Some(NOW + 3_600));
        assert_eq!(five.resets_in(NOW), Some(3_600));
        assert_eq!(usage.seven_day.unwrap().used_percent, 61.0);
    }

    #[test]
    fn either_claude_window_may_be_missing() {
        let only_five = parse_claude_rate_limits(&json!({"five_hour": {"used_percentage": 5}}));
        assert!(only_five.five_hour.is_some());
        assert!(only_five.seven_day.is_none());

        let neither = parse_claude_rate_limits(&json!({}));
        assert!(neither.is_empty());

        let nulled =
            parse_claude_rate_limits(&json!({"five_hour": null, "seven_day": {"utilization": 3}}));
        assert!(nulled.five_hour.is_none());
        assert_eq!(nulled.seven_day.unwrap().used_percent, 3.0);
    }

    #[test]
    fn numbers_are_accepted_as_strings_too() {
        let usage = parse_claude_rate_limits(&json!({
            "five_hour": {"used_percentage": "23.5", "resets_at": "1787003600"},
            "seven_day": {"used_percentage": "61%"},
        }));
        assert_eq!(usage.five_hour.unwrap().used_percent, 23.5);
        assert_eq!(usage.five_hour.unwrap().resets_at, Some(1_787_003_600));
        assert_eq!(usage.seven_day.unwrap().used_percent, 61.0);
    }

    #[test]
    fn a_window_with_no_percentage_is_no_window() {
        let usage = parse_claude_rate_limits(&json!({
            "five_hour": {"resets_at": NOW},
            "seven_day": {"used_percentage": "not a number"},
        }));
        assert!(usage.is_empty());
    }

    #[test]
    fn percentages_are_clamped_to_a_sane_range() {
        let usage = parse_claude_rate_limits(&json!({
            "five_hour": {"used_percentage": 140},
            "seven_day": {"used_percentage": -3},
        }));
        assert_eq!(usage.five_hour.unwrap().used_percent, 100.0);
        assert_eq!(usage.seven_day.unwrap().used_percent, 0.0);
    }

    /// A whole status line payload, in the shape Claude Code sends one.
    fn status_payload() -> Value {
        json!({
            "session_id": "s-cache-1",
            "model": {"display_name": "Opus 5"},
            "context_window": {"used_percentage": 42.4},
            "rate_limits": {
                "five_hour": {"used_percentage": 23.5, "resets_at": NOW + 3_600},
                "seven_day": {"used_percentage": 61.0, "resets_at": NOW + 86_400},
            },
        })
    }

    fn rate_limits_only(used: f64) -> StatusFields {
        StatusFields {
            rate_limits: Some(json!({"five_hour": {"used_percentage": used}})),
            ..StatusFields::default()
        }
    }

    /// The endpoint's real response shape, captured from a live call. The
    /// numbers are the only thing invented; the field names, the three `kind`
    /// values, and the RFC 3339 `resets_at` are what it actually sends.
    fn oauth_response() -> Value {
        json!({"limits": [
            {"kind": "session", "group": "session", "percent": 10,
             "severity": "normal", "resets_at": "2026-08-24T00:39:59.848967+08:00",
             "scope": null, "is_active": false},
            {"kind": "weekly_all", "group": "weekly", "percent": 31,
             "severity": "normal", "resets_at": "2026-08-29T07:59:59.848987+08:00",
             "scope": null, "is_active": true},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 27,
             "severity": "normal", "resets_at": "2026-08-29T07:59:59.849189+08:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
             "is_active": false},
        ]})
    }

    #[test]
    fn the_oauth_response_parses_into_one_limit_per_window() {
        let limits = parse_claude_limits(&oauth_response());
        assert_eq!(limits.limits.len(), 3);

        assert_eq!(limits.limits[0].label, "Session");
        assert_eq!(limits.limits[0].percent, 10.0);
        assert_eq!(limits.limits[1].label, "Week");
        // A scoped weekly window is named after its model: that is the only
        // thing that tells two of them apart.
        assert_eq!(limits.limits[2].kind, "weekly_scoped");
        assert_eq!(limits.limits[2].label, "Fable");
        assert_eq!(limits.limits[2].percent, 27.0);

        // 2026-08-24T00:39:59+08:00 is 2026-08-23T16:39:59Z.
        let session_reset = limits.limits[0].resets_at.unwrap();
        assert_eq!(session_reset % 60, 59);
        assert!(limits.limits[1].resets_at.unwrap() > session_reset);
    }

    #[test]
    fn a_scoped_window_without_a_model_still_gets_a_name() {
        let limits = parse_claude_limits(&json!({"limits": [
            {"kind": "weekly_scoped", "percent": 5, "scope": null},
            {"kind": "something_new", "percent": 6},
        ]}));
        assert_eq!(limits.limits.len(), 2);
        assert_eq!(limits.limits[0].label, "Week");
        assert_eq!(
            limits.limits[1].label, "Week",
            "an unknown kind still renders"
        );
    }

    #[test]
    fn a_limit_with_no_percentage_is_not_a_limit() {
        let limits = parse_claude_limits(&json!({"limits": [
            {"kind": "session"},
            {"kind": "weekly_all", "percent": 31},
        ]}));
        assert_eq!(limits.limits.len(), 1);
        assert_eq!(limits.limits[0].label, "Week");

        // And a response with no limits at all is simply empty.
        assert!(parse_claude_limits(&json!({})).is_empty());
        assert!(parse_claude_limits(&Value::Null).is_empty());
    }

    #[test]
    fn rfc_3339_timestamps_parse_in_every_shape_the_api_sends() {
        // Offsets, fractional seconds, and Z, against known epoch seconds.
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(
            parse_iso8601("2026-08-23T16:39:59Z"),
            parse_iso8601("2026-08-24T00:39:59+08:00")
        );
        assert_eq!(
            parse_iso8601("2026-08-24T00:39:59.848967+08:00"),
            parse_iso8601("2026-08-24T00:39:59+08:00")
        );
        assert_eq!(
            parse_iso8601("2026-08-24T00:39:59+0800"),
            parse_iso8601("2026-08-24T00:39:59+08:00")
        );
        // A leap year, and a date before the epoch, which has no representation.
        assert!(parse_iso8601("2024-02-29T12:00:00Z").is_some());
        assert_eq!(parse_iso8601("1969-12-31T23:59:59Z"), None);
        assert_eq!(parse_iso8601("not a timestamp"), None);
        assert_eq!(parse_iso8601(""), None);
    }

    /// PowerShell writes UTF-8 with a byte-order mark by default, and
    /// `serde_json` will not parse past one. The user's own usage cache is
    /// written exactly that way, and Atoll read it as empty — showing "usage
    /// unavailable" with good numbers three bytes out of reach — until this was
    /// handled.
    #[test]
    fn a_byte_order_mark_does_not_hide_a_perfectly_good_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("statusline-usage-cache.json");

        fs::write(&path, format!("{BOM}{}", oauth_response())).unwrap();
        let limits = read_claude_usage_cache(&path)
            .unwrap()
            .expect("a BOM must not make a cache invisible");
        assert_eq!(limits.limits.len(), 3);

        // And the same for the credentials file, which Claude Code writes.
        let credentials = dir.path().join(".credentials.json");
        fs::write(
            &credentials,
            format!("{BOM}{{\"claudeAiOauth\":{{\"accessToken\":\"fake-token-for-tests\"}}}}"),
        )
        .unwrap();
        assert_eq!(
            read_claude_oauth_token(&credentials).as_deref(),
            Some("fake-token-for-tests")
        );

        assert!(parse_foreign_json(&format!("{BOM}{{}}")).is_some());
        assert!(parse_foreign_json("{ not json").is_none());
    }

    #[test]
    fn the_usage_cache_round_trips_and_ages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("claude-usage.json");

        write_claude_usage_cache(&path, &oauth_response(), NOW).unwrap();
        let limits = read_claude_usage_cache(&path).unwrap().unwrap();
        assert_eq!(limits.limits.len(), 3);
        assert_eq!(limits.fetched_at, Some(NOW));

        assert!(!limits.is_stale(NOW + CLAUDE_USAGE_TTL_SECS - 1, CLAUDE_USAGE_TTL_SECS));
        assert!(limits.is_stale(NOW + CLAUDE_USAGE_TTL_SECS, CLAUDE_USAGE_TTL_SECS));
        // A reading that never happened is stale by definition.
        assert!(ClaudeLimits::default().is_stale(0, CLAUDE_USAGE_TTL_SECS));

        // A missing or broken cache reads as nothing rather than failing.
        let missing = dir.path().join("absent.json");
        assert!(read_claude_usage_cache(&missing).unwrap().is_none());
        fs::write(&missing, "{ not json").unwrap();
        assert!(read_claude_usage_cache(&missing).unwrap().is_none());
    }

    /// The user's own status line script keeps a cache in exactly the endpoint's
    /// shape, which is why the same reader handles both.
    #[test]
    fn a_bare_response_body_reads_as_a_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("statusline-usage-cache.json");
        fs::write(&path, oauth_response().to_string()).unwrap();

        let limits = read_claude_usage_cache(&path).unwrap().unwrap();
        assert_eq!(limits.limits.len(), 3);
        assert_eq!(limits.fetched_at, None, "no stamp, so treated as stale");
        assert!(limits.is_stale(NOW, CLAUDE_USAGE_TTL_SECS));
    }

    /// The token is a secret. This test uses an obvious fake, and no real one
    /// may ever appear in this repository.
    #[test]
    fn the_oauth_token_is_read_from_the_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");

        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"fake-token-for-tests","refreshToken":"x"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_claude_oauth_token(&path).as_deref(),
            Some("fake-token-for-tests")
        );

        // Every way it can be absent reads as absent, never as an empty token.
        for body in [
            r#"{}"#,
            r#"{"claudeAiOauth":{}}"#,
            r#"{"claudeAiOauth":{"accessToken":""}}"#,
            r#"{"claudeAiOauth":{"accessToken":"   "}}"#,
            "not json",
        ] {
            fs::write(&path, body).unwrap();
            assert_eq!(read_claude_oauth_token(&path), None, "for {body}");
        }
        assert_eq!(read_claude_oauth_token(&dir.path().join("absent")), None);
    }

    #[test]
    fn the_cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("rl.json");

        let payload = status_payload();
        write_status_cache(&path, &status_fields(&payload), NOW).unwrap();

        let status = read_status_cache(&path).unwrap().unwrap();
        assert_eq!(status.cached_at, Some(NOW));
        assert_eq!(status.usage.cached_at, Some(NOW));
        assert_eq!(status.usage.five_hour.unwrap().used_percent, 23.5);
        assert_eq!(status.usage.seven_day.unwrap().used_percent, 61.0);
        assert_eq!(status.model.as_deref(), Some("Opus 5"));
        assert_eq!(status.context_percent, Some(42.4));
        assert_eq!(status.session_id.as_deref(), Some("s-cache-1"));
        assert!(!status.is_empty());

        // The wrapper is exactly what the module documents.
        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["cachedAt"], json!(NOW));
        assert_eq!(raw["rateLimits"], payload["rate_limits"]);
        assert_eq!(raw["model"], json!("Opus 5"));
        assert_eq!(raw["contextPercent"], json!(42.4));
        assert_eq!(raw["sessionId"], json!("s-cache-1"));
    }

    #[test]
    fn a_payload_that_carries_nothing_says_so() {
        assert!(status_fields(&json!({})).is_empty());
        assert!(status_fields(&Value::Null).is_empty());
        // Blank strings are nothing, not empty names to render.
        assert!(
            status_fields(&json!({"model": {"display_name": "  "}, "session_id": ""})).is_empty()
        );
        assert!(status_fields(&json!({"rate_limits": null})).is_empty());
        assert!(!status_fields(&json!({"model": {"display_name": "Opus 5"}})).is_empty());
    }

    /// Claude Code sends `rate_limits` on some turns and not others. A turn that
    /// only carried a model name must not blank the percentages on screen.
    #[test]
    fn a_later_write_merges_rather_than_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl.json");

        write_status_cache(&path, &status_fields(&status_payload()), NOW).unwrap();
        write_status_cache(
            &path,
            &status_fields(&json!({
                "model": {"display_name": "Haiku 4.5"},
                "context_window": {"used_percentage": 3},
            })),
            NOW + 60,
        )
        .unwrap();

        let status = read_status_cache(&path).unwrap().unwrap();
        assert_eq!(status.model.as_deref(), Some("Haiku 4.5"));
        assert_eq!(status.context_percent, Some(3.0));
        assert_eq!(status.cached_at, Some(NOW + 60));
        assert_eq!(
            status.usage.five_hour.unwrap().used_percent,
            23.5,
            "the rate limits from the earlier turn must survive"
        );
        assert_eq!(
            status.session_id.as_deref(),
            Some("s-cache-1"),
            "and so must the session id"
        );
    }

    /// A cache written by an Atoll that only knew about rate limits.
    #[test]
    fn an_older_cache_reads_without_the_newer_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl.json");
        fs::write(
            &path,
            format!(r#"{{"cachedAt":{NOW},"rateLimits":{{"five_hour":{{"used_percentage":9}}}}}}"#),
        )
        .unwrap();

        let status = read_status_cache(&path).unwrap().unwrap();
        assert_eq!(status.usage.five_hour.unwrap().used_percent, 9.0);
        assert_eq!(status.cached_at, Some(NOW));
        assert_eq!(status.model, None);
        assert_eq!(status.context_percent, None);
        assert_eq!(status.session_id, None);

        // And the next write upgrades it in place without losing anything.
        write_status_cache(
            &path,
            &status_fields(&json!({"model": {"display_name": "Opus 5"}})),
            NOW + 1,
        )
        .unwrap();
        let upgraded = read_status_cache(&path).unwrap().unwrap();
        assert_eq!(upgraded.model.as_deref(), Some("Opus 5"));
        assert_eq!(upgraded.usage.five_hour.unwrap().used_percent, 9.0);
    }

    #[test]
    fn writing_the_cache_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl.json");

        write_status_cache(&path, &rate_limits_only(1.0), NOW).unwrap();
        write_status_cache(&path, &rate_limits_only(2.0), NOW + 1).unwrap();

        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["rl.json"], "a rewrite must replace, not accumulate");
        assert_eq!(
            read_rl_cache(&path)
                .unwrap()
                .unwrap()
                .five_hour
                .unwrap()
                .used_percent,
            2.0
        );
    }

    #[test]
    fn a_missing_or_broken_cache_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl.json");
        assert!(read_rl_cache(&path).unwrap().is_none());

        fs::write(&path, "{ not json").unwrap();
        assert!(read_rl_cache(&path).unwrap().is_none());
    }

    #[test]
    fn an_unwrapped_cache_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl.json");
        fs::write(&path, r#"{"five_hour":{"used_percentage":7}}"#).unwrap();

        let usage = read_rl_cache(&path).unwrap().unwrap();
        assert_eq!(usage.five_hour.unwrap().used_percent, 7.0);
        assert!(usage.cached_at.is_none());

        // Writing over a bare object replaces it with a real wrapper rather
        // than merging into a shape that is neither one thing nor the other.
        write_status_cache(&path, &rate_limits_only(8.0), NOW).unwrap();
        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.get("five_hour").is_none(), "got {raw}");
        assert_eq!(
            raw["rateLimits"]["five_hour"]["used_percentage"],
            json!(8.0)
        );
    }

    // -------------------------------------------------------------- Codex

    /// One `token_count` record in the shape current Codex builds write.
    fn token_count(primary: Value, secondary: Value, plan: &str) -> Value {
        json!({
            "timestamp": "2026-08-23T10:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {"total_tokens": 1_000},
                    "model_context_window": 258_400,
                },
                "rate_limits": {
                    "limit_id": "codex",
                    "limit_name": null,
                    "primary": primary,
                    "secondary": secondary,
                    "credits": null,
                    "individual_limit": null,
                    "plan_type": plan,
                    "rate_limit_reached_type": null,
                },
            },
        })
    }

    fn write_rollout(home: &Path, day: &str, name: &str, lines: &[Value]) -> PathBuf {
        let dir = codex_sessions_dir(home).join(day);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn codex_rate_limits_parse_in_the_current_shape() {
        let usage = parse_codex_rate_limits(&json!({
            "limit_id": "codex",
            "primary": {"used_percent": 23.0, "window_minutes": 10_080, "resets_at": NOW + 600},
            "secondary": null,
            "plan_type": "pro",
        }));

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 23.0);
        assert_eq!(primary.window_minutes, Some(10_080));
        assert_eq!(primary.resets_at, Some(NOW + 600));
        assert!(usage.secondary.is_none());
        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn the_older_relative_reset_spelling_is_understood() {
        // Older builds write `resets_in_seconds` instead of `resets_at`.
        let usage = parse_codex_rate_limits(&json!({
            "primary": {"used_percent": 3.0, "window_minutes": 299, "resets_in_seconds": 17_148},
        }));
        let resets_at = usage
            .primary
            .unwrap()
            .resets_at
            .expect("resolved to absolute");
        let now = crate::now_unix_secs();
        assert!(
            resets_at >= now + 17_100 && resets_at <= now + 17_200,
            "expected roughly now + 17148, got {resets_at} against {now}"
        );
    }

    #[test]
    fn an_unknown_plan_is_reported_as_no_plan() {
        let usage = parse_codex_rate_limits(&json!({
            "primary": {"used_percent": 1.0},
            "plan_type": "unknown",
        }));
        assert!(usage.plan_type.is_none());
    }

    #[test]
    fn the_last_token_count_in_the_file_wins() {
        let home = tempfile::tempdir().unwrap();
        let path = write_rollout(
            home.path(),
            "2026/08/23",
            "rollout-2026-08-23T10-00-00-synthetic.jsonl",
            &[
                json!({"type": "response_item", "payload": {"type": "message"}}),
                token_count(
                    json!({"used_percent": 10.0, "window_minutes": 300, "resets_at": NOW}),
                    json!(null),
                    "pro",
                ),
                json!({"type": "response_item", "payload": {"type": "message"}}),
                token_count(
                    json!({"used_percent": 42.0, "window_minutes": 300, "resets_at": NOW + 60}),
                    json!({"used_percent": 8.0, "window_minutes": 10_080, "resets_at": NOW + 99}),
                    "pro",
                ),
            ],
        );

        let usage = read_codex_rollout(&path).unwrap().unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 42.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 8.0);
    }

    #[test]
    fn a_null_info_does_not_hide_the_rate_limits() {
        let home = tempfile::tempdir().unwrap();
        let path = write_rollout(
            home.path(),
            "2026/08/23",
            "rollout-2026-08-23T10-00-00-synthetic.jsonl",
            &[json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": null,
                    "rate_limits": {"primary": {"used_percent": 12.0}},
                },
            })],
        );
        assert_eq!(
            read_codex_rollout(&path)
                .unwrap()
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            12.0
        );
    }

    #[test]
    fn the_scan_prefers_the_newest_rollout() {
        let home = tempfile::tempdir().unwrap();
        let old = write_rollout(
            home.path(),
            "2026/08/22",
            "rollout-2026-08-22T10-00-00-synthetic.jsonl",
            &[token_count(
                json!({"used_percent": 5.0}),
                json!(null),
                "pro",
            )],
        );
        let new = write_rollout(
            home.path(),
            "2026/08/23",
            "rollout-2026-08-23T10-00-00-synthetic.jsonl",
            &[token_count(
                json!({"used_percent": 77.0}),
                json!(null),
                "prolite",
            )],
        );
        set_mtime(
            &old,
            std::time::SystemTime::now() - std::time::Duration::from_secs(86_400),
        );
        set_mtime(&new, std::time::SystemTime::now());

        let usage = scan_codex_usage(home.path()).unwrap().unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 77.0);
        assert_eq!(usage.plan_type.as_deref(), Some("prolite"));
        assert_eq!(usage.source.as_deref(), Some(new.as_path()));
    }

    #[test]
    fn the_scan_falls_through_a_rollout_with_no_token_count() {
        let home = tempfile::tempdir().unwrap();
        let with_numbers = write_rollout(
            home.path(),
            "2026/08/22",
            "rollout-2026-08-22T10-00-00-synthetic.jsonl",
            &[token_count(
                json!({"used_percent": 31.0}),
                json!(null),
                "pro",
            )],
        );
        // A session that ended before Codex ever reported usage.
        let empty = write_rollout(
            home.path(),
            "2026/08/23",
            "rollout-2026-08-23T10-00-00-synthetic.jsonl",
            &[json!({"type": "session_meta", "payload": {"id": "synthetic"}})],
        );
        set_mtime(
            &with_numbers,
            std::time::SystemTime::now() - std::time::Duration::from_secs(600),
        );
        set_mtime(&empty, std::time::SystemTime::now());

        let usage = scan_codex_usage(home.path()).unwrap().unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 31.0);
    }

    #[test]
    fn files_that_are_not_rollouts_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = codex_sessions_dir(home.path()).join("2026/08/23");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("notes.jsonl"),
            token_count(json!({"used_percent": 99.0}), json!(null), "pro").to_string(),
        )
        .unwrap();

        assert!(scan_codex_usage(home.path()).unwrap().is_none());
    }

    #[test]
    fn a_missing_codex_directory_is_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        assert!(scan_codex_usage(home.path()).unwrap().is_none());
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}
