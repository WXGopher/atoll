//! Installation and removal of Atoll's hook wiring in Claude Code and Codex configuration files.
//!
//! # Rules this module plays by
//!
//! `settings.json` belongs to the user, not to Atoll. So:
//!
//! - The file is manipulated as a [`Value`] tree. There is no settings struct,
//!   because a struct would silently drop every key Atoll does not know about.
//! - Entries Atoll owns are recognized by [`MANAGED_MARKER`] appearing in their
//!   `command`. Uninstall removes exactly those and leaves the user's own hooks
//!   — including their own hooks on the same events — untouched.
//! - Every write is preceded by a timestamped backup next to the original.
//! - Output is pretty-printed with sorted keys (`serde_json::Map` is a
//!   `BTreeMap`), so reinstalling produces no spurious diff.
//!
//! # Windows command form
//!
//! Claude Code's hook reference documents two spellings. The *shell form*
//! (`command` only) runs the string through a shell, which on Windows means the
//! quoting rules depend on which shell Claude Code picked — and PowerShell in
//! particular will not execute a leading quoted path without `&`. The *exec
//! form* (`command` plus an `args` array) spawns the process directly with no
//! shell and therefore no quoting rules at all.
//!
//! Atoll writes the exec form **for hooks**. As a safety net for any build that
//! ignores `args` and runs the bare `command`, `command` is the plain absolute
//! path and `atoll-hook` defaults `--source` to `claude` — so the degraded path
//! still works as long as the path has no spaces.
//!
//! `statusLine` is the exception, and learning that cost a user their status
//! bar: its schema is `{type, command, padding?, refreshInterval?}` with no
//! `args` key at all. An entry written in the exec form there runs `atoll.exe`
//! with no subcommand, which prints nothing. See [`statusline_command`].
//!
//! # Where the binaries live
//!
//! Everything `settings.json` names runs on every hook and every turn, so it
//! cannot be a path a build rewrites. [`install_binaries`] copies the running
//! `atoll.exe` and its `atoll-hook.exe` into `%LOCALAPPDATA%\Atollin` first,
//! and the install points at those.
//!
//! # The usage bridge, and why it is no longer installed
//!
//! Atoll used to install itself as Claude Code's status line, because the
//! `rate_limits` object handed to a status line was the only place Claude Code
//! exposed its usage. It is not any more: [`crate::usage`] reads the same
//! numbers from `/api/oauth/usage`, the endpoint Claude Code's own tooling uses,
//! and Atoll no longer needs the slot at all.
//!
//! **The default is now [`BridgePolicy::Skip`]: `statusLine` is not read, not
//! written, and not considered.** Not even when the slot is empty. Occupying it
//! puts Atoll on the critical path of something the user looks at constantly,
//! for a benefit that is now available for free — and every way that went wrong
//! went wrong silently.
//!
//! [`BridgePolicy::Wrap`] survives for anyone who explicitly wants it, with one
//! warning worth repeating: Claude Code latches a status line command that fails
//! repeatedly, and stops running it for the rest of the session.
//!
//! [`BridgePolicy::Wrap`] is the explicit opt-in. It stashes the user's entry
//! under [`ORIGINAL_STATUSLINE_KEY`], carries every key of theirs that Atoll
//! does not own onto the wrapper (`refreshInterval` and anything else Claude
//! Code reads off that object), and `atoll statusline` runs the stashed command
//! with Atoll's own stdout, so its bytes and exit code reach Claude Code
//! untouched. Uninstall puts the original back.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Substring that marks a hook entry as Atoll's. Present in every `command`
/// Atoll writes, because the command is a path ending in `atoll-hook.exe`.
pub const MANAGED_MARKER: &str = "atoll-hook";

/// The `settings.json` key holding the status line entry Atoll displaced.
///
/// Top-level and underscore-prefixed: Claude Code ignores keys it does not know,
/// and the prefix says at a glance that it is not the user's own setting.
pub const ORIGINAL_STATUSLINE_KEY: &str = "_atollOriginalStatusLine";

/// The `settings.json` key Claude Code reads its status line from.
pub const STATUSLINE_KEY: &str = "statusLine";

/// The subcommand Atoll's status line entry invokes.
pub const STATUSLINE_SUBCOMMAND: &str = "statusline";

/// One hook entry Atoll installs into Claude Code.
#[derive(Debug, Clone, Copy)]
pub struct HookSpec {
    /// The `hooks` key this entry lives under, e.g. `PreToolUse`.
    pub event: &'static str,
    /// Matcher for the group, or `None` for events that take no matcher.
    pub matcher: Option<&'static str>,
    /// Per-hook timeout in seconds. `None` leaves Claude Code's default.
    pub timeout: Option<u64>,
}

/// The events Atoll subscribes to in Claude Code.
///
/// `UserPromptSubmit`, `Stop`, `SessionStart`, and `SessionEnd` take no
/// matcher. `PermissionRequest` carries a day-long timeout because it is a
/// human-facing prompt — Claude Code's 60 s default would cancel it long before
/// the user got back to their desk.
pub const CLAUDE_HOOKS: &[HookSpec] = &[
    HookSpec {
        event: crate::protocol::events::SESSION_START,
        matcher: None,
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::USER_PROMPT_SUBMIT,
        matcher: None,
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::STOP,
        matcher: None,
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::SESSION_END,
        matcher: None,
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::NOTIFICATION,
        matcher: Some("*"),
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::PRE_TOOL_USE,
        matcher: Some("*"),
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::POST_TOOL_USE,
        matcher: Some("*"),
        timeout: None,
    },
    HookSpec {
        event: crate::protocol::events::PERMISSION_REQUEST,
        matcher: None,
        timeout: Some(86_400),
    },
];

/// Whether one event's hook is currently wired up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStatus {
    pub event: String,
    pub installed: bool,
    /// The managed `command` found, when installed.
    pub command: Option<String>,
    /// Set when the event could not be touched, e.g. the existing value is not
    /// an array.
    pub note: Option<String>,
}

/// Who owns `settings.json`'s `statusLine`, and whether Atoll is wrapping
/// something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    /// No status line at all.
    Absent,
    /// Atoll's status line, with nothing behind it.
    Installed,
    /// Atoll's status line, delegating to the user's saved original.
    Wrapping,
    /// Someone else's status line. Atoll never removes one of these.
    Foreign,
}

impl BridgeState {
    pub fn as_str(self) -> &'static str {
        match self {
            BridgeState::Absent => "not installed",
            BridgeState::Installed => "installed",
            BridgeState::Wrapping => "installed (wrapping your own status line)",
            BridgeState::Foreign => "another status line is in place",
        }
    }

    /// Whether Atoll's bridge is what Claude Code will run.
    pub fn is_atoll(self) -> bool {
        matches!(self, BridgeState::Installed | BridgeState::Wrapping)
    }
}

/// What an install may do to `statusLine`.
///
/// The default is [`BridgePolicy::Skip`]. Atoll reads Claude Code's usage from
/// `/api/oauth/usage` now, so it has no reason to touch `statusLine` at all —
/// and a setting the user looks at all day is not one to occupy without a
/// reason. The other two variants exist for people who ask for them.
#[derive(Debug, Clone, Copy, Default)]
pub enum BridgePolicy<'a> {
    /// Leave `statusLine` exactly as it is, occupied or not. **The default.**
    #[default]
    Skip,
    /// Install Atoll's status line into an empty slot, and refresh it if it is
    /// already ours. An entry belonging to anyone else is left alone.
    IfEmpty(&'a Path),
    /// Install over an existing entry, stashing it so `atoll statusline` can run
    /// it and pass its output straight through. Only ever on an explicit opt-in.
    Wrap(&'a Path),
}

/// What an install or uninstall did.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub settings_path: PathBuf,
    /// `None` when there was nothing to back up or nothing changed.
    pub backup_path: Option<PathBuf>,
    /// Whether the file was actually rewritten.
    pub changed: bool,
    pub entries: Vec<EntryStatus>,
    /// The state of the usage bridge after the operation.
    pub bridge: BridgeState,
    /// Set when an existing status line was found and deliberately not touched,
    /// because the install had no explicit permission to wrap it.
    pub bridge_left_alone: bool,
}

/// `~/.claude/settings.json`, resolved at runtime.
pub fn claude_settings_path() -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the home directory",
        )
    })?;
    Ok(home.join(".claude").join("settings.json"))
}

/// The `atoll-hook` executable that ships beside the running `atoll`.
pub fn hook_binary_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the running executable has no parent directory",
        )
    })?;
    Ok(dir.join(format!("atoll-hook{}", std::env::consts::EXE_SUFFIX)))
}

/// The absolute path of the running `atoll` executable, which is what the status
/// line entry has to name — Claude Code runs it with an arbitrary working
/// directory, so a relative path would not resolve.
pub fn atoll_binary_path() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// Where Atoll keeps the copies of itself that `settings.json` points at:
/// `%LOCALAPPDATA%\Atoll\bin`.
pub fn stable_bin_dir() -> io::Result<PathBuf> {
    Ok(crate::usage::atoll_data_dir()?.join("bin"))
}

/// The pair of binaries `settings.json` should name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableBinaries {
    pub atoll: PathBuf,
    pub hook: PathBuf,
    /// Whether anything was actually copied. False when Atoll is already
    /// running from the stable directory.
    pub copied: bool,
}

/// Copy the running `atoll.exe` and its sibling `atoll-hook.exe` into
/// [`stable_bin_dir`], and return the paths that belong in `settings.json`.
///
/// # Why a copy at all
///
/// Whatever `settings.json` names is what every hook and every status line
/// invocation runs, for as long as the install lasts. Pointing that at a build
/// directory means a `cargo build` deletes and rewrites the binary under live
/// sessions: the status line goes blank and hooks fail to start for as long as
/// the link step takes. It happened, and the user watched their status bar
/// empty out. A copy under `%LOCALAPPDATA%` is not in anybody's build path.
///
/// Already running from there — the normal case after the first install — copies
/// nothing.
pub fn install_binaries() -> io::Result<StableBinaries> {
    let stable_dir = stable_bin_dir()?;
    let running = atoll_binary_path()?;
    let atoll = stable_dir.join(binary_name("atoll"));
    let hook = stable_dir.join(binary_name("atoll-hook"));

    if same_file(&running, &atoll) {
        return Ok(StableBinaries {
            atoll,
            hook,
            copied: false,
        });
    }

    fs::create_dir_all(&stable_dir)?;
    let source_hook = hook_binary_path()?;
    if !source_hook.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is missing; Atoll installs the hook that ships beside it",
                source_hook.display()
            ),
        ));
    }

    replace_file(&running, &atoll)?;
    replace_file(&source_hook, &hook)?;
    sweep_displaced(&stable_dir);

    Ok(StableBinaries {
        atoll,
        hook,
        copied: true,
    })
}

fn binary_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Copy `source` over `target`, even when `target` is a running program.
///
/// Windows will not let a running executable be overwritten, but it will let one
/// be *renamed*: the process keeps its open handle to the file under its new
/// name and carries on. So the old copy is moved aside, the new one written in
/// its place, and the leftovers swept up on a later install once nothing holds
/// them.
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    match fs::copy(source, target) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() != io::ErrorKind::PermissionDenied => return Err(error),
        Err(_) => {}
    }

    let displaced = target.with_extension(format!("old-{}", crate::now_unix_secs()));
    fs::rename(target, &displaced)?;
    match fs::copy(source, target) {
        Ok(_) => Ok(()),
        Err(error) => {
            // Put it back rather than leaving the user with no binary at all.
            let _ = fs::rename(&displaced, target);
            Err(error)
        }
    }
}

/// Delete the copies displaced by earlier installs, ignoring the ones still
/// running.
fn sweep_displaced(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.starts_with("old-"))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Whether two paths name the same file, comparing case-insensitively because
/// Windows paths do.
fn same_file(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

fn home_dir() -> Option<PathBuf> {
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Add (or refresh) Atoll's hooks in `settings_path`, and treat `statusLine`
/// according to `bridge`.
///
/// Idempotent: managed entries are stripped before insertion, so reinstalling —
/// or installing after moving the binary — replaces rather than duplicates.
/// Writes nothing when the file already says exactly this.
pub fn install_claude(
    settings_path: &Path,
    hook_binary: &Path,
    bridge: BridgePolicy<'_>,
) -> io::Result<InstallReport> {
    let original = read_settings(settings_path)?;
    let mut settings = original.clone();

    let bridge_left_alone = install_statusline(as_object_mut(&mut settings)?, bridge);

    let root = as_object_mut(&mut settings)?;
    // Whether the user already had a `hooks` key. If they did, it stays even
    // when nothing lands in it: an empty object of theirs is still theirs.
    let had_hooks_key = root.contains_key("hooks");
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the \"hooks\" key in settings.json is not an object",
        )
    })?;

    strip_managed(hooks);

    let command = hook_binary.to_string_lossy().to_string();
    let mut entries = Vec::new();
    for spec in CLAUDE_HOOKS {
        let note = insert_hook(hooks, spec, &command).err();
        entries.push(EntryStatus {
            event: spec.event.to_string(),
            installed: note.is_none(),
            command: note.is_none().then(|| command.clone()),
            note,
        });
    }

    if hooks.is_empty() && !had_hooks_key {
        as_object_mut(&mut settings)?.remove("hooks");
    }

    let (changed, backup_path) = write_if_changed(settings_path, &original, &settings)?;
    Ok(InstallReport {
        settings_path: settings_path.to_path_buf(),
        backup_path,
        changed,
        entries,
        bridge: statusline_state(&settings),
        bridge_left_alone,
    })
}

/// Remove every hook entry carrying [`MANAGED_MARKER`] and Atoll's status line,
/// leaving the user's own hooks — and every other settings key — exactly as they
/// were.
pub fn uninstall_claude(settings_path: &Path) -> io::Result<InstallReport> {
    let original = read_settings(settings_path)?;
    let mut settings = original.clone();

    if let Some(hooks) = as_object_mut(&mut settings)?
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
    {
        strip_managed(hooks);
        // An empty `hooks` object left behind is Atoll's litter — install is
        // what creates the key when it is missing — and an empty one does
        // nothing either way. Unlike a *group* the user left empty, there is no
        // reading under which keeping this serves them.
        if hooks.is_empty() {
            as_object_mut(&mut settings)?.remove("hooks");
        }
    }
    uninstall_statusline(as_object_mut(&mut settings)?);

    let (changed, backup_path) = write_if_changed(settings_path, &original, &settings)?;
    let entries = status_from(&settings);
    Ok(InstallReport {
        settings_path: settings_path.to_path_buf(),
        backup_path,
        changed,
        entries,
        bridge: statusline_state(&settings),
        // Uninstall never leaves somebody else's line in place *instead* of
        // doing something; it simply has nothing of ours to take out.
        bridge_left_alone: false,
    })
}

/// Report which of [`CLAUDE_HOOKS`] are currently installed. Read-only.
pub fn status_claude(settings_path: &Path) -> io::Result<Vec<EntryStatus>> {
    Ok(status_from(&read_settings(settings_path)?))
}

/// Report the usage bridge's state. Read-only.
pub fn status_bridge(settings_path: &Path) -> io::Result<BridgeState> {
    Ok(statusline_state(&read_settings(settings_path)?))
}

// -------------------------------------------------------------- usage bridge

/// Who currently owns `statusLine`.
pub fn statusline_state(settings: &Value) -> BridgeState {
    match settings.get(STATUSLINE_KEY) {
        None | Some(Value::Null) => BridgeState::Absent,
        Some(entry) if is_atoll_statusline(entry) => {
            if settings.get(ORIGINAL_STATUSLINE_KEY).is_some() {
                BridgeState::Wrapping
            } else {
                BridgeState::Installed
            }
        }
        Some(_) => BridgeState::Foreign,
    }
}

/// The status line entry Atoll displaced, for `atoll statusline` to delegate to.
pub fn original_statusline(settings: &Value) -> Option<&Value> {
    settings
        .get(ORIGINAL_STATUSLINE_KEY)
        .filter(|value| !value.is_null())
}

/// Read [`ORIGINAL_STATUSLINE_KEY`] straight out of a settings file.
///
/// A missing or unparseable file yields `None` rather than an error: the status
/// line runs on every turn, and a broken `settings.json` must degrade to "no
/// delegation", not to a status line that fails.
pub fn read_original_statusline(settings_path: &Path) -> Option<Value> {
    let settings = read_settings(settings_path).ok()?;
    original_statusline(&settings).cloned()
}

/// Whether a `statusLine` entry is one Atoll wrote.
///
/// Recognized by the pair "names the atoll binary" and "invokes the statusline
/// subcommand", so a user command that merely lives in a directory called
/// `atoll` is not mistaken for ours.
pub fn is_atoll_statusline(entry: &Value) -> bool {
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if command.contains("atoll") && command.contains(STATUSLINE_SUBCOMMAND) {
        return true;
    }

    // Recognise the exec form an older Atoll wrote, so that an install can
    // replace it rather than mistaking it for the user's own status line and
    // stashing it. That form never worked — `statusLine` has no `args` — and
    // this is the only thing that still reads it.
    command.contains("atoll")
        && entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .any(|arg| arg.eq_ignore_ascii_case(STATUSLINE_SUBCOMMAND))
            })
}

/// The keys Atoll owns in a `statusLine` entry. Everything else in the entry
/// belongs to the user and is copied through verbatim.
const STATUSLINE_OWNED_KEYS: [&str; 3] = ["args", "command", "type"];

/// The `command` string for Atoll's status line: the binary and the subcommand,
/// as one shell string.
///
/// **Not the exec form.** Hooks take an `args` array; `statusLine` does not.
/// Claude Code's schema for it is `{type, command, padding?, refreshInterval?}`
/// and nothing else, so an entry carrying `args` runs `atoll.exe` with no
/// subcommand — which prints nothing, and leaves the user staring at a blank
/// status bar. That is exactly what happened in the field.
///
/// The path is quoted only when it needs to be. Claude Code hands this string to
/// `cmd`, whose rules for a command line that both begins and ends with a quote
/// are notoriously conditional; leaving a space-free path bare avoids the
/// question entirely, and a path with a space has no choice.
pub fn statusline_command(atoll_binary: &Path) -> String {
    let path = atoll_binary.to_string_lossy();
    if path.contains(' ') {
        format!("\"{path}\" {STATUSLINE_SUBCOMMAND}")
    } else {
        format!("{path} {STATUSLINE_SUBCOMMAND}")
    }
}

/// `siblings`, when given, is the entry Atoll is replacing. Every key in it that
/// Atoll does not own is carried over — `refreshInterval` above all, which
/// Claude Code reads off this same object and which a wrapper that dropped it
/// would silently reset to the default.
fn statusline_entry(atoll_binary: &Path, siblings: Option<&Map<String, Value>>) -> Value {
    let mut entry = Map::new();
    if let Some(siblings) = siblings {
        for (key, value) in siblings {
            if !STATUSLINE_OWNED_KEYS.contains(&key.as_str()) {
                entry.insert(key.clone(), value.clone());
            }
        }
    }
    entry.insert(
        "command".to_string(),
        Value::String(statusline_command(atoll_binary)),
    );
    entry.insert("type".to_string(), Value::String("command".to_string()));
    Value::Object(entry)
}

/// Apply `policy` to `statusLine`. Returns whether an existing entry was found
/// and deliberately left alone.
fn install_statusline(root: &mut Map<String, Value>, policy: BridgePolicy<'_>) -> bool {
    let atoll_binary = match policy {
        BridgePolicy::Skip => return false,
        BridgePolicy::IfEmpty(binary) | BridgePolicy::Wrap(binary) => binary,
    };

    let current = root.get(STATUSLINE_KEY).cloned();
    let occupied = !matches!(current, None | Some(Value::Null));
    let ours = current.as_ref().is_some_and(is_atoll_statusline);

    if occupied && !ours && !matches!(policy, BridgePolicy::Wrap(_)) {
        // The red line. Somebody else's status line stays somebody else's.
        return true;
    }

    if occupied && !ours && !root.contains_key(ORIGINAL_STATUSLINE_KEY) {
        // Stash before displacing. Guarded on the key being absent because
        // overwriting an existing stash with our own entry is the one way to
        // lose the user's original for good.
        root.insert(
            ORIGINAL_STATUSLINE_KEY.to_string(),
            current.clone().unwrap_or(Value::Null),
        );
    }

    // Siblings come from whatever is in the slot right now: the user's entry on
    // a first wrap, our own — which already carries them — on a refresh.
    let siblings = current.as_ref().and_then(Value::as_object);
    root.insert(
        STATUSLINE_KEY.to_string(),
        statusline_entry(atoll_binary, siblings),
    );
    false
}

/// Take Atoll's status line back out, restoring what it displaced.
fn uninstall_statusline(root: &mut Map<String, Value>) {
    let ours = root.get(STATUSLINE_KEY).is_some_and(is_atoll_statusline);
    let present = !matches!(root.get(STATUSLINE_KEY), None | Some(Value::Null));

    if present && !ours {
        // Someone else's status line is in place — the user replaced ours by
        // hand. Not ours to remove, and the stash stays with it: it is still
        // the only copy of whatever we once displaced.
        return;
    }

    match root.remove(ORIGINAL_STATUSLINE_KEY) {
        Some(original) => {
            root.insert(STATUSLINE_KEY.to_string(), original);
        }
        None => {
            root.remove(STATUSLINE_KEY);
        }
    }
}

fn status_from(settings: &Value) -> Vec<EntryStatus> {
    let hooks = settings.get("hooks").and_then(Value::as_object);
    CLAUDE_HOOKS
        .iter()
        .map(|spec| {
            let command = hooks
                .and_then(|hooks| hooks.get(spec.event))
                .and_then(Value::as_array)
                .and_then(|groups| {
                    groups
                        .iter()
                        .filter_map(|group| group.get("hooks")?.as_array())
                        .flatten()
                        .filter_map(|hook| hook.get("command")?.as_str())
                        .find(|command| command.contains(MANAGED_MARKER))
                        .map(str::to_string)
                });
            EntryStatus {
                event: spec.event.to_string(),
                installed: command.is_some(),
                command,
                note: None,
            }
        })
        .collect()
}

/// Parse `settings_path`, treating a missing file as an empty object.
///
/// A file that exists but does not parse is an error: overwriting it would
/// destroy settings we cannot read.
fn read_settings(settings_path: &Path) -> io::Result<Value> {
    match fs::read_to_string(settings_path) {
        Ok(text) if text.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(text) => {
            serde_json::from_str(text.trim_start_matches(crate::usage::BOM)).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not valid JSON: {error}", settings_path.display()),
                )
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(error) => Err(error),
    }
}

fn as_object_mut(settings: &mut Value) -> io::Result<&mut Map<String, Value>> {
    settings.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "settings.json does not contain a JSON object",
        )
    })
}

/// Drop every managed hook from every event, including events Atoll no longer
/// installs — that is what makes an upgrade that changes the event set clean.
/// Remove every entry carrying [`MANAGED_MARKER`], and the containers that held
/// nothing but those entries.
///
/// The "and the containers" half is careful about whose emptiness it is. A group
/// — or a whole event key — that Atoll emptied goes with its entries, because
/// Atoll is what put it there. One that was *already* empty when we arrived is
/// the user's own, and tidying it away would be a change they did not ask for.
fn strip_managed(hooks: &mut Map<String, Value>) {
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            // Shaped unexpectedly; not ours to touch, let alone delete.
            return true;
        };
        let had_groups = !groups.is_empty();

        groups.retain_mut(|group| {
            let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let had_entries = !entries.is_empty();
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(MANAGED_MARKER))
            });
            !entries.is_empty() || !had_entries
        });

        !groups.is_empty() || !had_groups
    });
}

/// Remove groups whose hook list went empty, then events whose group list did.
/// Without this, uninstall would leave `"Stop": [{"hooks": []}]` behind.
/// Append Atoll's entry to the matching group under `spec.event`, creating the
/// group if needed. `Err(note)` when the existing shape is not something we can
/// safely extend.
fn insert_hook(
    hooks: &mut Map<String, Value>,
    spec: &HookSpec,
    command: &str,
) -> Result<(), String> {
    let entry = hook_entry(spec, command);

    let groups = hooks
        .entry(spec.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let groups = groups
        .as_array_mut()
        .ok_or_else(|| format!("\"{}\" is not an array; left untouched", spec.event))?;

    // Join an existing group with the same matcher rather than adding a second
    // one, so a user's PreToolUse "*" group keeps its shape.
    let existing = groups.iter_mut().find(|group| {
        let matcher = group.get("matcher").and_then(Value::as_str);
        matcher == spec.matcher
    });

    match existing {
        Some(group) => {
            let entries = group
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| {
                    format!(
                        "a \"{}\" group has no \"hooks\" array; left untouched",
                        spec.event
                    )
                })?;
            entries.push(entry);
        }
        None => {
            let mut group = Map::new();
            if let Some(matcher) = spec.matcher {
                group.insert("matcher".to_string(), Value::String(matcher.to_string()));
            }
            group.insert("hooks".to_string(), Value::Array(vec![entry]));
            groups.push(Value::Object(group));
        }
    }
    Ok(())
}

/// One hook entry in exec form. See the module docs for why `args` is separate.
fn hook_entry(spec: &HookSpec, command: &str) -> Value {
    let mut entry = Map::new();
    entry.insert(
        "args".to_string(),
        Value::Array(vec![
            Value::String("--source".to_string()),
            Value::String("claude".to_string()),
        ]),
    );
    entry.insert("command".to_string(), Value::String(command.to_string()));
    if let Some(timeout) = spec.timeout {
        entry.insert("timeout".to_string(), Value::from(timeout));
    }
    entry.insert("type".to_string(), Value::String("command".to_string()));
    Value::Object(entry)
}

/// Back up and rewrite `settings_path`, but only if `updated` differs from
/// `original`. Returns `(changed, backup_path)`.
fn write_if_changed(
    settings_path: &Path,
    original: &Value,
    updated: &Value,
) -> io::Result<(bool, Option<PathBuf>)> {
    if original == updated && settings_path.exists() {
        return Ok((false, None));
    }

    let backup_path = if settings_path.exists() {
        let backup = backup_path_for(settings_path);
        fs::copy(settings_path, &backup)?;
        Some(backup)
    } else {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        None
    };

    let mut text = serde_json::to_string_pretty(updated).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(settings_path, text)?;
    Ok((true, backup_path))
}

/// `settings.json.backup.<ISO8601, colons stripped>`, with a counter if that
/// name is taken (two installs inside one second).
fn backup_path_for(settings_path: &Path) -> PathBuf {
    let stamp = compact_utc(now_unix_secs());
    let base = settings_path.as_os_str().to_string_lossy().to_string();
    let candidate = PathBuf::from(format!("{base}.backup.{stamp}"));
    if !candidate.exists() {
        return candidate;
    }
    for n in 1..1000 {
        let candidate = PathBuf::from(format!("{base}.backup.{stamp}.{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-08-23T114233Z` — ISO 8601 with the colons removed, since they are not
/// legal in Windows filenames.
fn compact_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic-Gregorian date. Cheaper than taking on a date-time dependency for
/// the one timestamp this crate formats.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A synthetic hook path. Never a real one: fixtures must not carry this
    /// machine's layout into the repository.
    fn fake_hook_binary() -> PathBuf {
        PathBuf::from(r"C:\Tools\Atoll\bin\atoll-hook.exe")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Settings with a user's own hook, a custom top-level key, and a user hook
    /// on an event Atoll also wants.
    fn seeded_settings() -> Value {
        serde_json::json!({
            "model": "opus",
            "customUserKey": {"nested": [1, 2, 3]},
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "my-own-linter.exe"}]
                    },
                    {
                        "matcher": "*",
                        "hooks": [{"type": "command", "command": "user-audit.sh"}]
                    }
                ],
                "Stop": [
                    {"hooks": [{"type": "command", "command": "notify-me.exe"}]}
                ]
            }
        })
    }

    fn write_seed(dir: &Path, value: &Value) -> PathBuf {
        let path = dir.join("settings.json");
        fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn install_creates_all_events_when_settings_are_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");

        let report = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        assert!(report.changed);
        assert!(report.backup_path.is_none(), "nothing existed to back up");
        assert!(report.entries.iter().all(|entry| entry.installed));

        let settings = read(&path);
        let hooks = settings["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), CLAUDE_HOOKS.len());
        for spec in CLAUDE_HOOKS {
            let group = &settings["hooks"][spec.event][0];
            assert_eq!(
                group.get("matcher").and_then(Value::as_str),
                spec.matcher,
                "matcher for {}",
                spec.event
            );
            let entry = &group["hooks"][0];
            assert_eq!(entry["type"], "command");
            assert_eq!(
                entry["command"].as_str().unwrap(),
                fake_hook_binary().to_string_lossy()
            );
            assert_eq!(entry["args"], serde_json::json!(["--source", "claude"]));
            assert_eq!(
                entry.get("timeout").and_then(Value::as_u64),
                spec.timeout,
                "timeout for {}",
                spec.event
            );
        }
    }

    #[test]
    fn permission_request_gets_a_day_long_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();

        let settings = read(&path);
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            86_400
        );
        assert!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]
                .get("timeout")
                .is_none(),
            "events without an explicit timeout keep Claude Code's default"
        );
    }

    #[test]
    fn install_preserves_unknown_keys_and_user_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let settings = read(&path);

        assert_eq!(settings["model"], "opus");
        assert_eq!(
            settings["customUserKey"]["nested"],
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");

        // The user's Bash-matcher group is untouched and still alone.
        let bash_group = &settings["hooks"]["PreToolUse"][0];
        assert_eq!(bash_group["matcher"], "Bash");
        assert_eq!(bash_group["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(bash_group["hooks"][0]["command"], "my-own-linter.exe");

        // Atoll joined the existing "*" group instead of adding a second one.
        let star_group = &settings["hooks"]["PreToolUse"][1];
        assert_eq!(star_group["matcher"], "*");
        let entries = star_group["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["command"], "user-audit.sh");
        assert!(
            entries[1]["command"]
                .as_str()
                .unwrap()
                .contains(MANAGED_MARKER)
        );

        // Same for the matcher-less Stop group.
        let stop_entries = settings["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(stop_entries.len(), 2);
        assert_eq!(stop_entries[0]["command"], "notify-me.exe");
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();

        let second = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        assert!(!second.changed, "a no-op install must not rewrite the file");
        assert!(second.backup_path.is_none(), "and must not leave a backup");
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);

        let third = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
        assert!(!third.changed);

        // Exactly one managed entry per event, never a growing pile.
        let settings = read(&path);
        for spec in CLAUDE_HOOKS {
            let managed = settings["hooks"][spec.event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .filter(|entry| entry["command"].as_str().unwrap().contains(MANAGED_MARKER))
                .count();
            assert_eq!(managed, 1, "managed entries for {}", spec.event);
        }
    }

    #[test]
    fn reinstall_replaces_a_stale_binary_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let moved = PathBuf::from(r"D:\Elsewhere\atoll-hook.exe");
        let report = install_claude(&path, &moved, BridgePolicy::Skip).unwrap();

        assert!(report.changed);
        let settings = read(&path);
        let entries = settings["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "the old path was replaced, not kept");
        assert_eq!(
            entries[0]["command"].as_str().unwrap(),
            moved.to_string_lossy()
        );
    }

    #[test]
    fn uninstall_restores_the_original_shape() {
        let dir = tempfile::tempdir().unwrap();
        let original = seeded_settings();
        let path = write_seed(dir.path(), &original);

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let report = uninstall_claude(&path).unwrap();
        assert!(report.changed);
        assert!(report.entries.iter().all(|entry| !entry.installed));

        assert_eq!(
            read(&path),
            original,
            "uninstall must leave the tree exactly as it found it"
        );
    }

    #[test]
    fn uninstall_removes_the_hooks_key_when_it_was_all_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &serde_json::json!({"model": "opus"}));

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        uninstall_claude(&path).unwrap();

        let settings = read(&path);
        assert!(
            settings.get("hooks").is_none(),
            "an emptied hooks key is removed, not left as {{}}"
        );
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn uninstall_is_idempotent_and_safe_on_a_virgin_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());

        let first = uninstall_claude(&path).unwrap();
        assert!(!first.changed, "nothing of ours was installed");
        assert_eq!(read(&path), seeded_settings());

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        uninstall_claude(&path).unwrap();
        let second = uninstall_claude(&path).unwrap();
        assert!(!second.changed);
    }

    #[test]
    fn writes_are_backed_up_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());
        let before = fs::read_to_string(&path).unwrap();

        let report = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let backup = report.backup_path.expect("an existing file is backed up");

        assert_eq!(fs::read_to_string(&backup).unwrap(), before);
        let name = backup.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("settings.json.backup."), "got {name}");
        assert!(
            !name.contains(':'),
            "colons are illegal in filenames: {name}"
        );
    }

    #[test]
    fn status_tracks_install_and_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());

        assert!(status_claude(&path).unwrap().iter().all(|e| !e.installed));

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let installed = status_claude(&path).unwrap();
        assert_eq!(installed.len(), CLAUDE_HOOKS.len());
        assert!(installed.iter().all(|entry| entry.installed));
        assert!(
            installed[0]
                .command
                .as_deref()
                .is_some_and(|c| c.contains(MANAGED_MARKER))
        );

        uninstall_claude(&path).unwrap();
        assert!(status_claude(&path).unwrap().iter().all(|e| !e.installed));
    }

    #[test]
    fn status_of_a_missing_file_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let entries = status_claude(&dir.path().join("settings.json")).unwrap();
        assert_eq!(entries.len(), CLAUDE_HOOKS.len());
        assert!(entries.iter().all(|entry| !entry.installed));
    }

    #[test]
    fn malformed_settings_are_refused_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{ this is not json").unwrap();

        let error = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "a file we cannot parse is a file we must not rewrite"
        );
    }

    #[test]
    fn output_is_pretty_printed_with_sorted_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"zebra": 1, "alpha": 2}"#).unwrap();

        install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert!(text.ends_with("}\n"), "trailing newline");
        assert!(text.contains("\n  "), "pretty printed");
        assert!(
            text.find("\"alpha\"").unwrap() < text.find("\"zebra\"").unwrap(),
            "keys are sorted"
        );
    }

    #[test]
    fn a_hooks_key_of_the_wrong_type_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"hooks": "nope"}"#).unwrap();

        assert_eq!(
            install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn an_event_of_the_wrong_type_is_reported_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"hooks": {"Stop": "surprise"}}"#).unwrap();

        let report = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        let stop = report.entries.iter().find(|e| e.event == "Stop").unwrap();
        assert!(!stop.installed);
        assert!(stop.note.is_some());

        assert_eq!(read(&path)["hooks"]["Stop"], "surprise");
        // Every other event still went in.
        assert!(
            report
                .entries
                .iter()
                .filter(|e| e.event != "Stop")
                .all(|e| e.installed)
        );
    }

    // ------------------------------------------------------------- bridge

    /// A synthetic `atoll.exe` path, for the same reason as the hook one.
    fn fake_atoll_binary() -> PathBuf {
        PathBuf::from(r"C:\Tools\Atoll\bin\atoll.exe")
    }

    /// The status line a user might already have.
    fn user_statusline() -> Value {
        serde_json::json!({
            "type": "command",
            "command": "powershell -File C:\\Tools\\my-statusline.ps1",
            "padding": 0,
            // The key whose loss started all this: Claude Code reads it off
            // this same object, so a wrapper that drops it silently resets the
            // user's refresh rate to the default.
            "refreshInterval": 30,
        })
    }

    /// The hooks half of "only add, never change": Atoll appends its entries to
    /// whatever containers exist and takes back exactly those, leaving even the
    /// user's empty scaffolding as it found it.
    #[test]
    fn an_install_and_uninstall_round_trip_leaves_the_users_hooks_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let mine = serde_json::json!({
            "hooks": {
                // A group of the user's on an event Atoll also wants.
                "PreToolUse": [
                    {"matcher": "*", "hooks": [{"type": "command", "command": "their-tool"}]},
                    // A group they left empty. Not ours to tidy away.
                    {"matcher": "Bash", "hooks": []},
                ],
                // An event Atoll does not touch at all.
                "PreCompact": [{"hooks": [{"type": "command", "command": "compact-tool"}]}],
            },
            "statusLine": user_statusline(),
            "model": "opus",
        });
        let path = write_seed(dir.path(), &mine);
        let before = fs::read_to_string(&path).unwrap();

        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::IfEmpty(&fake_atoll_binary()),
        )
        .unwrap();

        // Their entry is still there, beside ours, in the same group.
        let during = read(&path);
        let group = &during["hooks"]["PreToolUse"][0];
        assert_eq!(group["matcher"], serde_json::json!("*"));
        assert_eq!(
            group["hooks"][0],
            serde_json::json!({"type": "command", "command": "their-tool"}),
            "the user's own hook must keep its place at the front"
        );
        assert!(
            group["hooks"][1]["command"]
                .as_str()
                .unwrap()
                .contains(MANAGED_MARKER)
        );

        uninstall_claude(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.trim_end(),
            before.trim_end(),
            "an install followed by an uninstall must give the file back unchanged"
        );
        // The one normalization Atoll does make, and the only difference the
        // round trip can leave: the file ends with a newline.
        assert!(after.ends_with('\n'));
        assert_eq!(after.trim_end_matches('\n'), before.trim_end_matches('\n'));
    }

    #[test]
    fn the_status_line_command_quotes_only_when_it_must() {
        // cmd's rules for a command line that both opens and closes with a quote
        // are conditional enough to be worth not triggering.
        let plain = PathBuf::from(r"C:\Users\me\AppData\Local\Atoll\bin\atoll.exe");
        assert_eq!(
            statusline_command(&plain),
            r"C:\Users\me\AppData\Local\Atoll\bin\atoll.exe statusline"
        );

        let spaced = PathBuf::from(r"C:\Program Files\Atoll\atoll.exe");
        let quoted = statusline_command(&spaced);
        assert!(quoted.starts_with('"'), "got {quoted}");
        assert!(quoted.ends_with(r#"atoll.exe" statusline"#), "got {quoted}");
    }

    /// An entry an older Atoll wrote in the exec form is still recognised as
    /// ours, so an install replaces it instead of mistaking it for the user's
    /// own status line and stashing a copy of our own broken entry.
    #[test]
    fn the_broken_exec_form_is_still_recognised_as_ours() {
        let legacy = serde_json::json!({
            "type": "command",
            "command": r"C:\tools\atoll.exe",
            "args": ["statusline"],
        });
        assert!(is_atoll_statusline(&legacy));
        assert!(is_atoll_statusline(&statusline_entry(
            &fake_atoll_binary(),
            None
        )));

        // Somebody else's line is still somebody else's, and a tool that merely
        // has "atoll" in its name is not ours either.
        assert!(!is_atoll_statusline(&user_statusline()));
        assert!(!is_atoll_statusline(&serde_json::json!({
            "type": "command",
            "command": "atoll-adjacent-tool",
        })));
    }

    /// Whatever `settings.json` names runs on every hook and every turn, so it
    /// must not be a path a `cargo build` rewrites underneath a live session.
    #[test]
    fn an_install_only_ever_names_the_stable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // Stand in for what `install_binaries` returns: paths under the data
        // directory rather than wherever the running binary happens to live.
        let stable = dir.path().join("Atoll").join("bin");
        let atoll = stable.join("atoll.exe");
        let hook = stable.join("atoll-hook.exe");

        install_claude(&path, &hook, BridgePolicy::IfEmpty(&atoll)).unwrap();

        let settings = read(&path);
        let rendered = settings.to_string();
        assert!(
            rendered.contains(&atoll.to_string_lossy().replace('\\', "\\\\")),
            "the status line must name the stable copy: {rendered}"
        );
        for spec in CLAUDE_HOOKS {
            let group = &settings["hooks"][spec.event][0]["hooks"][0];
            assert_eq!(
                group["command"].as_str().unwrap(),
                hook.to_string_lossy(),
                "{} must name the stable hook",
                spec.event
            );
        }
    }

    #[test]
    fn the_stable_directory_lives_beside_the_other_atoll_state() {
        // Not next to the executable, and not in the build tree: somewhere no
        // build ever writes.
        let dir = stable_bin_dir().unwrap();
        assert!(dir.ends_with("bin"));
        assert!(dir.parent().unwrap().ends_with("Atoll"));
    }

    #[test]
    fn the_bridge_is_installed_when_there_is_no_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &serde_json::json!({"model": "opus"}));

        // The default policy: an empty slot is fair game.
        let report = install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::IfEmpty(&fake_atoll_binary()),
        )
        .unwrap();
        assert_eq!(report.bridge, BridgeState::Installed);
        assert!(!report.bridge_left_alone);

        let settings = read(&path);
        // A shell string, and no `args`. Claude Code's schema for `statusLine`
        // is {type, command, padding?, refreshInterval?} and nothing else, so an
        // entry carrying `args` runs atoll.exe with no subcommand and prints
        // nothing — which is what emptied a user's status bar.
        assert_eq!(
            settings["statusLine"],
            serde_json::json!({
                "command": format!("{} statusline", fake_atoll_binary().to_string_lossy()),
                "type": "command",
            })
        );
        assert!(
            settings["statusLine"].get("args").is_none(),
            "statusLine takes no args array"
        );
        assert!(
            settings.get(ORIGINAL_STATUSLINE_KEY).is_none(),
            "nothing was displaced, so nothing is stashed"
        );
    }

    #[test]
    fn an_existing_status_line_is_stashed_and_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );

        let report = install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        assert_eq!(report.bridge, BridgeState::Wrapping);
        assert!(!report.bridge_left_alone);

        let settings = read(&path);
        assert!(is_atoll_statusline(&settings["statusLine"]));
        assert_eq!(settings[ORIGINAL_STATUSLINE_KEY], user_statusline());
        assert_eq!(
            original_statusline(&settings),
            Some(&user_statusline()),
            "the wrap target is what atoll statusline will delegate to"
        );

        // Every key of the user's that Atoll does not own rides onto the
        // wrapper, because Claude Code reads them off whatever is in the slot.
        assert_eq!(
            settings["statusLine"]["refreshInterval"],
            serde_json::json!(30)
        );
        assert_eq!(settings["statusLine"]["padding"], serde_json::json!(0));
    }

    /// The red line, and the regression this whole policy exists for: a status
    /// line the user already had is not Atoll's to replace, wrapper or not.
    #[test]
    fn an_existing_status_line_is_never_touched_without_an_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );
        let before = fs::read_to_string(&path).unwrap();

        let report = install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::IfEmpty(&fake_atoll_binary()),
        )
        .unwrap();

        assert!(
            report.bridge_left_alone,
            "the report has to say the slot was left alone"
        );
        assert_eq!(report.bridge, BridgeState::Foreign);

        let settings = read(&path);
        assert_eq!(
            settings["statusLine"],
            user_statusline(),
            "byte for byte, including refreshInterval"
        );
        assert!(
            settings.get(ORIGINAL_STATUSLINE_KEY).is_none(),
            "nothing was displaced, so nothing was stashed"
        );
        // The hooks half still went in — that is an append, not a replacement.
        assert!(before != fs::read_to_string(&path).unwrap());
        assert!(read(&path)["hooks"].is_object());
    }

    /// A refresh must not quietly undo the sibling keys it carried last time.
    #[test]
    fn refreshing_a_wrapper_keeps_the_users_sibling_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );
        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();

        let moved = PathBuf::from(r"D:\Elsewhere\atoll.exe");
        install_claude(&path, &fake_hook_binary(), BridgePolicy::Wrap(&moved)).unwrap();

        let settings = read(&path);
        assert_eq!(
            settings["statusLine"]["command"].as_str().unwrap(),
            statusline_command(&moved)
        );
        assert_eq!(
            settings["statusLine"]["refreshInterval"],
            serde_json::json!(30)
        );
        assert_eq!(settings[ORIGINAL_STATUSLINE_KEY], user_statusline());

        // And uninstalling puts the user's entry back exactly as it was.
        uninstall_claude(&path).unwrap();
        let restored = read(&path);
        assert_eq!(restored["statusLine"], user_statusline());
        assert!(restored.get(ORIGINAL_STATUSLINE_KEY).is_none());
    }

    #[test]
    fn reinstalling_over_our_own_bridge_never_eats_the_stash() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );

        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        let after_first = fs::read_to_string(&path).unwrap();

        let second = install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        assert!(!second.changed, "an unchanged install rewrites nothing");
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
        assert_eq!(read(&path)[ORIGINAL_STATUSLINE_KEY], user_statusline());
    }

    #[test]
    fn moving_the_binary_refreshes_the_bridge_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();

        let moved = PathBuf::from(r"D:\Elsewhere\atoll.exe");
        let report =
            install_claude(&path, &fake_hook_binary(), BridgePolicy::Wrap(&moved)).unwrap();
        assert!(report.changed);
        assert_eq!(
            read(&path)["statusLine"]["command"].as_str().unwrap(),
            statusline_command(&moved)
        );
        assert!(read(&path).get(ORIGINAL_STATUSLINE_KEY).is_none());
    }

    #[test]
    fn no_usage_bridge_leaves_the_status_line_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );

        let report = install_claude(&path, &fake_hook_binary(), BridgePolicy::Skip).unwrap();
        assert_eq!(report.bridge, BridgeState::Foreign);

        let settings = read(&path);
        assert_eq!(settings["statusLine"], user_statusline());
        assert!(settings.get(ORIGINAL_STATUSLINE_KEY).is_none());
        // The hooks still went in.
        assert!(report.entries.iter().all(|entry| entry.installed));
    }

    #[test]
    fn uninstall_restores_the_wrapped_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "model": "opus",
            "statusLine": user_statusline(),
        });
        let path = write_seed(dir.path(), &original);

        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        let report = uninstall_claude(&path).unwrap();

        assert_eq!(report.bridge, BridgeState::Foreign);
        assert_eq!(
            read(&path),
            original,
            "uninstall must leave the tree exactly as it found it"
        );
    }

    #[test]
    fn uninstall_removes_a_bridge_that_wrapped_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &serde_json::json!({"model": "opus"}));

        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        let report = uninstall_claude(&path).unwrap();

        assert_eq!(report.bridge, BridgeState::Absent);
        assert_eq!(read(&path), serde_json::json!({"model": "opus"}));
    }

    #[test]
    fn uninstall_will_not_remove_someone_elses_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );

        let report = uninstall_claude(&path).unwrap();
        assert!(!report.changed);
        assert_eq!(read(&path)["statusLine"], user_statusline());
    }

    #[test]
    fn uninstall_repairs_a_stash_left_without_a_status_line() {
        // Someone deleted `statusLine` by hand but left the stash behind.
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(
            dir.path(),
            &serde_json::json!({ORIGINAL_STATUSLINE_KEY: user_statusline()}),
        );

        uninstall_claude(&path).unwrap();
        let settings = read(&path);
        assert_eq!(settings["statusLine"], user_statusline());
        assert!(settings.get(ORIGINAL_STATUSLINE_KEY).is_none());
    }

    #[test]
    fn the_three_bridge_states_are_told_apart() {
        assert_eq!(
            statusline_state(&serde_json::json!({})),
            BridgeState::Absent
        );
        assert_eq!(
            statusline_state(&serde_json::json!({"statusLine": user_statusline()})),
            BridgeState::Foreign
        );
        assert_eq!(
            statusline_state(&serde_json::json!({
                "statusLine": statusline_entry(&fake_atoll_binary(), None),
            })),
            BridgeState::Installed
        );
        assert_eq!(
            statusline_state(&serde_json::json!({
                "statusLine": statusline_entry(&fake_atoll_binary(), None),
                ORIGINAL_STATUSLINE_KEY: user_statusline(),
            })),
            BridgeState::Wrapping
        );
    }

    #[test]
    fn only_our_own_status_line_is_recognized_as_ours() {
        assert!(is_atoll_statusline(&statusline_entry(
            &fake_atoll_binary(),
            None
        )));
        // The shell form, for a build that ignores `args`.
        assert!(is_atoll_statusline(&serde_json::json!({
            "type": "command",
            "command": r"C:\Tools\Atoll\bin\atoll.exe statusline",
        })));
        // Named atoll but not our subcommand: someone else's tool.
        assert!(!is_atoll_statusline(&serde_json::json!({
            "type": "command",
            "command": r"C:\Tools\Atoll\bin\other.exe",
        })));
        // A status line that merely lives in a directory called atoll.
        assert!(!is_atoll_statusline(&serde_json::json!({
            "type": "command",
            "command": r"C:\atoll\my-line.ps1",
            "args": ["--pretty"],
        })));
        assert!(!is_atoll_statusline(&user_statusline()));
    }

    #[test]
    fn status_bridge_reads_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_seed(dir.path(), &seeded_settings());
        assert_eq!(status_bridge(&path).unwrap(), BridgeState::Absent);

        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        assert_eq!(status_bridge(&path).unwrap(), BridgeState::Installed);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn read_original_statusline_degrades_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nothing.json");
        assert!(read_original_statusline(&missing).is_none());

        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{ not json").unwrap();
        assert!(read_original_statusline(&broken).is_none());

        let path = write_seed(
            dir.path(),
            &serde_json::json!({"statusLine": user_statusline()}),
        );
        install_claude(
            &path,
            &fake_hook_binary(),
            BridgePolicy::Wrap(&fake_atoll_binary()),
        )
        .unwrap();
        assert_eq!(read_original_statusline(&path), Some(user_statusline()));
    }

    #[test]
    fn compact_utc_formats_a_known_instant() {
        // 2026-08-23T11:42:33Z
        assert_eq!(compact_utc(1_787_485_353), "2026-08-23T114233Z");
        assert_eq!(compact_utc(0), "1970-01-01T000000Z");
        // A leap day, to exercise civil_from_days.
        assert_eq!(compact_utc(1_709_164_800), "2024-02-29T000000Z");
    }
}
