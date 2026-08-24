//! The status line bridge: cache what Claude Code told us, then render — or hand
//! the payload on to the status line Atoll displaced.
//!
//! Every step degrades rather than fails. Claude Code renders this command's
//! stdout inside the session on every turn, so an error message here would be
//! pinned under the user's conversation until they went and fixed it.

use std::io::{self, Read, Write};
use std::process::Stdio;

use atoll_core::now_unix_secs;
use atoll_core::{install, usage};
use serde_json::Value;

use crate::out::outln;

pub fn run() -> io::Result<()> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    // The whole reason the bridge exists: `rate_limits` appears in this payload
    // and nowhere else Atoll can reach. The model name and context percentage
    // ride along for free, and Atoll shows those too.
    let fields = usage::status_fields(&payload);
    if !fields.is_empty()
        && let Ok(path) = usage::rl_cache_path()
    {
        let _ = usage::write_status_cache(&path, &fields, now_unix_secs());
    }

    let original = install::claude_settings_path()
        .ok()
        .and_then(|path| install::read_original_statusline(&path));

    // If the user has a status line of their own behind us, it *is* the status
    // line. Atoll never renders over it — not even when it produces nothing, and
    // not even when it fails to start. An empty line is a bug the user can see
    // and fix; Atoll's line quietly standing in their place is one they cannot.
    if let Some(entry) = original {
        let code = delegate(&entry, &raw);
        // The child inherited our stdout, so its bytes and its exit status are
        // already Claude Code's. Leaving through `exit` keeps the status intact.
        std::process::exit(code);
    }

    outln!("{}", render(&payload));
    Ok(())
}

/// Run the status line Atoll displaced, on Atoll's own stdout, and return the
/// exit code to leave with.
///
/// # Transparency
///
/// The child **inherits** this process' stdout and stderr rather than being
/// captured and replayed. That is the whole point: no buffering, no encoding
/// assumption, no size limit, and no place for Atoll to accidentally alter a
/// byte. A status line that emits UTF-16, ANSI colour, or nothing at all reaches
/// Claude Code exactly as it would have without Atoll in the way.
///
/// A child that cannot be started, or that dies on a signal, yields `0` and an
/// empty line. Never Atoll's own rendering: standing in for the user's status
/// line is a worse failure than showing nothing.
///
/// # Why `cmd /S /C`
///
/// `statusLine.command` is a **shell string**, not an argv: users write things
/// like `mytool | head -1` there, and Claude Code's own Windows implementation
/// hands it to `%ComSpec%` — `cmd.exe`. Running it through `cmd` therefore
/// reproduces the exact quoting, redirection, and `PATH` lookup the user's line
/// was written against. PowerShell would parse the same string differently
/// (`&` and `|` mean other things, and a quoted leading path needs `&`), so it
/// is not a safe substitute.
///
/// The command line is built with [`CommandExt::raw_arg`] rather than `arg`,
/// and this is the bug that made a real user's status line disappear. Rust's
/// normal argument escaping is written for `CreateProcess` parsing: it wraps an
/// argument containing spaces in quotes and escapes interior quotes as `\"`.
/// `cmd.exe` does neither — it has its own rules, and `\"` means a literal
/// backslash followed by a quote. So a perfectly ordinary command like
/// `powershell -File "C:\My Scripts\line.ps1"` reached cmd mangled and failed.
/// `/S` makes cmd's behaviour simple and total: strip the first and last quote
/// of the remainder, run everything in between verbatim.
fn delegate(entry: &Value, stdin_payload: &str) -> i32 {
    let Some(program) = entry
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|program| !program.is_empty())
    else {
        return 0;
    };

    let args: Vec<String> = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut command = if args.is_empty() {
        // Shell form. See the doc comment above for why this is cmd, and why
        // the command line is assembled by hand.
        let mut command = std::process::Command::new(shell());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.raw_arg("/S");
            command.raw_arg("/C");
            command.raw_arg(format!("\"{program}\""));
        }
        #[cfg(not(windows))]
        command.arg("-c").arg(program);
        command
    } else {
        // Exec form: an argv, spawned directly, with no shell to quote for.
        let mut command = std::process::Command::new(program);
        command.args(&args);
        command
    };

    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        // The user's command is gone or unrunnable. Say nothing and let them
        // see an empty status line, which is the truth.
        Err(_) => return 0,
    };

    // Ignore a write failure: a status line that does not read its stdin is
    // perfectly normal, and closing the pipe on it is not an error. Dropping the
    // handle afterwards is what gives the child its EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_payload.as_bytes());
    }

    child
        .wait()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(0)
}

/// `%ComSpec%`, or `cmd.exe` when it is unset.
#[cfg(windows)]
fn shell() -> String {
    std::env::var("ComSpec")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string())
}

#[cfg(not(windows))]
fn shell() -> String {
    "/bin/sh".to_string()
}

/// `[<model>] <n>% context`, dropping whichever half the payload did not carry.
fn render(payload: &Value) -> String {
    let model = payload
        .get("model")
        .and_then(|model| model.get("display_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let context = payload
        .get("context_window")
        .and_then(|window| window.get("used_percentage"))
        .and_then(loose_f64);

    match (model, context) {
        (Some(model), Some(percent)) => format!("[{model}] {percent:.0}% context"),
        (Some(model), None) => format!("[{model}]"),
        (None, Some(percent)) => format!("{percent:.0}% context"),
        (None, None) => String::new(),
    }
}

/// A number that may have arrived as a string, since this payload comes from
/// another program.
fn loose_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().trim_end_matches('%').trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_status_line_renders_model_and_context() {
        let payload = json!({
            "model": {"display_name": "Opus 5"},
            "context_window": {"used_percentage": 42.4},
        });
        assert_eq!(render(&payload), "[Opus 5] 42% context");
    }

    #[test]
    fn the_status_line_degrades_field_by_field() {
        assert_eq!(
            render(&json!({"model": {"display_name": "Opus 5"}})),
            "[Opus 5]"
        );
        assert_eq!(
            render(&json!({"context_window": {"used_percentage": "7"}})),
            "7% context"
        );
        assert_eq!(render(&json!({})), "");
        assert_eq!(render(&Value::Null), "");
        assert_eq!(render(&json!({"model": {"display_name": "  "}})), "");
    }
}
