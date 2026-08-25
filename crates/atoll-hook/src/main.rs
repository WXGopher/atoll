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
        // Captured here, not at click time: the hook's own parent is usually a
        // shell that lives for milliseconds, and this is the one moment the
        // whole chain up to the terminal is certainly alive.
        ancestors: ancestry::collect(),
    }
}

/// The process ancestry walk, on raw FFI.
///
/// Raw rather than the `windows` crate because this binary is spawned on every
/// hook event: it keeps exactly two dependencies, and three syscalls per hop —
/// open, ask, name — cost microseconds against a startup budget of
/// single-digit milliseconds.
#[cfg(windows)]
mod ancestry {
    use atoll_core::protocol::ProcessRef;

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

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn QueryFullProcessImageNameW(
            handle: isize,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
    }

    /// `GetCurrentProcess()`'s documented pseudo-handle value.
    const CURRENT_PROCESS: isize = -1;
    const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    /// This process's ancestors, nearest first, stopping before the shell.
    ///
    /// Every entry was alive at the instant it was recorded, pid and name
    /// taken from the same open handle — which is what lets a later reader
    /// treat "same pid, same exe" as "same process" with a straight face.
    /// Stops at `explorer.exe`: it is everyone's ancestor and nobody's
    /// terminal. The cap is paranoia against parent-pid cycles.
    pub fn collect() -> Vec<ProcessRef> {
        let mut chain: Vec<ProcessRef> = Vec::new();
        let mut next = parent_of(CURRENT_PROCESS);
        while chain.len() < 12 {
            let Some(pid) = next.filter(|&pid| pid > 4) else {
                break;
            };
            if chain.iter().any(|entry| entry.pid == pid) {
                break;
            }
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle == 0 {
                break;
            }
            let exe = exe_name(handle);
            next = parent_of(handle);
            unsafe { CloseHandle(handle) };
            let Some(exe) = exe else { break };
            if exe == "explorer.exe" {
                break;
            }
            chain.push(ProcessRef { pid, exe });
        }
        chain
    }

    /// The parent's pid, from `ProcessBasicInformation`.
    fn parent_of(handle: isize) -> Option<u32> {
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
                handle,
                PROCESS_BASIC_INFORMATION_CLASS,
                (&raw mut info).cast(),
                size_of::<ProcessBasicInformation>() as u32,
                &raw mut written,
            )
        };
        (status == 0)
            .then(|| u32::try_from(info.inherited_from_unique_process_id).ok())?
    }

    /// The executable's file name, lowercased: `"windowsterminal.exe"`.
    fn exe_name(handle: isize) -> Option<String> {
        let mut buffer = [0u16; 512];
        let mut length = buffer.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length)
        };
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        Some(name.to_lowercase())
    }
}

#[cfg(not(windows))]
mod ancestry {
    pub fn collect() -> Vec<atoll_core::protocol::ProcessRef> {
        Vec::new()
    }
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
    fn the_ancestry_walk_finds_real_processes() {
        // This test process was spawned by the test runner, so the chain must
        // hold at least that — live pids, named executables, no duplicates.
        let chain = ancestry::collect();
        assert!(!chain.is_empty());
        for entry in &chain {
            assert!(entry.pid > 4);
            assert_ne!(entry.pid, std::process::id());
            assert!(entry.exe.ends_with(".exe"), "unexpected exe: {}", entry.exe);
            assert_eq!(entry.exe, entry.exe.to_lowercase());
            assert_ne!(entry.exe, "explorer.exe");
        }
        let mut pids: Vec<u32> = chain.iter().map(|entry| entry.pid).collect();
        pids.dedup();
        assert_eq!(pids.len(), chain.len());
    }

    #[test]
    fn terminal_meta_carries_the_ancestry() {
        let meta = collect_terminal_meta();
        assert_eq!(meta.hook_pid, std::process::id());
        if cfg!(windows) {
            assert!(!meta.ancestors.is_empty());
        }
    }
}
