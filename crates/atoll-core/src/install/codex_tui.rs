//! Codex CLI's native footer configuration, independent of Atoll's hooks.
//!
//! The catalog and preview defaults were checked against `/statusline` in
//! codex-cli 0.154.0. Reset removes the override: Codex owns its actual defaults,
//! including changes in future versions. Sample values are not live telemetry.

use std::{fs, io, path::Path};

use toml_edit::{Array, DocumentMut, Item, Table, value};

pub const CATALOG_VERSION: &str = "0.154.0";
pub const DEFAULT_ITEMS: &[&str] = &["model-with-reasoning", "current-dir", "thread-name"];

pub struct Component {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub sample: &'static str,
}

macro_rules! components {
    ($(($id:literal, $label:literal, $description:literal, $sample:literal)),* $(,)?) => {
        pub const COMPONENTS: &[Component] = &[
            $(Component { id: $id, label: $label, description: $description, sample: $sample }),*
        ];
    };
}

components![
    (
        "model-with-reasoning",
        "Model + reasoning",
        "Model and thinking effort",
        "gpt-6-astra xhigh"
    ),
    (
        "current-dir",
        "Current directory",
        "Working directory",
        "~/atoll"
    ),
    (
        "thread-name",
        "Thread name",
        "Shown when the session is named",
        "Customize status bar"
    ),
    (
        "model",
        "Model",
        "Model name without reasoning",
        "gpt-6-astra"
    ),
    ("reasoning", "Reasoning", "Current thinking effort", "xhigh"),
    (
        "project-name",
        "Project name",
        "Shown when available",
        "atoll"
    ),
    ("hostname", "Hostname", "Current machine name", "my-pc"),
    (
        "git-branch",
        "Git branch",
        "Current repository branch",
        "main"
    ),
    (
        "pull-request-number",
        "Pull request",
        "Open PR for the current branch",
        "PR #123"
    ),
    (
        "branch-changes",
        "Branch changes",
        "Committed diff from the default branch",
        "+12 -3"
    ),
    (
        "run-state",
        "Run state",
        "Ready, working or thinking",
        "Ready"
    ),
    (
        "permissions",
        "Permissions",
        "Permission profile or sandbox mode",
        "Read-only"
    ),
    (
        "approval-mode",
        "Approval mode",
        "Command approval behavior",
        "Ask for approval"
    ),
    (
        "context-remaining",
        "Context remaining",
        "Percentage of context left",
        "Context 82% left"
    ),
    (
        "context-used",
        "Context used",
        "Percentage of context used",
        "Context 18% used"
    ),
    (
        "five-hour-limit",
        "5-hour / primary limit",
        "Remaining primary usage allowance",
        "5h 76%"
    ),
    (
        "weekly-limit",
        "Weekly limit",
        "Remaining weekly usage allowance",
        "weekly 64%"
    ),
    (
        "codex-version",
        "Codex version",
        "CLI application version",
        "0.154.0"
    ),
    (
        "context-window-size",
        "Context window size",
        "Available context in tokens",
        "272K window"
    ),
    (
        "used-tokens",
        "Tokens used",
        "Session tokens, omitted when zero",
        "12.4K used"
    ),
    (
        "total-input-tokens",
        "Input tokens",
        "Total session input tokens",
        "10K in"
    ),
    (
        "total-output-tokens",
        "Output tokens",
        "Total session output tokens",
        "2.4K out"
    ),
    (
        "thread-credits",
        "Thread credits",
        "Enterprise only, when available",
        "5.2 credits"
    ),
    (
        "estimated-thread-cost",
        "Thread cost",
        "Enterprise estimate, when available",
        "~$1.82"
    ),
    (
        "thread-id",
        "Thread ID",
        "Identifier of the started session",
        "550e8400-e29b-41d4"
    ),
    (
        "fast-mode",
        "Fast mode",
        "Shown while Fast mode is active",
        "Fast on"
    ),
    (
        "raw-output",
        "Raw output",
        "Raw scrollback mode indicator",
        "raw output"
    ),
    (
        "thread-title",
        "Thread title",
        "Title, or ID when unnamed",
        "Customize status bar"
    ),
    (
        "workspace-headline",
        "Workspace headline",
        "Enterprise notification, when available",
        "Workspace headline"
    ),
    (
        "task-progress",
        "Task progress",
        "Completed and total plan steps",
        "Tasks 2/4"
    ),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    /// None follows Codex defaults; Some([]) intentionally hides the footer.
    pub items: Option<Vec<String>>,
}

impl StatusLine {
    pub fn visible_items(&self) -> Vec<String> {
        self.items.clone().unwrap_or_else(default_items)
    }
}

pub fn default_items() -> Vec<String> {
    DEFAULT_ITEMS.iter().map(|id| (*id).to_owned()).collect()
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn document(path: &Path) -> io::Result<(DocumentMut, bool)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let bom = text.starts_with('\u{feff}');
    let doc = text
        .trim_start_matches('\u{feff}')
        .parse()
        .map_err(invalid)?;
    Ok((doc, bom))
}

fn status(doc: &DocumentMut) -> io::Result<StatusLine> {
    let Some(tui) = doc.get("tui") else {
        return Ok(StatusLine { items: None });
    };
    let tui = tui
        .as_table_like()
        .ok_or_else(|| invalid("tui must be a table"))?;
    let items = tui
        .get("status_line")
        .map(|item| {
            let array = item
                .as_array()
                .ok_or_else(|| invalid("tui.status_line must be a list of strings"))?;
            array
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("tui.status_line contains a non-string item"))
                })
                .collect::<io::Result<Vec<_>>>()
        })
        .transpose()?;
    Ok(StatusLine { items })
}

pub fn read(path: &Path) -> io::Result<StatusLine> {
    status(&document(path)?.0)
}

/// Re-read on every write to preserve unrelated edits made while the editor was
/// open. If the footer itself changed externally, require a reload before saving.
/// Existing files are backed up before the shared atomic replacement runs.
pub fn save(
    path: &Path,
    expected: &StatusLine,
    items: Option<&[String]>,
) -> io::Result<StatusLine> {
    let (mut doc, bom) = document(path)?;
    let current = status(&doc)?;
    if &current != expected {
        return Err(io::Error::other(
            "The Codex status bar changed outside Atoll. Close and reopen this editor to reload it.",
        ));
    }
    let updated = StatusLine {
        items: items.map(<[String]>::to_vec),
    };
    if updated == current {
        return Ok(current);
    }
    match items {
        Some(items) => {
            if !doc.contains_key("tui") {
                doc["tui"] = Item::Table(Table::new());
            }
            let mut array = Array::new();
            for id in items {
                array.push(id.as_str());
            }
            let tui = doc["tui"].as_table_like_mut().expect("validated table");
            // Preserve a comment attached to the value being edited as well.
            let mut replacement = value(array);
            if let Some(decor) = tui
                .get("status_line")
                .and_then(Item::as_value)
                .map(|v| v.decor().clone())
            {
                *replacement.as_value_mut().expect("array value").decor_mut() = decor;
            }
            tui.insert("status_line", replacement);
        }
        None => {
            if let Some(tui) = doc.get_mut("tui").and_then(Item::as_table_like_mut) {
                tui.remove("status_line");
            }
        }
    }
    let mut text = doc.to_string();
    if bom {
        text.insert(0, '\u{feff}');
    }
    super::codex::commit_files(vec![(path.to_owned(), Some(text.into_bytes()))])?;
    Ok(updated)
}

pub fn preview(items: &[String]) -> String {
    items
        .iter()
        .map(|id| {
            COMPONENTS
                .iter()
                .find(|component| component.id == id)
                .map_or_else(
                    || format!("[{id}]"),
                    |component| component.sample.to_owned(),
                )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hidden_and_custom_are_distinct_and_reset_follows_codex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let initial = read(&path).unwrap();
        assert_eq!(initial.visible_items(), default_items());
        save(&path, &initial, None).unwrap();
        assert!(
            !path.exists(),
            "resetting untouched defaults must not create a file"
        );
        let hidden = save(&path, &initial, Some(&[])).unwrap();
        assert_eq!(read(&path).unwrap().items, Some(vec![]));
        assert!(hidden.visible_items().is_empty());
        let custom = vec!["git-branch".into(), "model".into()];
        let applied = save(&path, &hidden, Some(&custom)).unwrap();
        assert_eq!(read(&path).unwrap().visible_items(), custom);
        save(&path, &applied, None).unwrap();
        assert_eq!(read(&path).unwrap(), initial);
        assert!(!fs::read_to_string(&path).unwrap().contains("status_line"));
    }

    #[test]
    fn saves_preserve_unrelated_concurrent_edits_comments_bom_inline_tui_and_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "\u{feff}# personal settings\ntui = { status_line = [\"model\"], theme = \"nord\" } # keep\n[features]\nhooks = true\n[profiles.work.tui]\nstatus_line = [\"git-branch\"]\n";
        fs::write(&path, original).unwrap();
        let initial = read(&path).unwrap();
        let concurrent = original.replace("hooks = true", "hooks = false");
        fs::write(&path, &concurrent).unwrap();
        let updated = save(&path, &initial, Some(&["current-dir".into()])).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        for fragment in [
            "\u{feff}# personal settings",
            "theme = \"nord\"",
            "# keep",
            "hooks = false",
            "[profiles.work.tui]\nstatus_line = [\"git-branch\"]",
        ] {
            assert!(saved.contains(fragment), "missing {fragment}: {saved}");
        }
        let backup = fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name().to_string_lossy().contains(".backup."))
            .unwrap();
        assert_eq!(fs::read_to_string(backup.path()).unwrap(), concurrent);
        save(&path, &updated, None).unwrap();
        let reset = fs::read_to_string(&path).unwrap();
        assert!(reset.contains("theme = \"nord\""));
        assert!(reset.contains("[profiles.work.tui]\nstatus_line = [\"git-branch\"]"));
    }

    #[test]
    fn conflicts_and_invalid_config_never_overwrite_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let initial = StatusLine { items: None };
        for text in [
            "[tui]\nstatus_line = [\"git-branch\"]\n",
            "[tui]\nstatus_line = [123]\n",
            "tui = true\n",
            "[broken",
        ] {
            fs::write(&path, text).unwrap();
            assert!(save(&path, &initial, Some(&default_items())).is_err());
            assert!(save(&path, &initial, None).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn unknown_items_order_and_value_comments_survive_editing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[tui]\nstatus_line = [\"future-component\", \"model\"] # custom footer\n",
        )
        .unwrap();
        let initial = read(&path).unwrap();
        let mut items = initial.visible_items();
        items.push("git-branch".into());
        save(&path, &initial, Some(&items)).unwrap();
        assert_eq!(read(&path).unwrap().visible_items(), items);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("# custom footer")
        );
        assert_eq!(preview(&items), "[future-component] · gpt-6-astra · main");
        let count = fs::read_dir(dir.path()).unwrap().count();
        save(&path, &read(&path).unwrap(), Some(&items)).unwrap();
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            count,
            "idempotent saves do not create backups"
        );
    }
}
