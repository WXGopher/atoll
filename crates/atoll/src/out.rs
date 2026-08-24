//! Printing that never panics.
//!
//! `println!` panics when the underlying write fails, and Atoll's stdout is
//! routinely a pipe whose reader has already gone away: a test harness that saw
//! the line it was waiting for, a shell that closed a pager, a parent process
//! that exited. That showed up as
//! `failed printing to stdout: os error 232` — `ERROR_BROKEN_PIPE` — killing the
//! whole process mid-session, which for a tool holding a live approval is the
//! worst possible moment.
//!
//! Every line Atoll prints goes through here instead, where nobody listening is
//! simply nothing to do.

use std::fmt::Arguments;
use std::io::Write;

pub fn stdout_line(args: Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if writeln!(handle, "{args}").is_ok() {
        let _ = handle.flush();
    }
}

pub fn stderr_line(args: Arguments<'_>) {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "{args}");
}

/// `println!` that shrugs off a closed pipe.
macro_rules! outln {
    () => { $crate::out::stdout_line(::std::format_args!("")) };
    ($($arg:tt)*) => { $crate::out::stdout_line(::std::format_args!($($arg)*)) };
}

/// `eprintln!` that shrugs off a closed pipe.
macro_rules! errln {
    ($($arg:tt)*) => { $crate::out::stderr_line(::std::format_args!($($arg)*)) };
}

pub(crate) use {errln, outln};
