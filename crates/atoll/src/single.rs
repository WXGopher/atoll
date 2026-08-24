//! One Atoll at a time.
//!
//! The named pipe is the identity: whoever holds `\\.\pipe\atoll` is *the*
//! Atoll, and every hook on the machine talks to it. Two of them would be two
//! readouts in the taskbar, one of which receives no events — which is
//! exactly what a user who double-clicks the shortcut, or a developer who has
//! just built a new binary, ends up with.
//!
//! So starting Atoll replaces whatever Atoll is already running rather than
//! refusing to start. The incumbent is asked to stand down over the very pipe
//! it holds — a protocol [`Event::Shutdown`], which is already what Atoll sends
//! its peers when it is going away — and the newcomer waits for the name to come
//! free.
//!
//! Refusing would be safe and is what this used to do; it is also useless. There
//! is no version of "another Atoll is already running" that the person starting
//! this one wanted to hear.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use atoll_core::pipe;
use atoll_core::protocol::{Envelope, Event, encode_line};
use atoll_core::server::PipeServer;

/// How long the incumbent gets to let go of the pipe.
///
/// Long enough for an event loop to notice a message and unwind, short enough
/// that a wedged process does not leave the user staring at nothing. Past it the
/// caller reports what happened rather than hanging.
pub const TAKEOVER_BUDGET: Duration = Duration::from_secs(3);
/// How often the name is retried while the incumbent is on its way out.
const RETRY: Duration = Duration::from_millis(100);

/// A bound pipe, and whether taking it involved evicting anybody.
pub struct Bound {
    pub server: PipeServer,
    pub replaced: bool,
}

/// Claim the configured pipe, standing down whatever holds it.
///
/// Must be called inside a tokio runtime, because binding is tokio's.
pub async fn bind_taking_over() -> io::Result<Bound> {
    let taken = match crate::headless::bind() {
        Ok(server) => {
            return Ok(Bound {
                server,
                replaced: false,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => error,
        // A failure that is not "somebody has it" is not ours to resolve.
        Err(error) => return Err(error),
    };

    // Sent even if the write fails: an incumbent that is already on its way out
    // has nothing to receive and the retry below will find the name free
    // regardless. The only thing worth reporting is the deadline expiring.
    ask_the_incumbent_to_stand_down();

    let deadline = Instant::now() + TAKEOVER_BUDGET;
    loop {
        tokio::time::sleep(RETRY).await;
        match crate::headless::bind() {
            Ok(server) => {
                return Ok(Bound {
                    server,
                    replaced: true,
                });
            }
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            Err(_) if Instant::now() < deadline => continue,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "another Atoll still holds {} {} seconds after being asked to \
                         stand down; close it and try again [{taken}]",
                        pipe::configured_pipe_path(),
                        TAKEOVER_BUDGET.as_secs(),
                    ),
                ));
            }
        }
    }
}

/// Open the pipe as a client and send one `Shutdown` event.
///
/// Deliberately plain blocking I/O: it is one short line to a local pipe, and
/// the alternative is a tokio client for the one place in the app that is a
/// client at all.
fn ask_the_incumbent_to_stand_down() {
    let Ok(line) = encode_line(&Envelope::Event {
        event: Event::Shutdown,
    }) else {
        return;
    };
    let path = pipe::configured_pipe_path();
    let Ok(mut handle) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return;
    };
    let _ = handle.write_all(line.as_bytes());
    let _ = handle.flush();
    // Dropping the handle closes the connection, which is what lets the
    // incumbent's read loop finish rather than sit on a half-open pipe while it
    // is trying to exit.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is a user-facing number: it is how long someone stares at
    /// nothing after double-clicking the shortcut.
    #[test]
    fn the_takeover_budget_is_short_enough_to_wait_through() {
        assert!(TAKEOVER_BUDGET <= Duration::from_secs(5));
        assert!(RETRY < TAKEOVER_BUDGET);
    }

    /// The line the incumbent has to recognise. If this stops round-tripping,
    /// every takeover degrades into the three-second wait and an error.
    #[test]
    fn the_stand_down_message_is_a_shutdown_event() {
        let line = encode_line(&Envelope::Event {
            event: Event::Shutdown,
        })
        .unwrap();
        assert!(line.ends_with('\n'), "the wire format is line-delimited");
        assert!(matches!(
            atoll_core::protocol::decode_line(line.trim_end()).unwrap(),
            Envelope::Event {
                event: Event::Shutdown
            }
        ));
    }
}
