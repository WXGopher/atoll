//! Atoll's hook binary.
//!
//! Agents spawn this on every hook event, so it must start fast, stay small,
//! and never block the agent. It reads a JSON payload on stdin and always exits
//! 0 — a broken hook must not break the session it observes.
//!
//! # Fail-open
//!
//! Every failure path — no app running, pipe busy, malformed stdin, a decision
//! that never arrives — ends the same way: **nothing on stdout, exit 0**. The
//! agent then falls back to its own permission prompt, which is exactly the
//! behavior a user gets without Atoll installed. There is deliberately no error
//! reporting on stdout or stderr; the agent would render it inside the session.
//!
//! # Why threads instead of async
//!
//! `std::fs::File` has no read timeout on Windows, and pulling in tokio would
//! cost more startup than the whole hook budget. So the pipe work runs on a
//! worker thread that reports progress over a channel, and the main thread
//! enforces deadlines with `recv_timeout`. When a deadline blows, `exit(0)`
//! abandons the still-blocked worker — correct, because at that point the only
//! thing left to do is nothing.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use atoll_core::pipe;
use atoll_core::protocol::{
    Command, Envelope, HookPayload, HookSource, Response, TERMINAL_ENV_VARS, TerminalMeta,
    encode_line, timeouts,
};
use serde_json::{Map, Value};

/// `ERROR_PIPE_BUSY`: every instance of the pipe is serving another client.
/// Worth retrying, unlike "no such pipe", which means no app is running.
const ERROR_PIPE_BUSY: i32 = 231;

/// How long to wait between attempts while the pipe is busy.
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// Progress reports from the worker thread. Each one releases the main thread
/// from one deadline.
enum Progress {
    Connected,
    Sent,
    Reply(String),
}

fn main() {
    // Exit before touching stdin: the agent does not care whether we drained it.
    if pipe::hooks_disabled() {
        return;
    }

    let source = parse_source();

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return;
    }

    // A payload we cannot parse is a payload we cannot act on. Fail open.
    let Ok(mut payload) = serde_json::from_str::<HookPayload>(&raw) else {
        return;
    };
    payload.set_terminal_meta(collect_terminal_meta());

    let event_name = payload.event_name().to_string();
    let wait_budget = timeouts::for_event(&event_name, source);

    let envelope = Envelope::Command {
        command: Command::ProcessClaudeHook {
            claude_hook: payload,
            source,
        },
    };
    let Ok(line) = encode_line(&envelope) else {
        return;
    };

    if let Some(reply) = exchange(line, wait_budget)
        && let Some(stdout_json) = decision_stdout(&reply)
    {
        // print!, not println!: the decision already ends in a newline.
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(stdout_json.as_bytes());
        let _ = stdout.flush();
    }
}

/// `--source claude|codex`, defaulting to Claude Code.
///
/// The default matters: if a Claude Code build ignores the installer's `args`
/// array and runs the bare command string, the hook still identifies itself
/// correctly instead of failing to parse its own arguments.
fn parse_source() -> HookSource {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--source=") {
            Some(inline) => Some(inline.to_string()),
            None if arg == "--source" => args.next(),
            None => None,
        };
        if let Some(value) = value
            && let Some(source) = HookSource::parse(value.trim())
        {
            return source;
        }
    }
    HookSource::Claude
}

/// The terminal-identifying environment this hook inherited, so the app can
/// offer "jump back to the session".
fn collect_terminal_meta() -> TerminalMeta {
    let mut env = Map::new();
    for name in TERMINAL_ENV_VARS {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            env.insert((*name).to_string(), Value::String(value));
        }
    }
    TerminalMeta {
        env,
        hook_pid: std::process::id(),
        // The parent is the agent CLI, which outlives this hook by the whole
        // session. The app walks the rest of the ancestry at click time, when
        // somebody actually asks to jump back to the terminal.
        parent_pid: parent_pid(),
    }
}

/// PID of this process's parent, from `NtQueryInformationProcess`.
///
/// One syscall on the pseudo-handle: no snapshot, no allocation, and no new
/// dependency, which matters in a binary that runs on every hook event. The
/// parent may already be gone by the time anyone reads this — PID reuse is the
/// reader's problem, and the reader guards against it by only ever activating
/// windows owned by processes that look like terminals.
#[cfg(windows)]
fn parent_pid() -> Option<u32> {
    #[repr(C)]
    struct ProcessBasicInformation {
        exit_status: isize,
        peb_base_address: *mut core::ffi::c_void,
        affinity_mask: usize,
        base_priority: isize,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process: isize,
            class: u32,
            info: *mut core::ffi::c_void,
            len: u32,
            written: *mut u32,
        ) -> i32;
    }

    // GetCurrentProcess() without linking kernel32 declarations: the
    // documented pseudo-handle value.
    const CURRENT_PROCESS: isize = -1;
    const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;

    let mut info = ProcessBasicInformation {
        exit_status: 0,
        peb_base_address: std::ptr::null_mut(),
        affinity_mask: 0,
        base_priority: 0,
        unique_process_id: 0,
        inherited_from_unique_process_id: 0,
    };
    let mut written = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(
            CURRENT_PROCESS,
            PROCESS_BASIC_INFORMATION_CLASS,
            (&raw mut info).cast(),
            size_of::<ProcessBasicInformation>() as u32,
            &raw mut written,
        )
    };
    (status == 0).then(|| u32::try_from(info.inherited_from_unique_process_id).ok())?
}

#[cfg(not(windows))]
fn parent_pid() -> Option<u32> {
    None
}

/// Connect, send `line`, and — when `wait_budget` is set — wait that long for a
/// reply line. `None` on any failure or timeout.
fn exchange(line: String, wait_budget: Option<Duration>) -> Option<String> {
    let path = pipe::configured_pipe_path();
    let wants_reply = wait_budget.is_some();
    let (tx, rx) = mpsc::channel::<Progress>();

    thread::spawn(move || {
        let Ok(mut pipe) = connect(&path, timeouts::CONNECT) else {
            return;
        };
        if tx.send(Progress::Connected).is_err() {
            return;
        }
        if pipe.write_all(line.as_bytes()).is_err() || pipe.flush().is_err() {
            return;
        }
        if tx.send(Progress::Sent).is_err() || !wants_reply {
            return;
        }

        let mut reply = String::new();
        if BufReader::new(pipe).read_line(&mut reply).is_ok() && !reply.trim().is_empty() {
            let _ = tx.send(Progress::Reply(reply));
        }
    });

    if !matches!(rx.recv_timeout(timeouts::CONNECT), Ok(Progress::Connected)) {
        return None;
    }
    if !matches!(rx.recv_timeout(timeouts::SEND), Ok(Progress::Sent)) {
        return None;
    }

    match rx.recv_timeout(wait_budget?) {
        Ok(Progress::Reply(reply)) => Some(reply),
        // A closed channel means the app hung up without deciding; a timeout
        // means it never got around to it. Both fail open.
        Ok(_) | Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// Open the pipe, retrying only while it is busy and only within `budget`.
fn connect(path: &str, budget: Duration) -> std::io::Result<File> {
    let deadline = Instant::now() + budget;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => return Ok(file),
            Err(error) => {
                let busy = error.raw_os_error() == Some(ERROR_PIPE_BUSY);
                if !busy || Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(BUSY_RETRY_INTERVAL);
            }
        }
    }
}

/// Turn a reply line into the exact bytes the agent expects on stdout.
fn decision_stdout(reply: &str) -> Option<String> {
    match atoll_core::protocol::decode_line(reply.trim_end()).ok()? {
        Envelope::Response {
            response: Response::Decision { decision },
        } => Some(decision.to_stdout_json()),
        // Ack / Error / anything else: the app explicitly declined to decide.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn parent_pid_answers_with_a_real_process() {
        // This test process was spawned by the test runner, so the syscall
        // must find a parent — a real pid, not a system sentinel.
        let parent = parent_pid().expect("a parent pid");
        assert!(parent > 4);
        assert_ne!(parent, std::process::id());
    }

    #[test]
    fn terminal_meta_carries_the_parent() {
        let meta = collect_terminal_meta();
        assert_eq!(meta.hook_pid, std::process::id());
        if cfg!(windows) {
            assert!(meta.parent_pid.is_some());
        }
    }
}
