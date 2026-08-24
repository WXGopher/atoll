//! Naming for the named pipe that carries the [`crate::protocol`] wire format.
//!
//! Kept free of dependencies so `atoll-hook` can use it without pulling in the
//! server half of this crate.

/// Pipe name used when nothing overrides it: `\\.\pipe\atoll`.
pub const DEFAULT_PIPE_NAME: &str = "atoll";

/// Overrides [`DEFAULT_PIPE_NAME`]. Both the app and the hook read it, which is
/// what lets the integration tests run against a throwaway pipe.
pub const PIPE_NAME_ENV: &str = "ATOLL_PIPE_NAME";

/// Set to `1`/`true`/`yes` to make the hook exit immediately without connecting.
pub const SKIP_HOOKS_ENV: &str = "ATOLL_SKIP_HOOKS";

/// The configured pipe name, from [`PIPE_NAME_ENV`] or the default.
pub fn pipe_name() -> String {
    match std::env::var(PIPE_NAME_ENV) {
        Ok(name) if !name.trim().is_empty() => name,
        _ => DEFAULT_PIPE_NAME.to_string(),
    }
}

/// Expand a bare pipe name into the full `\\.\pipe\<name>` path.
///
/// A value that already looks like a full path is passed through, so
/// `ATOLL_PIPE_NAME` accepts either spelling.
pub fn pipe_path(name: &str) -> String {
    if name.starts_with(r"\\") {
        name.to_string()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

/// The full path Atoll listens on and the hook connects to.
pub fn configured_pipe_path() -> String {
    pipe_path(&pipe_name())
}

/// Whether [`SKIP_HOOKS_ENV`] asks the hook to do nothing at all.
pub fn hooks_disabled() -> bool {
    match std::env::var(SKIP_HOOKS_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_are_expanded() {
        assert_eq!(pipe_path("atoll"), r"\\.\pipe\atoll");
        assert_eq!(pipe_path("atoll-test-7"), r"\\.\pipe\atoll-test-7");
    }

    #[test]
    fn full_paths_pass_through() {
        assert_eq!(pipe_path(r"\\.\pipe\custom"), r"\\.\pipe\custom");
    }
}
