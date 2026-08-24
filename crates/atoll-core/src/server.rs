//! The named-pipe server the Atoll app runs: accepts hook connections, hands
//! each decoded [`Envelope`] to a handler, and lets the handler reply later.
//!
//! # Why connections stay open
//!
//! A hook that fired `PreToolUse` or `PermissionRequest` is blocked on our
//! reply. The handler therefore receives a [`ConnectionHandle`] it may clone and
//! park — in M3 that means "until the user taps Allow" — and the connection
//! lives until either side hangs up.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::pipe;
use crate::protocol::{Envelope, encode_line};

/// A write handle for one accepted connection.
///
/// Cheap to clone and safe to keep past the handler call: sending on a closed
/// connection returns an error rather than panicking.
#[derive(Clone, Debug)]
pub struct ConnectionHandle {
    id: u64,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ConnectionHandle {
    /// Monotonic id, useful for correlating log lines.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the peer is (as far as we know) still connected.
    pub fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Queue one newline-terminated envelope. Returns once queued, not once
    /// written.
    pub fn send(&self, envelope: &Envelope) -> io::Result<()> {
        let line = encode_line(envelope).map_err(io::Error::other)?;
        self.tx
            .send(line.into_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }
}

/// What a handler does with each envelope. Called once per received line, from
/// a tokio task, so it must not block for long — park the
/// [`ConnectionHandle`] instead.
pub trait Handler: Send + Sync + 'static {
    fn on_envelope(&self, envelope: Envelope, connection: ConnectionHandle);

    /// A line arrived that was not valid protocol JSON. Default: ignore.
    fn on_decode_error(&self, _line: &str, _error: serde_json::Error) {}
}

impl<F> Handler for F
where
    F: Fn(Envelope, ConnectionHandle) + Send + Sync + 'static,
{
    fn on_envelope(&self, envelope: Envelope, connection: ConnectionHandle) {
        self(envelope, connection)
    }
}

/// A bound pipe, before it starts accepting.
///
/// Binding is separate from serving so callers can report the real path (and
/// fail fast on "another Atoll is already running") before entering the loop.
pub struct PipeServer {
    path: String,
    listener: NamedPipeServer,
}

impl PipeServer {
    /// Claim `\\.\pipe\<pipe_name>`.
    ///
    /// Fails with [`io::ErrorKind::AlreadyExists`] if another process already
    /// owns the name — `first_pipe_instance` makes a second Atoll a loud error
    /// instead of a silent split-brain.
    ///
    /// Must be called inside a tokio runtime.
    ///
    /// TODO(M2): set an explicit DACL granting only the current user. Tokio's
    /// `ServerOptions` can do this through
    /// `create_with_security_attributes_ptr`, which needs a SECURITY_ATTRIBUTES
    /// built from the process token's SID (`windows-sys` + unsafe). Until then
    /// the pipe carries the default descriptor, which lets other local accounts
    /// open a client handle and inject synthetic events. It does not let them
    /// impersonate the server or forge approvals, so this is noise-injection
    /// rather than privilege escalation.
    pub fn bind(pipe_name: &str) -> io::Result<Self> {
        let path = pipe::pipe_path(pipe_name);
        let listener = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)?;
        Ok(Self { path, listener })
    }

    /// Bind the pipe named by `ATOLL_PIPE_NAME`, or the default.
    pub fn bind_configured() -> io::Result<Self> {
        Self::bind(&pipe::pipe_name())
    }

    /// The full `\\.\pipe\...` path this server listens on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Accept forever, spawning a task per connection. Only returns on an
    /// accept-loop error.
    pub async fn serve<H: Handler>(self, handler: H) -> io::Result<()> {
        let handler = Arc::new(handler);
        let next_id = AtomicU64::new(1);
        let mut listener = self.listener;

        loop {
            listener.connect().await?;
            let connected = listener;
            // Immediately stand up the next instance so a hook that fires while
            // we are busy finds a pipe to open instead of ERROR_FILE_NOT_FOUND.
            listener = ServerOptions::new().create(&self.path)?;

            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                serve_connection(connected, id, handler).await;
            });
        }
    }
}

async fn serve_connection<H: Handler>(connection: NamedPipeServer, id: u64, handler: Arc<H>) {
    let (reader, mut writer) = tokio::io::split(connection);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let write_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let handle = ConnectionHandle { id, tx };
    let mut lines = BufReader::new(reader).lines();

    // The loop ends on clean EOF or on the peer hanging up — a broken pipe is
    // the normal way a hook that got its answer says goodbye.
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match crate::protocol::decode_line(&line) {
            Ok(envelope) => handler.on_envelope(envelope, handle.clone()),
            Err(error) => handler.on_decode_error(&line, error),
        }
    }

    // The peer is gone, so nothing more can be delivered. Dropping our handle is
    // not enough — the handler may still hold clones — so stop the writer
    // outright. Their `send` calls now fail, which is how a parked handler
    // learns the hook gave up.
    drop(handle);
    write_task.abort();
}
