//! Codex lifecycle hooks. User configuration is backed up and edited additively.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item};

use super::EntryStatus;

const MARKER: &str = "Managed by Atoll";
const RECORD: &str = "atoll-install.json";
const FEATURE_COMMENT: &str = " # Managed by Atoll";

// Tool events also release approvals answered in another client and keep long
// turns alive. Interrupt is distinct from a successful completion.
pub const CODEX_HOOKS: &[(&str, u64)] = &[
    ("SessionStart", 5),
    ("UserPromptSubmit", 5),
    ("PreToolUse", 5),
    ("PermissionRequest", 3_600),
    ("PostToolUse", 5),
    ("Stop", 5),
    ("Interrupt", 3),
    ("SessionEnd", 3),
];

#[derive(Debug)]
pub struct CodexReport {
    pub config_path: PathBuf,
    pub hooks_path: PathBuf,
    pub changed: bool,
    pub enabled: bool,
    pub entries: Vec<EntryStatus>,
    pub backups: Vec<PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    hooks_key_existed: bool,
    #[serde(default)]
    empty_events: Vec<String>,
    feature: Option<FeatureChange>,
}

#[derive(Serialize, Deserialize)]
struct FeatureChange {
    before: Option<String>,
    after: String,
    had_features: bool,
}

pub fn codex_home() -> io::Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| super::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "could not determine Codex home"))
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_config(home: &Path) -> io::Result<DocumentMut> {
    let bytes = read_optional(&home.join("config.toml"))?.unwrap_or_default();
    let text = std::str::from_utf8(&bytes)
        .map_err(invalid)?
        .trim_start_matches('\u{feff}');
    let doc = text.parse::<DocumentMut>().map_err(invalid)?;
    if let Some(features) = doc.get("features") {
        if !features.is_table_like() {
            return Err(invalid("Codex features must be a table"));
        }
        if features
            .get("hooks")
            .is_some_and(|hooks| hooks.as_bool().is_none())
        {
            return Err(invalid("Codex features.hooks must be a boolean"));
        }
    }
    Ok(doc)
}

fn read_hooks(home: &Path) -> io::Result<Value> {
    let Some(bytes) = read_optional(&home.join("hooks.json"))? else {
        return Ok(json!({}));
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(invalid)?
        .trim_start_matches('\u{feff}');
    let hooks: Value = serde_json::from_str(text).map_err(invalid)?;
    if !hooks.is_object() || hooks.get("hooks").is_some_and(|value| !value.is_object()) {
        return Err(invalid(
            "Codex hooks.json and its hooks field must be objects",
        ));
    }
    Ok(hooks)
}

fn managed(entry: &Value) -> bool {
    entry.get("statusMessage").and_then(Value::as_str) == Some(MARKER)
}

fn strip(hooks: &mut Value) {
    let Some(events) = hooks.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    events.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        let was_empty = groups.is_empty();
        groups.retain_mut(|group| {
            let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let was_empty = entries.is_empty();
            entries.retain(|entry| !managed(entry));
            was_empty || !entries.is_empty()
        });
        was_empty || !groups.is_empty()
    });
}

/// An encoded script works under either cmd or PowerShell, including paths
/// with spaces, apostrophes, ampersands and dollar signs. stdin stays inherited.
fn command(binary: &Path) -> String {
    let path = binary.to_string_lossy().replace('\'', "''");
    let script = format!("& '{path}' --source codex");
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}")
}

fn entries(hooks: &Value, expected: Option<&str>) -> Vec<EntryStatus> {
    CODEX_HOOKS
        .iter()
        .map(|(event, timeout)| {
            let found = hooks["hooks"][*event]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                .find(|entry| {
                    managed(entry)
                        && entry["type"] == "command"
                        && entry["timeout"].as_u64() == Some(*timeout)
                        && entry["command"]
                            .as_str()
                            .is_some_and(|cmd| expected.is_none_or(|expected| cmd == expected))
                });
            EntryStatus {
                event: (*event).to_string(),
                installed: found.is_some(),
                command: found
                    .and_then(|entry| entry["command"].as_str())
                    .map(str::to_string),
                note: None,
            }
        })
        .collect()
}

pub fn status_codex(home: &Path) -> io::Result<CodexReport> {
    let config = read_config(home)?;
    let hooks = read_hooks(home)?;
    Ok(CodexReport {
        config_path: home.join("config.toml"),
        hooks_path: home.join("hooks.json"),
        changed: false,
        enabled: config
            .get("features")
            .and_then(|v| v.get("hooks"))
            .and_then(Item::as_bool)
            .unwrap_or(true),
        entries: entries(&hooks, None),
        backups: Vec::new(),
    })
}

pub fn install_codex(home: &Path, binary: &Path) -> io::Result<CodexReport> {
    if !binary.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "atoll-hook binary not found",
        ));
    }
    let mut config = read_config(home)?;
    let old_config = config.to_string();
    let mut hooks = read_hooks(home)?;
    let old_hooks = hooks.clone();
    let record_path = home.join(RECORD);
    let mut record: Record = match read_optional(&record_path)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(invalid)?,
        None => Record {
            hooks_key_existed: hooks.get("hooks").is_some(),
            empty_events: hooks["hooks"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, value)| value.as_array().is_some_and(Vec::is_empty))
                .map(|(name, _)| name.clone())
                .collect(),
            ..Default::default()
        },
    };
    let had_features = config.contains_key("features");
    let before = config.get("features").and_then(|v| v.get("hooks")).cloned();
    if before.as_ref().and_then(Item::as_bool) != Some(true) {
        if !had_features {
            config["features"] = Item::Table(toml_edit::Table::new());
        }
        let mut value = toml_edit::Value::from(true);
        if config["features"].is_table() {
            value.decor_mut().set_suffix(FEATURE_COMMENT);
        }
        let after = Item::Value(value);
        record.feature = Some(FeatureChange {
            before: before.map(|value| value.to_string()),
            after: after.to_string(),
            had_features,
        });
        config["features"]["hooks"] = after;
        // Match the exact representation read back from disk, including the
        // whitespace TOML inserts around a value in a table or inline table.
        let rendered = config.to_string().parse::<DocumentMut>().map_err(invalid)?;
        record.feature.as_mut().expect("recorded feature").after =
            rendered["features"]["hooks"].to_string();
    }
    strip(&mut hooks);
    if hooks.get("hooks").is_none() {
        hooks["hooks"] = json!({});
    }
    let cmd = command(binary);
    for (event, timeout) in CODEX_HOOKS {
        let events = hooks["hooks"]
            .as_object_mut()
            .expect("validated hooks object");
        let groups = events
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                invalid(format!(
                    "Codex {event} hooks must be an array; nothing written"
                ))
            })?;
        groups.push(json!({"hooks": [{"type": "command", "command": cmd, "timeout": timeout, "statusMessage": MARKER}]}));
    }
    let mut writes = vec![(record_path, Some(json_bytes(&record)?))];
    if hooks != old_hooks {
        writes.push((home.join("hooks.json"), Some(json_bytes(&hooks)?)));
    }
    if config.to_string() != old_config {
        writes.push((
            home.join("config.toml"),
            Some(config.to_string().into_bytes()),
        ));
    }
    let (changed, backups) = commit_files(writes)?;
    Ok(CodexReport {
        config_path: home.join("config.toml"),
        hooks_path: home.join("hooks.json"),
        changed,
        enabled: true,
        entries: entries(&hooks, Some(&cmd)),
        backups,
    })
}

pub fn uninstall_codex(home: &Path) -> io::Result<CodexReport> {
    let mut config = read_config(home)?;
    let old_config = config.to_string();
    let mut hooks = read_hooks(home)?;
    let old_hooks = hooks.clone();
    let record_path = home.join(RECORD);
    let record = read_optional(&record_path)?
        .map(|bytes| serde_json::from_slice::<Record>(&bytes).map_err(invalid))
        .transpose()?;
    strip(&mut hooks);
    if let Some(record) = &record {
        for event in &record.empty_events {
            let removed_managed = old_hooks["hooks"][event]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                .any(managed);
            if removed_managed && hooks["hooks"].get(event).is_none() {
                hooks["hooks"][event] = json!([]);
            }
        }
        if !record.hooks_key_existed && hooks["hooks"].as_object().is_some_and(|v| v.is_empty()) {
            hooks
                .as_object_mut()
                .expect("validated object")
                .remove("hooks");
        }
        if let Some(feature) = &record.feature
            && config
                .get("features")
                .and_then(|v| v.get("hooks"))
                .is_some_and(|v| v.to_string() == feature.after)
        {
            let features = config["features"]
                .as_table_like_mut()
                .expect("validated features");
            if let Some(before) = &feature.before {
                let restored = format!("hooks={before}\n")
                    .parse::<DocumentMut>()
                    .map_err(invalid)?;
                *features.get_mut("hooks").expect("managed hook setting") =
                    restored["hooks"].clone();
            } else {
                features.remove("hooks");
            }
            if !feature.had_features && features.is_empty() {
                config.remove("features");
            }
        }
    }
    let mut writes = Vec::new();
    if hooks != old_hooks {
        writes.push((home.join("hooks.json"), Some(json_bytes(&hooks)?)));
    }
    if config.to_string() != old_config {
        writes.push((
            home.join("config.toml"),
            Some(config.to_string().into_bytes()),
        ));
    }
    if record.is_some() {
        writes.push((record_path, None));
    }
    let (changed, backups) = commit_files(writes)?;
    let mut report = status_codex(home)?;
    report.changed = changed;
    report.backups = backups;
    Ok(report)
}

fn json_bytes(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(invalid)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Prepare every backup before changing any user file. On failure restore all
/// earlier writes, so a half-installed hook never loses its rollback record.
pub(super) fn commit_files(
    writes: Vec<(PathBuf, Option<Vec<u8>>)>,
) -> io::Result<(bool, Vec<PathBuf>)> {
    let mut changes = Vec::new();
    let mut backups = Vec::new();
    for (path, bytes) in writes {
        let original = read_optional(&path)?;
        if bytes == original {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if original.is_some() {
            let backup = super::backup_path_for(&path);
            fs::copy(&path, &backup)?;
            backups.push(backup);
        }
        changes.push((path, bytes, original));
    }
    for (index, (path, bytes, _)) in changes.iter().enumerate() {
        if let Err(error) = replace(path, bytes.as_deref()) {
            let mut rollback_errors = Vec::new();
            for (path, _, original) in changes[..=index].iter().rev() {
                if let Err(rollback) = replace(path, original.as_deref()) {
                    rollback_errors.push(rollback.to_string());
                }
            }
            return Err(io::Error::other(format!(
                "{error}; rollback errors: {rollback_errors:?}"
            )));
        }
    }
    Ok((!changes.is_empty(), backups))
}

fn replace(path: &Path, bytes: Option<&[u8]>) -> io::Result<()> {
    if let Some(bytes) = bytes {
        let temporary = path.with_extension(format!("atoll-tmp-{}", std::process::id()));
        fs::write(&temporary, bytes)?;
        let result = fs::rename(&temporary, path);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    } else {
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("atoll-hook.exe");
        fs::write(&hook, b"test executable").unwrap();
        (dir, hook)
    }

    #[test]
    fn install_is_idempotent_and_uninstall_preserves_user_hooks_and_comments() {
        let (dir, hook) = scratch();
        let home = dir.path();
        let original_config = "# personal settings\nmodel = 'local-model'\n\n[features]\n# keep this\nhooks = false # disabled previously\nother = true\n\n[hooks]\n# inline hooks remain untouched\n";
        fs::write(home.join("config.toml"), original_config).unwrap();
        let original_hooks = json!({"custom": {"keep": true}, "hooks": {
            "PermissionRequest": [{"matcher":"Bash", "hooks":[{"type":"command", "command":"my-approval-check"}]}],
            "Stop": [], "ForeignEvent": [{"hooks":[]}]}});
        fs::write(
            home.join("hooks.json"),
            json_bytes(&original_hooks).unwrap(),
        )
        .unwrap();
        let installed = install_codex(home, &hook).unwrap();
        assert!(installed.changed && installed.enabled);
        assert!(installed.entries.iter().all(|entry| entry.installed));
        assert_eq!(
            fs::read_to_string(&installed.backups[0]).unwrap(),
            String::from_utf8(json_bytes(&original_hooks).unwrap()).unwrap()
        );
        assert!(!install_codex(home, &hook).unwrap().changed);
        let removed = uninstall_codex(home).unwrap();
        assert!(removed.changed);
        assert!(!removed.enabled);
        assert_eq!(read_hooks(home).unwrap(), original_hooks);
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            original_config
        );
        assert!(!home.join(RECORD).exists());
        assert!(!uninstall_codex(home).unwrap().changed);
    }

    #[test]
    fn invalid_existing_event_writes_nothing() {
        let (dir, hook) = scratch();
        let home = dir.path();
        let bytes = b"{\"hooks\":{\"Stop\":false}}";
        fs::write(home.join("hooks.json"), bytes).unwrap();
        assert!(install_codex(home, &hook).is_err());
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), bytes);
        assert!(!home.join("config.toml").exists());
        assert!(!home.join(RECORD).exists());
    }

    #[test]
    fn later_user_feature_edits_are_not_undone() {
        let (dir, hook) = scratch();
        let home = dir.path();
        install_codex(home, &hook).unwrap();
        fs::write(
            home.join("config.toml"),
            "[features]\nhooks = true # user chose this\nother = true\n",
        )
        .unwrap();
        uninstall_codex(home).unwrap();
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            "[features]\nhooks = true # user chose this\nother = true\n"
        );
    }

    #[test]
    fn an_empty_install_and_uninstall_restore_default_feature_state() {
        let (dir, hook) = scratch();
        let home = dir.path();
        assert!(install_codex(home, &hook).unwrap().changed);
        assert!(!install_codex(home, &hook).unwrap().changed);
        uninstall_codex(home).unwrap();
        assert!(read_config(home).unwrap().is_empty());
        assert_eq!(read_hooks(home).unwrap(), json!({}));
    }

    #[test]
    fn existing_inline_features_keep_their_other_fields() {
        let (dir, hook) = scratch();
        let home = dir.path();
        fs::write(
            home.join("config.toml"),
            "features = {hooks = false, other = true}\n",
        )
        .unwrap();
        install_codex(home, &hook).unwrap();
        assert!(status_codex(home).unwrap().enabled);
        uninstall_codex(home).unwrap();
        let config = read_config(home).unwrap();
        assert_eq!(config["features"]["hooks"].as_bool(), Some(false));
        assert_eq!(config["features"]["other"].as_bool(), Some(true));
    }
}
