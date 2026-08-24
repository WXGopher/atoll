//! `atoll setup …`: the command-line face of [`atoll_core::install`].
//!
//! The settings window drives the same functions; this stays because a hook
//! that will not install is much easier to diagnose from a terminal.

use std::io;

use atoll_core::{install, transcript};
use clap::{Subcommand, ValueEnum};

use crate::out::outln;
use crate::util::home_dir;

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Add Atoll's hooks to the agent's configuration.
    Install {
        agent: Agent,
        /// Deprecated and ignored: `statusLine` is left alone by default now.
        #[arg(long, hide = true)]
        no_usage_bridge: bool,
        /// LEGACY: also install Atoll as your status line, wrapping whatever is
        /// there so it still runs.
        ///
        /// You almost certainly do not want this. Atoll reads Claude Code's
        /// usage from its OAuth endpoint, so the status line buys nothing — and
        /// Claude Code latches a status line command that fails repeatedly,
        /// leaving it blank for the rest of the session.
        #[arg(long, conflicts_with = "no_usage_bridge")]
        wrap_status_line: bool,
    },
    /// Remove Atoll's hooks from the agent's configuration.
    Uninstall { agent: Agent },
    /// Report whether Atoll's hooks are currently installed.
    Status { agent: Agent },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

pub fn run(action: Action) -> io::Result<()> {
    match action {
        Action::Install {
            agent: Agent::Claude,
            no_usage_bridge,
            wrap_status_line,
        } => {
            let settings = install::claude_settings_path()?;

            // Everything settings.json names has to keep working while Atoll is
            // rebuilt, so the install runs from a copy of itself that no build
            // ever touches.
            let stable = install::install_binaries()?;
            if stable.copied {
                outln!("binaries : copied to {}", stable.atoll.display());
            }

            // `statusLine` is left alone unless explicitly asked for: Atoll
            // reads usage from the OAuth endpoint and has no need of the slot.
            let _ = no_usage_bridge;
            let policy = if wrap_status_line {
                install::BridgePolicy::Wrap(stable.atoll.as_path())
            } else {
                install::BridgePolicy::Skip
            };
            let report = install::install_claude(&settings, &stable.hook, policy)?;
            outln!("settings : {}", report.settings_path.display());
            outln!("hook     : {}", stable.hook.display());
            if report.bridge.is_atoll() {
                outln!("statusline: {}", install::statusline_command(&stable.atoll));
            }
            if let Some(backup) = &report.backup_path {
                outln!("backup   : {}", backup.display());
            }
            outln!(
                "{}",
                if report.changed {
                    "installed."
                } else {
                    "already up to date; nothing written."
                }
            );
            print_entries(&report.entries);
            outln!("  usage bridge: {}", report.bridge.as_str());
            if report.bridge_left_alone {
                outln!(
                    "  your status line was left exactly as it is. Atoll reads Claude Code's
                       rate limits from the status line payload, so without the bridge the
                       readout shows no Claude usage. To wrap yours — it keeps running, and its
                       output still reaches you — rerun with --wrap-status-line."
                );
            }
            Ok(())
        }
        Action::Uninstall {
            agent: Agent::Claude,
        } => {
            let settings = install::claude_settings_path()?;
            let report = install::uninstall_claude(&settings)?;
            outln!("settings : {}", report.settings_path.display());
            if let Some(backup) = &report.backup_path {
                outln!("backup   : {}", backup.display());
            }
            outln!(
                "{}",
                if report.changed {
                    "removed Atoll's hooks; your own hooks were left alone."
                } else {
                    "no Atoll hooks were installed; nothing written."
                }
            );
            outln!("  usage bridge: {}", report.bridge.as_str());
            Ok(())
        }
        Action::Status {
            agent: Agent::Claude,
        } => {
            let settings = install::claude_settings_path()?;
            outln!("settings : {}", settings.display());
            outln!("hook     : {}", install::stable_bin_dir()?.display());
            print_entries(&install::status_claude(&settings)?);
            outln!(
                "  usage bridge: {}",
                install::status_bridge(&settings)?.as_str()
            );

            // The transcript scan is the other half of "is this working?": it
            // says whether Atoll can see the sessions it is meant to track.
            if let Some(home) = home_dir() {
                let options = transcript::ScanOptions::new(home);
                match transcript::scan_claude(&options) {
                    Ok(found) => outln!("  transcripts: {} recent session(s)", found.len()),
                    Err(error) => outln!("  transcripts: unreadable ({error})"),
                }
            }
            Ok(())
        }
        // M4 wires up ~/.codex/config.toml and hooks.json.
        Action::Install { agent, .. } | Action::Uninstall { agent } | Action::Status { agent } => {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("setup for {} is not implemented yet", agent.as_str()),
            ))
        }
    }
}

fn print_entries(entries: &[install::EntryStatus]) {
    for entry in entries {
        let mark = if entry.installed { "ok " } else { "-- " };
        let detail = match (&entry.command, &entry.note) {
            (_, Some(note)) => note.clone(),
            (Some(command), None) => command.clone(),
            (None, None) => "not installed".to_string(),
        };
        outln!("  {mark}{:<17} {detail}", entry.event);
    }
}
