//! Headless mode: the bring-up harness that predates the UI and still outlives
//! it as a diagnostic.
//!
//! It binds the named pipe, prints every hook event as it arrives, folds them
//! into a [`SessionTable`], and prints a one-line summary whenever the table
//! changes. `atoll` with no subcommand runs the app instead; this is what you
//! reach for when the question is "is the pipe carrying what I think it is".

use std::io;
use std::sync::Mutex;

use atoll_core::now_unix_secs;
use atoll_core::protocol::{
    Command, Envelope, Event, HookDecision, HookPayload, HookSource, Response, events,
};
use atoll_core::server::{ConnectionHandle, Handler, PipeServer};
use atoll_core::state::{SessionTable, TableCounts};
use clap::{Args as ClapArgs, ValueEnum};

use crate::out::{errln, outln};
use crate::usage_cache::UsageSnapshot;
use crate::util::{timestamp, truncate};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// DANGEROUS: approve every `PermissionRequest` instead of watching them go
    /// past.
    ///
    /// While a server with this flag is running, every agent session on this
    /// machine has its permission prompts answered by Atoll — including sessions
    /// you are not watching. Off by default, in which case a `PermissionRequest`
    /// is held unanswered and the hook fails open to the agent's own prompt.
    #[arg(long)]
    pub auto_allow: bool,

    /// What to do with a `PreToolUse`. Defaults to `ack`, which is what the app
    /// itself does; `hold` is for watching a hook block.
    #[arg(long, value_enum, value_name = "MODE", default_value = "ack")]
    pub pre_tool_use: PreToolUseMode,
}

/// What headless mode does with a `PreToolUse`.
///
/// There is deliberately no mode that *approves* one. `PreToolUse` fires before
/// every tool call, including everything the user's own settings already allow,
/// and before Claude Code has decided whether to ask anybody — so the only
/// answers that make sense are "carry on" and "say nothing".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum PreToolUseMode {
    /// Reply `Ack`: the hook exits immediately, prints nothing, and Claude Code
    /// carries on to its own permission flow — which is what raises the
    /// `PermissionRequest` Atoll can actually answer.
    #[default]
    Ack,
    /// Send nothing. The hook blocks until its own timeout, then fails open.
    /// Useful for watching that block happen; ruinous as a default, since it
    /// would stall every tool call for 45 seconds.
    Hold,
}

impl PreToolUseMode {
    fn as_str(self) -> &'static str {
        match self {
            PreToolUseMode::Ack => "ack",
            PreToolUseMode::Hold => "hold",
        }
    }
}

/// The reply modes in force.
///
/// `PermissionRequest` has no mode of its own: the 2026-08-23 verdict settled
/// its reply shape (the object form; see
/// [`atoll_core::protocol::PermissionRequestDecision`]), so the only remaining
/// question is whether the user opted into answering at all — which is what
/// `--auto-allow` says. And no `PreToolUse` mode approves anything, so there is
/// nothing left here to gate.
#[derive(Debug, Clone, Copy)]
struct Modes {
    auto_allow: bool,
    pre_tool_use: PreToolUseMode,
}

impl Modes {
    fn resolve(args: &Args) -> Self {
        Self {
            auto_allow: args.auto_allow,
            pre_tool_use: args.pre_tool_use,
        }
    }
}

/// Bind the pipe and print the event stream until interrupted.
pub fn run(args: &Args) -> io::Result<()> {
    let modes = Modes::resolve(args);

    // Only the serving modes need a runtime, so `atoll setup` stays instant.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        // Headless takes the pipe over exactly the way the app does, so that
        // "one Atoll at a time" can be tested without a display. See
        // [`crate::single`].
        let bound = crate::single::bind_taking_over().await?;
        let server = bound.server;

        // The integration tests wait for this exact line before firing a hook,
        // so it must be printed only once the pipe is really accepting — and it
        // must stay the *first* line, which is why the eviction notice below
        // comes after the fact rather than before it.
        outln!("atoll: listening on {}", server.path());
        if bound.replaced {
            outln!("atoll: an Atoll was already running; it stood down");
        }
        if modes.auto_allow {
            outln!(
                "atoll: DANGER: approvals are auto-allowed for ALL sessions \
                 (PreToolUse={}, PermissionRequest=object)",
                modes.pre_tool_use.as_str(),
            );
        } else {
            outln!(
                "atoll: observing only; PreToolUse={}, PermissionRequest is left \
                 to the agent's own prompt",
                modes.pre_tool_use.as_str(),
            );
        }

        server.serve(EventPrinter::new(modes)).await
    })
}

/// Claim the configured pipe, turning "someone else has it" into a message that
/// says so. Shared with the UI app, which needs the same diagnosis.
pub fn bind() -> io::Result<PipeServer> {
    PipeServer::bind_configured().map_err(|error| {
        // CreateNamedPipe with FILE_FLAG_FIRST_PIPE_INSTANCE reports a name that
        // is already taken as ERROR_ACCESS_DENIED rather than
        // ERROR_ALREADY_EXISTS, so both kinds mean the same thing here.
        if matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
        ) {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "cannot listen on {}: the name is already in use \
                     (is Atoll already running?) [{error}]",
                    atoll_core::pipe::configured_pipe_path()
                ),
            )
        } else {
            error
        }
    })
}

/// What headless mode sends back for one event.
enum Reply {
    /// Nothing at all: the hook blocks and eventually fails open.
    Hold,
    /// `Ack`: released without a decision.
    Ack,
    Decision(Box<HookDecision>),
}

/// The stand-in for the UI: log every event, track the session table, and answer
/// blocking events according to [`Modes`].
struct EventPrinter {
    modes: Modes,
    table: Mutex<SessionTable>,
    /// The last summary printed, so an event that changes nothing stays quiet.
    last_counts: Mutex<Option<TableCounts>>,
    usage: Mutex<UsageSnapshot>,
}

impl EventPrinter {
    fn new(modes: Modes) -> Self {
        Self {
            modes,
            table: Mutex::new(SessionTable::new()),
            last_counts: Mutex::new(None),
            usage: Mutex::new(UsageSnapshot::default()),
        }
    }
}

impl Handler for EventPrinter {
    fn on_envelope(&self, envelope: Envelope, connection: ConnectionHandle) {
        match envelope {
            Envelope::Command {
                command:
                    Command::ProcessClaudeHook {
                        claude_hook,
                        source,
                    },
            } => self.on_hook(claude_hook, source, connection),
            Envelope::Hello { hello } => {
                outln!("[{}] hello from {}", timestamp(), hello.client);
            }
            Envelope::Event { event } => {
                let Event::Shutdown = event;
                // A newer Atoll is starting and wants this pipe; there is only
                // ever one. Exiting outright rather than unwinding is the whole
                // of what a headless server has to do to let go: the pipe
                // handles are closed by the process ending, and there is no
                // window to take down first.
                outln!("[{}] stood down for a new Atoll", timestamp());
                std::process::exit(0);
            }
            // We are the decision-maker; nobody sends us decisions.
            Envelope::Response { .. } => {}
        }
    }

    fn on_decode_error(&self, line: &str, error: serde_json::Error) {
        let preview: String = line.chars().take(120).collect();
        errln!("[{}] undecodable line ({error}): {preview}", timestamp());
    }
}

impl EventPrinter {
    fn on_hook(&self, payload: HookPayload, source: HookSource, connection: ConnectionHandle) {
        let event_name = payload.event_name().to_string();
        let reply = self.reply_for(&event_name);

        // Answer before logging, never after. Writing to a console can block for
        // an unbounded time — a full pipe buffer when stdout is redirected, or
        // simply a user selecting text in a QuickEdit-enabled console window —
        // and an approval stuck behind a paused terminal would strand the agent
        // for the hook's entire timeout.
        let outcome = self.send(reply, &connection);

        let now = now_unix_secs();
        let counts = {
            let mut table = self.table.lock().expect("session table");
            table.apply(&payload, source, now);
            table.counts(now)
        };

        outln!(
            "[{}] {} {:<17} {}",
            timestamp(),
            short_session(payload.session_id.as_deref()),
            event_name,
            payload.tool_name.as_deref().unwrap_or("-"),
        );
        if let Some(outcome) = outcome {
            outln!(
                "           >>> {} is waiting for a decision: {} {}",
                source.as_str(),
                event_name,
                summarize_input(&payload),
            );
            outln!("           {outcome}");
        }
        self.print_summary_if_changed(counts, now);
    }

    /// The reply for `event_name` under the current modes, or `None` for an
    /// event that does not block and so takes no reply at all.
    fn reply_for(&self, event_name: &str) -> Option<Reply> {
        let reason = || Some("auto-allowed by Atoll (headless)".to_string());
        Some(match event_name {
            // Never a decision, in any mode: see [`PreToolUseMode`].
            events::PRE_TOOL_USE => match self.modes.pre_tool_use {
                PreToolUseMode::Ack => Reply::Ack,
                PreToolUseMode::Hold => Reply::Hold,
            },
            events::PERMISSION_REQUEST if self.modes.auto_allow => {
                HookDecision::allow_for(event_name, reason())
                    .map(|decision| Reply::Decision(Box::new(decision)))
                    .unwrap_or(Reply::Hold)
            }
            events::PERMISSION_REQUEST => Reply::Hold,
            // Nothing else blocks, so nothing else gets a reply.
            _ => return None,
        })
    }

    /// Send `reply`, returning the line to log about it — or `None` when this
    /// was not a blocking event and there is nothing to report.
    fn send(&self, reply: Option<Reply>, connection: &ConnectionHandle) -> Option<String> {
        let (envelope, note) = match reply? {
            Reply::Hold => {
                return Some("... holding; the hook will block and then fail open".to_string());
            }
            Reply::Ack => (
                Envelope::Response {
                    response: Response::Ack,
                },
                "<<< acked (no decision; Claude Code prompts as usual)",
            ),
            Reply::Decision(decision) => (
                Envelope::Response {
                    response: Response::Decision {
                        decision: *decision,
                    },
                },
                "<<< auto-allowed",
            ),
        };
        Some(match connection.send(&envelope) {
            Ok(()) => note.to_string(),
            Err(error) => format!("<<< could not reply: {error}"),
        })
    }

    /// Print the one-line table summary, but only when it says something new.
    fn print_summary_if_changed(&self, counts: TableCounts, now: u64) {
        let mut last = self.last_counts.lock().expect("summary state");
        if *last == Some(counts) {
            return;
        }
        *last = Some(counts);

        let usage = self.usage.lock().expect("usage cache").refreshed(now);
        outln!("           == {}", summary_line(counts, &usage));
    }
}

/// `N sessions: X running, Y waiting | claude 5h A% 7d B% | codex 5h C% 7d D%`
fn summary_line(counts: TableCounts, usage: &UsageSnapshot) -> String {
    let mut line = format!(
        "{} session{}: {} running, {} waiting",
        counts.total,
        if counts.total == 1 { "" } else { "s" },
        counts.running,
        counts.waiting,
    );
    if counts.completed > 0 {
        line.push_str(&format!(", {} done", counts.completed));
    }
    for segment in usage.detail_lines() {
        line.push_str(&format!(" | {segment}"));
    }
    line
}

/// First 8 characters of the session id — enough to tell sessions apart in a log
/// without making every line unreadable.
fn short_session(session_id: Option<&str>) -> String {
    let id = session_id.unwrap_or("--------");
    id.chars().take(8).collect()
}

/// A one-line preview of what the tool was asked to do.
pub fn summarize_input(payload: &HookPayload) -> String {
    let Some(input) = &payload.tool_input else {
        return String::new();
    };
    // Prefer the fields a human would actually recognize.
    for key in ["command", "file_path", "path", "pattern", "url"] {
        if let Some(value) = input.get(key).and_then(|value| value.as_str()) {
            return truncate(&crate::util::one_line(value), 100);
        }
    }
    truncate(&crate::util::one_line(&input.to_string()), 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::usage::{ClaudeLimits, CodexUsage, UsageLimit, WindowUsage};

    fn window(used: f64, minutes: Option<u64>) -> WindowUsage {
        WindowUsage {
            used_percent: used,
            resets_at: None,
            window_minutes: minutes,
        }
    }

    fn args(auto_allow: bool, pre_tool_use: PreToolUseMode) -> Args {
        Args {
            auto_allow,
            pre_tool_use,
        }
    }

    #[test]
    fn the_summary_line_reads_as_advertised() {
        let counts = TableCounts {
            total: 3,
            running: 1,
            waiting: 2,
            completed: 0,
            stale: 0,
        };
        let usage = UsageSnapshot {
            claude: ClaudeLimits {
                limits: vec![
                    UsageLimit {
                        kind: "session".into(),
                        label: "5h".into(),
                        percent: 23.5,
                        resets_at: None,
                    },
                    UsageLimit {
                        kind: "weekly_all".into(),
                        label: "7d".into(),
                        percent: 61.0,
                        resets_at: None,
                    },
                ],
                fetched_at: None,
            },
            codex: Some(CodexUsage {
                primary: Some(window(7.0, Some(300))),
                secondary: Some(window(58.0, Some(10_080))),
                plan_type: Some("pro".into()),
                source: None,
            }),
            refreshed_at: None,
        };
        assert_eq!(
            summary_line(counts, &usage),
            "3 sessions: 1 running, 2 waiting | claude 5h 77% 7d 39% | codex 5h 93% Week 42%"
        );
    }

    #[test]
    fn the_summary_line_drops_usage_it_does_not_have() {
        let counts = TableCounts {
            total: 1,
            running: 1,
            ..TableCounts::default()
        };
        assert_eq!(
            summary_line(counts, &UsageSnapshot::default()),
            "1 session: 1 running, 0 waiting"
        );
    }

    #[test]
    fn completed_sessions_are_only_mentioned_when_there_are_some() {
        let counts = TableCounts {
            total: 2,
            running: 1,
            completed: 1,
            ..TableCounts::default()
        };
        assert!(summary_line(counts, &UsageSnapshot::default()).contains("1 done"));
    }

    #[test]
    fn pre_tool_use_is_acked_by_default_in_both_modes() {
        // Holding a PreToolUse stalls every tool call for the hook's whole
        // budget, so it is never what happens unless somebody asks for it.
        assert_eq!(
            Modes::resolve(&args(false, PreToolUseMode::default())).pre_tool_use,
            PreToolUseMode::Ack
        );
        assert_eq!(
            Modes::resolve(&args(true, PreToolUseMode::default())).pre_tool_use,
            PreToolUseMode::Ack
        );
        assert_eq!(
            Modes::resolve(&args(false, PreToolUseMode::Hold)).pre_tool_use,
            PreToolUseMode::Hold,
            "but it is still available on request"
        );
    }

    /// The defect this guards: `PreToolUse` fires for every tool call, so a mode
    /// that answered one with a permission decision would be deciding on the
    /// user's behalf hundreds of times a session.
    #[test]
    fn no_mode_ever_decides_a_pre_tool_use() {
        for auto_allow in [false, true] {
            for pre_tool_use in [PreToolUseMode::Ack, PreToolUseMode::Hold] {
                let printer = EventPrinter::new(Modes {
                    auto_allow,
                    pre_tool_use,
                });
                let reply = printer.reply_for(events::PRE_TOOL_USE);
                assert!(
                    !matches!(reply, Some(Reply::Decision(_))),
                    "auto_allow={auto_allow} pre_tool_use={pre_tool_use:?} produced a decision"
                );
                match pre_tool_use {
                    PreToolUseMode::Ack => assert!(matches!(reply, Some(Reply::Ack))),
                    PreToolUseMode::Hold => assert!(matches!(reply, Some(Reply::Hold))),
                }
            }
        }
    }

    #[test]
    fn a_permission_request_is_the_only_thing_auto_allow_answers() {
        let allowing = EventPrinter::new(Modes {
            auto_allow: true,
            pre_tool_use: PreToolUseMode::Ack,
        });
        let Some(Reply::Decision(decision)) = allowing.reply_for(events::PERMISSION_REQUEST) else {
            panic!("expected a decision");
        };
        // The object form, which is the only PermissionRequest shape there is.
        assert!(decision.to_stdout_json().contains(r#""behavior":"allow""#));

        let watching = EventPrinter::new(Modes {
            auto_allow: false,
            pre_tool_use: PreToolUseMode::Ack,
        });
        assert!(matches!(
            watching.reply_for(events::PERMISSION_REQUEST),
            Some(Reply::Hold)
        ));

        // Nothing else ever gets a reply, whatever the modes say — and a
        // non-blocking event must produce no log line about deciding either.
        assert!(allowing.reply_for(events::STOP).is_none());
        assert!(allowing.reply_for(events::SESSION_START).is_none());
        assert!(allowing.reply_for(events::POST_TOOL_USE).is_none());
    }

    #[test]
    fn the_input_summary_prefers_the_field_a_human_reads() {
        let payload: HookPayload = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"git\n  status"}}"#,
        )
        .unwrap();
        assert_eq!(summarize_input(&payload), "git status");

        let bare: HookPayload = serde_json::from_str(r#"{"hook_event_name":"Stop"}"#).unwrap();
        assert_eq!(summarize_input(&bare), "");
    }
}
