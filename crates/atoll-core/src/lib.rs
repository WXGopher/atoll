//! Shared types and logic for Atoll.
//!
//! The hook binary and the desktop app both depend on this crate so that the
//! wire format, on-disk state, and installation layout stay in sync.
//!
//! `atoll-hook` disables the default `server` feature, so everything outside
//! [`server`] must stay free of heavyweight dependencies.

pub mod install;
pub mod pipe;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;
pub mod state;
pub mod transcript;
pub mod usage;

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, or `0` if the clock is before it.
///
/// Every module that needs wall-clock time takes it as an argument instead of
/// reading it, so the reducers and parsers below stay deterministic under test.
/// This is the one place that actually looks at the clock.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
