//! The seam between the pipe server's threads and Slint's event loop.
//!
//! The server runs a tokio runtime on a thread of its own — it has to, since a
//! blocked hook holds its connection open for as long as the user takes to
//! decide, and none of that may happen on the thread painting the card. What
//! crosses over is a plain channel of [`HookEvent`]s plus a wake-up through
//! [`slint::invoke_from_event_loop`]; nothing in this module touches UI state.

use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};

use atoll_core::protocol::{Command, Envelope, HookPayload, HookSource, Response, events};
use atoll_core::server::{ConnectionHandle, Handler};

/// One hook event, on its way to the UI thread.
pub struct HookEvent {
    pub payload: HookPayload,
    pub source: HookSource,
    /// The waiting hook's connection.
    ///
    /// Present only for `PermissionRequest` — the one event that means a human
    /// is about to be asked. Holding it is what keeps the agent paused; dropping
    /// every clone of it, or failing to send on it, is how the hook finds out
    /// that nobody is coming.
    ///
    /// A `PreToolUse` never arrives with one: it has already been released by
    /// the time the UI hears about it. See [`Forwarder::on_envelope`].
    pub reply: Option<ConnectionHandle>,
}

/// A live pipe server. Dropping this does not stop it: the thread owns the
/// runtime and runs until the process exits, which is exactly as long as the
/// window it feeds.
pub struct Bridge {
    pub path: String,
    /// Whether an Atoll that was already running was stood down to get here.
    pub replaced: bool,
    pub events: Receiver<HookEvent>,
}

/// Bind the configured pipe and start serving on a background thread.
///
/// Binding happens on that thread but is reported back before this returns, so
/// a pipe that cannot be had at all is still a startup error the caller can
/// print rather than a window that opens and then quietly does nothing. A pipe
/// held by another Atoll is not that: see [`crate::single`].
pub fn start() -> io::Result<Bridge> {
    let (events_tx, events_rx) = channel();
    let (ready_tx, ready_rx) = channel();

    std::thread::Builder::new()
        .name("atoll-pipe".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            runtime.block_on(async move {
                let bound = match crate::single::bind_taking_over().await {
                    Ok(bound) => bound,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx
                    .send(Ok((bound.server.path().to_string(), bound.replaced)))
                    .is_err()
                {
                    // The app gave up before we finished binding.
                    return;
                }
                let _ = bound.server.serve(Forwarder { events: events_tx }).await;
            });
        })?;

    let (path, replaced) = ready_rx
        .recv()
        .map_err(|_| io::Error::other("the pipe server thread stopped before it bound"))??;

    Ok(Bridge {
        path,
        replaced,
        events: events_rx,
    })
}

struct Forwarder {
    events: Sender<HookEvent>,
}

impl Handler for Forwarder {
    fn on_envelope(&self, envelope: Envelope, connection: ConnectionHandle) {
        // A newer Atoll is starting and wants this pipe. There is only ever one
        // Atoll on a machine — see [`crate::single`] — so this one goes, and it
        // goes the same way the tray's Quit takes it: through the event loop, so
        // the windows come down in order.
        if let Envelope::Event {
            event: atoll_core::protocol::Event::Shutdown,
        } = envelope
        {
            let _ = slint::invoke_from_event_loop(|| {
                slint::quit_event_loop().ok();
            });
            return;
        }

        let Envelope::Command {
            command:
                Command::ProcessClaudeHook {
                    claude_hook,
                    source,
                },
        } = envelope
        else {
            // Hellos and stray responses carry nothing a card can show.
            return;
        };

        // Which events Atoll actually holds open, decided here rather than on
        // the UI thread so that releasing a hook never queues behind a repaint.
        //
        // `PreToolUse` fires before *every* tool call, including everything the
        // user's own settings already allow, and its hook waits 45 seconds for
        // an answer. Answering it with anything but "carry on" would mean a card
        // per tool call and a session that stalls on each one, so it is acked
        // the moment it arrives: the hook exits silently and Claude Code runs
        // its own permission flow, which is what raises the `PermissionRequest`
        // below if a human is really needed.
        let reply = match claude_hook.event_name() {
            events::PERMISSION_REQUEST => Some(connection),
            events::PRE_TOOL_USE => {
                let _ = connection.send(&Envelope::Response {
                    response: Response::Ack,
                });
                None
            }
            // Nothing else waits on a reply at all.
            _ => None,
        };

        if self
            .events
            .send(HookEvent {
                payload: claude_hook,
                source,
                reply,
            })
            .is_err()
        {
            // The UI is gone; there is nobody left to decide anything.
            return;
        }

        // Nudge the event loop into draining the queue. The closure captures
        // nothing, which is what lets it be `Send`; the UI thread finds its own
        // state through a thread-local. Before the loop is running this fails,
        // and the first thing the app does after starting it is drain anyway.
        let _ = slint::invoke_from_event_loop(super::pump);
    }
}
