//! Route a session to the client that owns it, with no cross-client fallback.

use atoll_core::protocol::{HookSource, ProcessRef};
use atoll_core::state::{CodexClient, SessionState};

use super::win;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Terminal,
    Desktop,
    Unavailable,
}

impl Route {
    fn open(self, terminal: impl FnOnce() -> bool, desktop: impl FnOnce() -> bool) -> bool {
        match self {
            Self::Terminal => terminal(),
            Self::Desktop => desktop(),
            Self::Unavailable => false,
        }
    }
}

fn route(state: &SessionState, ancestors: &[ProcessRef], captured: bool) -> Route {
    if state.source != HookSource::Codex {
        return if ancestors.is_empty() {
            Route::Unavailable
        } else {
            Route::Terminal
        };
    }
    if captured || ancestors.iter().any(|p| p.exe == "windowsterminal.exe") {
        Route::Terminal
    } else if ancestors.iter().any(|p| p.exe == "chatgpt.exe") {
        Route::Desktop
    } else {
        match state.codex_client {
            CodexClient::Cli => Route::Terminal,
            CodexClient::Desktop => Route::Desktop,
            CodexClient::Unknown if !ancestors.is_empty() => Route::Terminal,
            CodexClient::Unknown => Route::Unavailable,
        }
    }
}

pub fn can_jump(state: &SessionState, desktop_available: bool) -> bool {
    let ancestors = state
        .terminal
        .as_ref()
        .map(|meta| meta.ancestors.as_slice())
        .unwrap_or_default();
    let captured = state
        .terminal
        .as_ref()
        .and_then(win::target::from_meta)
        .is_some();
    match route(state, ancestors, captured) {
        Route::Desktop => desktop_available && win::codex::thread_uri(&state.session_id).is_some(),
        Route::Terminal => captured || !ancestors.is_empty() || state.transcript_path.is_some(),
        Route::Unavailable => state.source == HookSource::Codex && state.transcript_path.is_some(),
    }
}

pub struct Plan {
    route: Route,
    target: Option<win::target::Target>,
    ancestors: Vec<ProcessRef>,
    owner: Option<win::file_owner::Owner>,
}

impl Plan {
    /// File ownership is queried on a worker, never while rendering the panel.
    pub fn resolve(state: &SessionState) -> Self {
        let target = state.terminal.as_ref().and_then(win::target::from_meta);
        let owner = (state.source == HookSource::Codex && target.is_none())
            .then(|| {
                state
                    .transcript_path
                    .as_deref()
                    .and_then(|path| win::file_owner::codex(path.as_ref()))
            })
            .flatten();
        let ancestors = owner
            .as_ref()
            .map(|owner| owner.ancestors.clone())
            .or_else(|| state.terminal.as_ref().map(|meta| meta.ancestors.clone()))
            .unwrap_or_default();
        Self {
            route: route(state, &ancestors, target.is_some()),
            target,
            ancestors,
            owner,
        }
    }

    pub fn activate(&self, session_id: &str, hint: Option<&str>) -> bool {
        if self.owner.as_ref().is_some_and(|owner| !owner.is_alive()) {
            return false;
        }
        let chain: Vec<_> = self
            .ancestors
            .iter()
            .map(|p| format!("{}:{}", p.pid, p.exe))
            .collect();
        crate::util::debug_log(&format!(
            "jump {session_id}: {:?}; chain {}",
            self.route,
            chain.join(" <- ")
        ));
        self.route.open(
            || match &self.target {
                Some(target) => {
                    win::target::activate(target) || win::target::activate_window(target)
                }
                None => win::activate_terminal_from(&self.ancestors, hint),
            },
            || win::codex::open_thread(session_id),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_with_missing_or_closed_terminal_never_opens_the_desktop() {
        let mut state =
            SessionState::new("019d4531-4232-70fc-bcab-0123456789ab", HookSource::Codex, 1);
        state.codex_client = CodexClient::Cli;
        for ancestors in [
            vec![],
            vec![ProcessRef {
                pid: 42,
                exe: "codex.exe".into(),
            }],
        ] {
            let choice = route(&state, &ancestors, false);
            assert_eq!(choice, Route::Terminal);
            assert!(!choice.open(|| false, || panic!("CLI failure must not open the App")));
            assert!(choice.open(|| true, || panic!("CLI success must not open the App")));
        }
    }

    #[test]
    fn desktop_requires_client_evidence_and_a_captured_terminal_takes_priority() {
        let mut state = SessionState::new("s", HookSource::Codex, 1);
        assert_eq!(route(&state, &[], false), Route::Unavailable);
        state.codex_client = CodexClient::Desktop;
        assert_eq!(route(&state, &[], false), Route::Desktop);
        assert!(Route::Desktop.open(|| panic!("desktop should open its conversation"), || true));
        assert_eq!(route(&state, &[], true), Route::Terminal);
        state.codex_client = CodexClient::Cli;
        let desktop = [ProcessRef {
            pid: 43,
            exe: "chatgpt.exe".into(),
        }];
        assert_eq!(
            route(&state, &desktop, false),
            Route::Desktop,
            "live owner can resume an old CLI conversation in the App"
        );
    }

    #[test]
    #[ignore = "uses an existing live Codex CLI session and focuses its terminal"]
    fn native_plain_cli_returns_to_its_own_terminal() {
        let home = atoll_core::install::codex_home().unwrap();
        let id = std::env::var("ATOLL_CLI_TEST_SESSION").expect("live CLI session id");
        let expected: u32 = std::env::var("ATOLL_CLI_TEST_PID")
            .unwrap()
            .parse()
            .unwrap();
        let sessions = atoll_core::codex::SessionCache::default()
            .scan(&home, atoll_core::now_unix_secs())
            .unwrap();
        let state = sessions
            .iter()
            .find(|state| state.session_id == id)
            .unwrap();
        assert_eq!(state.codex_client, CodexClient::Cli);
        assert!(
            state.terminal.is_none(),
            "test the path without hook metadata"
        );
        let plan = Plan::resolve(state);
        assert_eq!(plan.route, Route::Terminal);
        assert_eq!(plan.ancestors.first().unwrap().pid, expected);
        assert!(plan.activate(&id, None));
        assert!(win::terminal_is_foreground(&plan.ancestors));
    }
}
