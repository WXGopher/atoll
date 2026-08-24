//! Atoll's user-facing binary.
//!
//! With no subcommand it runs the app: a usage readout in the taskbar, cards
//! for the approvals a session needs, a tray icon, and the named-pipe server
//! that feeds them all. The subcommands are the parts that have to
//! work from a terminal — `headless` for watching the raw event stream, `setup`
//! for hook installation, and `statusline` for the usage bridge Claude Code
//! invokes on every turn.
//!
//! Built for the GUI subsystem so that double-clicking `atoll.exe` — or running
//! it at login — opens no console window. The subcommands get their terminal
//! back through [`attach_parent_console`].
#![windows_subsystem = "windows"]

mod app;
mod headless;
mod out;
mod setup;
mod single;
mod statusline;
mod usage_cache;
mod util;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::out::errln;

/// Windows-native agent session monitor and approval hub.
#[derive(Debug, Parser)]
#[command(name = "atoll", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch the hook event stream in a terminal, with no windows at all.
    Headless(headless::Args),
    /// Install, remove, or inspect Atoll's hook wiring for an agent.
    Setup {
        #[command(subcommand)]
        action: setup::Action,
    },
    /// Render a status line from a payload on stdin (used by Claude Code).
    Statusline,
}

/// A GUI-subsystem process launched from a shell starts with no console and no
/// standard handles, which would make every subcommand silent. Attaching to the
/// parent's console gives them back — but only when the handles are actually
/// missing: when a parent piped them (Claude Code running `statusline`, the
/// test harness running `headless`), attaching would clobber the redirection.
/// With no console to attach to — the double-click case — this is a no-op.
fn attach_parent_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    let missing = |handle| unsafe {
        !GetStdHandle(handle).is_ok_and(|handle| !handle.is_invalid() && !handle.0.is_null())
    };
    if missing(STD_OUTPUT_HANDLE) && missing(STD_ERROR_HANDLE) {
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

fn main() -> ExitCode {
    // Only the subcommands belong on a terminal. The bare app must never
    // attach: launched from a shell-adjacent parent (a scripted start, a
    // hotkey runner), attaching would spill its startup banner into whatever
    // TUI happens to own that console.
    if std::env::args_os().nth(1).is_some() {
        attach_parent_console();
    }
    let cli = Cli::parse();

    let result = match cli.command {
        None => app::run(),
        Some(Command::Headless(args)) => headless::run(&args),
        Some(Command::Setup { action }) => setup::run(action),
        Some(Command::Statusline) => statusline::run(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            errln!("atoll: {error}");
            ExitCode::FAILURE
        }
    }
}
