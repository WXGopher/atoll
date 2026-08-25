//! What the settings window shows and what its buttons do.
//!
//! The window itself is wired up in [`super`]; this is the part that can be
//! tested — reading the agent's configuration, summarising it in a line a person
//! can act on, and running the installer against an explicit path rather than
//! whatever `~` happens to be.

use std::io;
use std::path::Path;

use atoll_core::install::{self, BridgePolicy, CLAUDE_HOOKS};

/// How Claude Code is wired up right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookStatus {
    pub installed: usize,
    pub total: usize,
}

impl HookStatus {
    /// Whether Atoll is installed at all, which is what the button's label
    /// switches on.
    pub fn is_installed(&self) -> bool {
        self.installed > 0
    }

    /// Whether every hook Atoll wants is present. A partial install is still
    /// "installed" — the button has to offer to remove it — but it is worth
    /// saying out loud.
    pub fn is_complete(&self) -> bool {
        self.installed == self.total
    }
}

pub fn read_status(settings_path: &Path) -> io::Result<HookStatus> {
    let entries = install::status_claude(settings_path)?;
    Ok(HookStatus {
        installed: entries.iter().filter(|entry| entry.installed).count(),
        total: CLAUDE_HOOKS.len(),
    })
}

/// The line under "Claude Code" in the settings window.
///
/// Hooks and nothing else. The status-line bridge is a terminal affair the CLI
/// still knows how to wire; Atoll's own windows are the tray's, and they do
/// not mention it.
pub fn describe(status: &HookStatus) -> String {
    if !status.is_installed() {
        "Hooks not installed".to_string()
    } else if status.is_complete() {
        format!("{} hooks installed", status.total)
    } else {
        format!("{} of {} hooks installed", status.installed, status.total)
    }
}

/// What to say when the configuration cannot be read at all.
pub fn describe_error(error: &io::Error) -> String {
    format!("Could not read settings.json: {error}")
}

/// Install Atoll's hooks, returning the sentence to show underneath.
///
/// Hooks only: `statusLine` is never touched from here. The terminal-side
/// usage bridge still exists for whoever wants it, but it belongs to the CLI
/// (`atoll setup install claude`), not to the tray's settings window.
pub fn install(settings_path: &Path) -> io::Result<String> {
    // A copy of Atoll that no `cargo build` can pull out from under a live
    // session; see `install::install_binaries`.
    let stable = install::install_binaries()?;

    let report = install::install_claude(settings_path, &stable.hook, BridgePolicy::Skip)?;

    Ok(if report.changed {
        match &report.backup_path {
            Some(backup) => format!(
                "Installed. Your previous settings.json was backed up to {}.",
                backup.display()
            ),
            None => "Installed.".to_string(),
        }
    } else {
        "Already installed; nothing was written.".to_string()
    })
}

/// Remove Atoll's hooks, leaving the user's own alone.
pub fn uninstall(settings_path: &Path) -> io::Result<String> {
    let report = install::uninstall_claude(settings_path)?;
    Ok(if report.changed {
        "Removed Atoll's hooks. Your own hooks were left alone.".to_string()
    } else {
        "No Atoll hooks were installed; nothing was written.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never the real one: these tests write settings files.
    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_virgin_settings_file_reads_as_not_installed() {
        let dir = scratch();
        let settings = dir.path().join("settings.json");
        let status = read_status(&settings).unwrap();

        assert_eq!(status.installed, 0);
        assert_eq!(status.total, CLAUDE_HOOKS.len());
        assert!(!status.is_installed());
        assert_eq!(describe(&status), "Hooks not installed");
    }

    #[test]
    fn installing_and_removing_move_the_summary() {
        let dir = scratch();
        let settings = dir.path().join("settings.json");
        let hook = dir.path().join("atoll-hook.exe");
        let atoll = dir.path().join("atoll.exe");
        std::fs::write(&hook, b"").unwrap();

        install::install_claude(&settings, &hook, BridgePolicy::IfEmpty(atoll.as_path())).unwrap();
        let status = read_status(&settings).unwrap();
        assert!(status.is_installed() && status.is_complete());
        assert_eq!(
            describe(&status),
            format!("{} hooks installed", status.total)
        );

        let message = uninstall(&settings).unwrap();
        assert!(message.starts_with("Removed"), "got {message}");
        assert!(!read_status(&settings).unwrap().is_installed());

        // And a second removal says so rather than pretending it did something.
        assert!(uninstall(&settings).unwrap().starts_with("No Atoll hooks"));
    }

    #[test]
    fn a_partial_install_is_reported_as_partial() {
        let dir = scratch();
        let settings = dir.path().join("settings.json");
        let hook = dir.path().join("atoll-hook.exe");
        std::fs::write(&hook, b"").unwrap();
        install::install_claude(&settings, &hook, BridgePolicy::Skip).unwrap();

        // Rip one event back out behind Atoll's back, the way a user editing
        // settings.json by hand would.
        let raw = std::fs::read_to_string(&settings).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let removed = CLAUDE_HOOKS[0].event;
        value["hooks"]
            .as_object_mut()
            .unwrap()
            .remove(removed)
            .expect("the event was installed");
        std::fs::write(&settings, value.to_string()).unwrap();

        let status = read_status(&settings).unwrap();
        assert!(status.is_installed());
        assert!(!status.is_complete());
        assert!(
            describe(&status).contains(&format!("{} of {}", status.total - 1, status.total)),
            "got {}",
            describe(&status)
        );
    }

    /// The status line is a terminal affair the tray's window says nothing
    /// about: somebody else's `statusLine` must neither change the summary nor
    /// get in the way of reading the hooks.
    #[test]
    fn somebody_elses_status_line_is_not_the_windows_business() {
        let dir = scratch();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"statusLine":{"type":"command","command":"their-own-tool"}}"#,
        )
        .unwrap();

        let status = read_status(&settings).unwrap();
        assert_eq!(describe(&status), "Hooks not installed");
    }

    #[test]
    fn installing_without_the_hook_binary_refuses_rather_than_writing_a_dead_path() {
        let dir = scratch();
        let settings = dir.path().join("settings.json");
        // `install` looks for the hook beside the running test binary, where
        // there is none, so this exercises the guard.
        if install::hook_binary_path().is_ok_and(|path| !path.exists()) {
            let error = install(&settings).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert!(
                !settings.exists(),
                "a refused install must not have written anything"
            );
        }
    }
}
