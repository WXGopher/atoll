//! What Atoll believes is happening in every live agent session.
//!
//! [`SessionState::apply`] is a reducer: it folds one hook payload into one
//! session's state and does nothing else — no clock, no filesystem, no network.
//! Wall-clock time arrives as the `now` argument, which is what makes the whole
//! state machine testable from a literal list of events.
//!
//! [`SessionTable`] is the fan-out: it routes payloads to sessions by
//! `session_id`, creates sessions on first sight, drops them on `SessionEnd`,
//! and ages out the ones whose agent went away without saying so.
//!
//! # Phases
//!
//! ```text
//! UserPromptSubmit / SessionStart ──────────────► Running
//! PreToolUse (tool_name == AskUserQuestion) ────► WaitingForAnswer
//! PreToolUse (anything else) ───────────────────► Running (activity only)
//! PermissionRequest (tool_name == AskUserQuestion) ► WaitingForAnswer
//! PermissionRequest (anything else) ────────────► WaitingForApproval
//! PostToolUse (clears the matching pending) ────► Running (once none are left)
//! a decision Atoll sent back ───────────────────► Running
//! Stop ─────────────────────────────────────────► Completed
//! ```
//!
//! # Why `PreToolUse` is not a request
//!
//! `PreToolUse` fires before **every** tool call, including the ones the user's
//! own permission settings already allow, and it fires before Claude Code has
//! decided whether a human needs to be asked at all. A session that treated it
//! as an approval request would raise a card for every `Read` and hold the
//! agent for the hook's whole 45-second budget waiting for someone to press a
//! button nobody knew was there.
//!
//! `PermissionRequest` is the event that means "a human is about to be asked".
//! Its hook budget is a day rather than 45 seconds, which is the protocol saying
//! the same thing.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::protocol::{HookPayload, HookSource, TerminalMeta, events};

/// How long an unanswered approval stays interesting. Past this the agent has
/// certainly moved on — its own hook timed out and it fell back to prompting in
/// the terminal — so a card for it would be a lie.
pub const PENDING_TTL_SECS: u64 = 180;

/// How long a session may go without an event before it is presumed dead.
/// Sessions that end cleanly send `SessionEnd`; this covers the ones that do
/// not, such as a terminal window closed mid-turn.
pub const STALE_AFTER_SECS: u64 = 15 * 60;

/// What a session is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The agent is working, or waiting on the user in its own terminal.
    Running,
    /// The agent is blocked on a tool approval Atoll can answer.
    WaitingForApproval,
    /// The agent asked the user a question (`AskUserQuestion`) whose options
    /// Atoll can render directly.
    WaitingForAnswer,
    /// The turn finished.
    Completed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::WaitingForApproval => "waitingForApproval",
            Phase::WaitingForAnswer => "waitingForAnswer",
            Phase::Completed => "completed",
        }
    }

    /// Whether the session is blocked on a human.
    pub fn is_waiting(self) -> bool {
        matches!(self, Phase::WaitingForApproval | Phase::WaitingForAnswer)
    }
}

/// The tool name Claude Code uses when it wants the user to pick an answer.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// An approval Atoll has seen and not yet resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    /// How this approval is matched to its `PostToolUse` or its decision. See
    /// [`correlation_key`].
    pub key: String,
    /// `PreToolUse` or `PermissionRequest`.
    pub event: String,
    pub tool_name: Option<String>,
    /// The agent's `tool_input`, kept whole so the UI can render whatever the
    /// tool happens to carry — including `AskUserQuestion`'s options.
    pub tool_input: Option<Value>,
    /// When the request arrived, in Unix seconds.
    pub requested_at: u64,
}

impl PendingApproval {
    /// `tool_input.questions` for an `AskUserQuestion`, so the card can render
    /// the options without reparsing the payload.
    pub fn questions(&self) -> Option<&Value> {
        self.tool_input.as_ref()?.get("questions")
    }

    /// Whether this approval is a question rather than a tool approval.
    pub fn is_question(&self) -> bool {
        self.tool_name.as_deref() == Some(ASK_USER_QUESTION)
    }

    pub fn is_expired(&self, now: u64, ttl_secs: u64) -> bool {
        now.saturating_sub(self.requested_at) >= ttl_secs
    }
}

/// One agent session, as reconstructed from its hook events.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    /// The agent's `session_id`; the table's primary key.
    pub session_id: String,
    /// Which agent this session belongs to.
    pub source: HookSource,
    pub phase: Phase,
    /// The session's working directory, from whichever payload last carried one.
    pub cwd: Option<String>,
    /// Path to the session's `.jsonl` transcript, for [`crate::transcript`].
    pub transcript_path: Option<String>,
    /// The name of the most recent event.
    pub last_event: String,
    /// When that event arrived, in Unix seconds.
    pub last_seen: u64,
    /// When the session was first seen, in Unix seconds.
    pub first_seen: u64,
    /// Terminal metadata the hook injected, for "jump back to the session".
    pub terminal: Option<TerminalMeta>,
    /// Unresolved approvals, oldest first.
    pub pending: Vec<PendingApproval>,
}

impl SessionState {
    /// A brand-new session, before any event has been folded in.
    pub fn new(session_id: impl Into<String>, source: HookSource, now: u64) -> Self {
        Self {
            session_id: session_id.into(),
            source,
            phase: Phase::Running,
            cwd: None,
            transcript_path: None,
            last_event: String::new(),
            last_seen: now,
            first_seen: now,
            terminal: None,
            pending: Vec::new(),
        }
    }

    /// Fold one hook payload into this session.
    ///
    /// Pure: the only inputs are `self`, `payload`, and `now`. `SessionEnd` is
    /// *not* handled here — removing a session is the table's job, and a
    /// reducer that could delete its own subject would be an awkward shape.
    pub fn apply(&mut self, payload: &HookPayload, now: u64) {
        let event = payload.event_name();

        // Identity fields ride along on every payload; take the freshest
        // non-empty value and never overwrite a known value with nothing.
        if let Some(cwd) = payload.cwd.as_deref().filter(|value| !value.is_empty()) {
            self.cwd = Some(cwd.to_string());
        }
        if let Some(path) = payload
            .transcript_path
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            self.transcript_path = Some(path.to_string());
        }
        if let Some(meta) = payload.terminal_meta() {
            self.terminal = Some(meta);
        }
        self.last_event = event.to_string();
        self.last_seen = now;

        match event {
            events::SESSION_START | events::USER_PROMPT_SUBMIT => {
                // A new turn invalidates anything left over from the last one.
                self.pending.clear();
                self.phase = Phase::Running;
            }
            // `PreToolUse` fires on *every* tool call — including the ones the
            // user's own allow-list already waved through — and it fires before
            // Claude Code has decided whether to ask anybody anything. It is an
            // activity signal, not a request, and treating it as one is how you
            // end up prompting for every `Read`.
            //
            // The one exception is `AskUserQuestion`, whose name alone says the
            // turn is about to stop for a human.
            events::PRE_TOOL_USE => {
                if payload.tool_name.as_deref() == Some(ASK_USER_QUESTION) {
                    self.push_pending(payload, now);
                    self.phase = Phase::WaitingForAnswer;
                } else if !self.phase.is_waiting() {
                    // A concurrent tool call must not clear a wait the user is
                    // still looking at.
                    self.phase = Phase::Running;
                }
            }
            // `PermissionRequest` is the event that means "Claude Code is about
            // to prompt a human". Its hook budget is a day, because that is how
            // long a human might take.
            events::PERMISSION_REQUEST => {
                self.push_pending(payload, now);
                self.phase = if payload.tool_name.as_deref() == Some(ASK_USER_QUESTION) {
                    Phase::WaitingForAnswer
                } else {
                    Phase::WaitingForApproval
                };
            }
            events::POST_TOOL_USE => {
                // The tool ran, so whatever gated it is settled — however it was
                // settled, and whether or not we ever saw the request. A
                // `PostToolUse` for a tool we have no pending for is normal
                // (Atoll started mid-session, or the user answered in the
                // terminal) and must not disturb the phase.
                self.resolve(&correlation_key(payload));
            }
            events::STOP => {
                self.pending.clear();
                self.phase = Phase::Completed;
            }
            // `Notification` and anything we do not model refresh `last_seen`
            // and nothing else.
            _ => {}
        }

        self.prune_pending(now, PENDING_TTL_SECS);
    }

    /// Record that `key` is no longer outstanding, and return to [`Phase::Running`]
    /// once nothing is. Call this when a decision goes back to the hook.
    ///
    /// Returns whether anything was actually removed.
    pub fn resolve(&mut self, key: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|approval| approval.key != key);
        let removed = self.pending.len() != before;

        // Only a resolution that emptied the queue ends the wait; a session with
        // two approvals outstanding is still waiting after the first one lands.
        if removed && self.pending.is_empty() && self.phase.is_waiting() {
            self.phase = Phase::Running;
        }
        removed
    }

    /// Resolve the oldest outstanding approval — what "the user tapped Allow on
    /// the card" means when the card did not carry a key.
    pub fn resolve_oldest(&mut self) -> Option<PendingApproval> {
        if self.pending.is_empty() {
            return None;
        }
        let approval = self.pending.remove(0);
        if self.pending.is_empty() && self.phase.is_waiting() {
            self.phase = Phase::Running;
        }
        Some(approval)
    }

    /// The approval a card should show: the oldest one still outstanding.
    pub fn current_pending(&self) -> Option<&PendingApproval> {
        self.pending.first()
    }

    /// The tool the session is blocked on, for a one-line "waiting for X".
    pub fn current_tool(&self) -> Option<&str> {
        self.current_pending()?.tool_name.as_deref()
    }

    /// Whether the agent has been silent long enough to presume it is gone.
    pub fn is_stale(&self, now: u64, stale_after_secs: u64) -> bool {
        now.saturating_sub(self.last_seen) >= stale_after_secs
    }

    /// Drop approvals older than `ttl_secs`, leaving the phase consistent.
    pub fn prune_pending(&mut self, now: u64, ttl_secs: u64) {
        let before = self.pending.len();
        self.pending
            .retain(|approval| !approval.is_expired(now, ttl_secs));
        if self.pending.len() != before && self.pending.is_empty() && self.phase.is_waiting() {
            // The agent's own hook timed out and it prompted in the terminal.
            self.phase = Phase::Running;
        }
    }

    fn push_pending(&mut self, payload: &HookPayload, now: u64) {
        let key = correlation_key(payload);
        // A retry of the same request replaces it rather than stacking up.
        self.pending.retain(|approval| approval.key != key);
        self.pending.push(PendingApproval {
            key,
            event: payload.event_name().to_string(),
            tool_name: payload.tool_name.clone(),
            tool_input: payload.tool_input.clone(),
            requested_at: now,
        });
    }
}

/// How a `PreToolUse` is matched to the `PostToolUse` that settles it.
///
/// Claude Code sends a `tool_use_id` on recent builds; older ones do not. The
/// fallback is the tool name, which is right whenever a session does not have
/// two approvals outstanding for the same tool at once — and when it is wrong,
/// the cost is one card clearing early, not a stuck session.
pub fn correlation_key(payload: &HookPayload) -> String {
    for field in [
        "tool_use_id",
        "toolUseId",
        "permission_request_id",
        "permissionRequestId",
        "request_id",
    ] {
        if let Some(id) = payload
            .extra
            .get(field)
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return id.to_string();
        }
    }
    payload
        .tool_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("{}:-", payload.event_name()))
}

/// A snapshot of how many sessions are in each phase.
///
/// Cheap to compare, which is how the headless log decides whether the table
/// changed in a way worth printing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableCounts {
    pub total: usize,
    pub running: usize,
    pub waiting: usize,
    pub completed: usize,
    pub stale: usize,
}

/// Every session Atoll is currently tracking, keyed by `session_id`.
#[derive(Debug, Clone)]
pub struct SessionTable {
    sessions: BTreeMap<String, SessionState>,
    stale_after_secs: u64,
}

impl Default for SessionTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTable {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            stale_after_secs: STALE_AFTER_SECS,
        }
    }

    /// Override how long a silent session survives. Mostly for tests.
    pub fn with_stale_after(mut self, seconds: u64) -> Self {
        self.stale_after_secs = seconds;
        self
    }

    /// Route one payload to its session, creating or removing it as the event
    /// requires. Returns the session id the payload was filed under, or `None`
    /// for a payload with no `session_id` (which we cannot attribute) or a
    /// `SessionEnd` that removed its session.
    pub fn apply(&mut self, payload: &HookPayload, source: HookSource, now: u64) -> Option<String> {
        let session_id = payload
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())?
            .to_string();

        if payload.event_name() == events::SESSION_END {
            self.sessions.remove(&session_id);
            self.sweep(now);
            return None;
        }

        let state = self
            .sessions
            .entry(session_id.clone())
            .or_insert_with(|| SessionState::new(session_id.clone(), source, now));
        state.apply(payload, now);

        self.sweep(now);
        Some(session_id)
    }

    /// Drop sessions that have gone quiet past the stale threshold, and expire
    /// approvals inside the ones that survive. Safe to call as often as you like.
    pub fn sweep(&mut self, now: u64) {
        let stale_after = self.stale_after_secs;
        self.sessions
            .retain(|_, state| !state.is_stale(now, stale_after));
        for state in self.sessions.values_mut() {
            state.prune_pending(now, PENDING_TTL_SECS);
        }
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionState> {
        self.sessions.get(session_id)
    }

    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut SessionState> {
        self.sessions.get_mut(session_id)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Every tracked session, ordered by session id so the output is stable.
    pub fn sessions(&self) -> impl Iterator<Item = &SessionState> {
        self.sessions.values()
    }

    /// Sessions blocked on a human, oldest request first — the order a UI
    /// should work through them in.
    pub fn waiting(&self) -> Vec<&SessionState> {
        let mut waiting: Vec<&SessionState> = self
            .sessions
            .values()
            .filter(|state| state.phase.is_waiting())
            .collect();
        waiting.sort_by_key(|state| {
            state
                .current_pending()
                .map(|approval| approval.requested_at)
                .unwrap_or(state.last_seen)
        });
        waiting
    }

    pub fn counts(&self, now: u64) -> TableCounts {
        let mut counts = TableCounts {
            total: self.sessions.len(),
            ..TableCounts::default()
        };
        for state in self.sessions.values() {
            match state.phase {
                Phase::Running => counts.running += 1,
                Phase::WaitingForApproval | Phase::WaitingForAnswer => counts.waiting += 1,
                Phase::Completed => counts.completed += 1,
            }
            if state.is_stale(now, self.stale_after_secs) {
                counts.stale += 1;
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Baseline for the synthetic clock; any constant works, this one just reads
    /// like a plausible instant rather than 0.
    const T0: u64 = 1_787_000_000;

    fn payload(raw: Value) -> HookPayload {
        serde_json::from_value(raw).expect("synthetic payload")
    }

    fn event(name: &str) -> HookPayload {
        payload(json!({
            "hook_event_name": name,
            "session_id": "s-1",
            "cwd": r"C:\synthetic\project",
            "transcript_path": r"C:\synthetic\t.jsonl",
        }))
    }

    fn pre_tool_use(tool: &str, id: Option<&str>) -> HookPayload {
        let mut raw = json!({
            "hook_event_name": events::PRE_TOOL_USE,
            "session_id": "s-1",
            "cwd": r"C:\synthetic\project",
            "tool_name": tool,
            "tool_input": {"command": "git status"},
        });
        if let Some(id) = id {
            raw["tool_use_id"] = json!(id);
        }
        payload(raw)
    }

    /// The event that actually asks a human: same shape as a `PreToolUse`, and
    /// the one the cards and the waiting phases are driven from.
    fn permission_request(tool: &str, id: Option<&str>) -> HookPayload {
        let mut payload = pre_tool_use(tool, id);
        payload.hook_event_name = Some(events::PERMISSION_REQUEST.to_string());
        payload
    }

    fn post_tool_use(tool: &str, id: Option<&str>) -> HookPayload {
        let mut raw = json!({
            "hook_event_name": events::POST_TOOL_USE,
            "session_id": "s-1",
            "tool_name": tool,
        });
        if let Some(id) = id {
            raw["tool_use_id"] = json!(id);
        }
        payload(raw)
    }

    /// Fold a whole sequence into one session and hand back the result.
    fn run(events: &[(HookPayload, u64)]) -> SessionState {
        let mut state = SessionState::new("s-1", HookSource::Claude, T0);
        for (payload, now) in events {
            state.apply(payload, *now);
        }
        state
    }

    #[test]
    fn a_fresh_session_is_running() {
        let state = run(&[(event(events::SESSION_START), T0)]);
        assert_eq!(state.phase, Phase::Running);
        assert_eq!(state.cwd.as_deref(), Some(r"C:\synthetic\project"));
        assert_eq!(
            state.transcript_path.as_deref(),
            Some(r"C:\synthetic\t.jsonl")
        );
        assert!(state.pending.is_empty());
    }

    /// The defect this guards against: `PreToolUse` fires for every tool call,
    /// including everything the user's own settings already allow. Treating it
    /// as a request raises a card for each one and stalls the agent for the
    /// hook's whole budget waiting on a button.
    #[test]
    fn pre_tool_use_is_activity_and_not_a_request() {
        let state = run(&[
            (event(events::USER_PROMPT_SUBMIT), T0),
            (pre_tool_use("Read", Some("tu-1")), T0 + 1),
        ]);
        assert_eq!(state.phase, Phase::Running);
        assert!(
            state.pending.is_empty(),
            "a PreToolUse must not leave an approval for anybody to answer"
        );
        assert_eq!(state.last_event, events::PRE_TOOL_USE);
        assert_eq!(state.last_seen, T0 + 1, "but it is still a sign of life");
    }

    #[test]
    fn a_pre_tool_use_does_not_clear_a_wait_the_user_is_looking_at() {
        // A second tool call, concurrent with an approval still on screen, must
        // not quietly take the card down.
        let state = run(&[
            (permission_request("Write", Some("tu-1")), T0),
            (pre_tool_use("Read", Some("tu-2")), T0 + 1),
        ]);
        assert_eq!(state.phase, Phase::WaitingForApproval);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_pending().unwrap().key, "tu-1");
    }

    #[test]
    fn permission_request_waits_for_approval_and_post_tool_use_releases_it() {
        let state = run(&[
            (event(events::USER_PROMPT_SUBMIT), T0),
            (permission_request("Bash", Some("tu-1")), T0 + 1),
        ]);
        assert_eq!(state.phase, Phase::WaitingForApproval);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_pending().unwrap().key, "tu-1");
        assert_eq!(
            state.current_pending().unwrap().event,
            events::PERMISSION_REQUEST
        );
        assert_eq!(state.current_tool(), Some("Bash"));

        let mut state = state;
        state.apply(&post_tool_use("Bash", Some("tu-1")), T0 + 2);
        assert_eq!(state.phase, Phase::Running);
        assert!(state.pending.is_empty());
        assert_eq!(state.current_tool(), None);
    }

    #[test]
    fn a_permission_request_keeps_the_whole_tool_input() {
        let state = run(&[(
            payload(json!({
                "hook_event_name": events::PERMISSION_REQUEST,
                "session_id": "s-1",
                "tool_name": "Write",
                "tool_input": {"file_path": r"C:\synthetic\a.txt"},
            })),
            T0,
        )]);
        assert_eq!(state.phase, Phase::WaitingForApproval);
        let pending = state.current_pending().unwrap();
        assert_eq!(pending.event, events::PERMISSION_REQUEST);
        assert_eq!(
            pending.tool_input.as_ref().unwrap()["file_path"],
            json!(r"C:\synthetic\a.txt")
        );
    }

    #[test]
    fn ask_user_question_waits_for_an_answer_and_keeps_its_options() {
        let questions = json!([{
            "question": "Which database?",
            "options": [{"label": "Postgres"}, {"label": "SQLite"}],
        }]);
        let state = run(&[(
            payload(json!({
                "hook_event_name": events::PRE_TOOL_USE,
                "session_id": "s-1",
                "tool_name": ASK_USER_QUESTION,
                "tool_input": {"questions": questions},
            })),
            T0,
        )]);

        assert_eq!(state.phase, Phase::WaitingForAnswer);
        let approval = state.current_pending().unwrap();
        assert!(approval.is_question());
        assert_eq!(approval.questions().unwrap(), &questions);
    }

    /// The card itself is raised by the `PermissionRequest` that follows, which
    /// is the one Atoll can hold open for as long as the user needs.
    #[test]
    fn a_question_reaching_us_twice_settles_as_one_pending() {
        let tool_input = json!({
            "questions": [{"question": "Which database?", "options": [{"label": "Postgres"}]}],
        });
        let raw = |name: &str| {
            payload(json!({
                "hook_event_name": name,
                "session_id": "s-1",
                "tool_name": ASK_USER_QUESTION,
                "tool_use_id": "tu-q",
                "tool_input": tool_input,
            }))
        };
        let state = run(&[
            (raw(events::PRE_TOOL_USE), T0),
            (raw(events::PERMISSION_REQUEST), T0 + 1),
        ]);

        assert_eq!(state.phase, Phase::WaitingForAnswer);
        assert_eq!(state.pending.len(), 1, "the same request, not two");
        let approval = state.current_pending().unwrap();
        assert_eq!(approval.event, events::PERMISSION_REQUEST);
        assert!(approval.is_question());
    }

    #[test]
    fn stop_completes_the_turn_and_clears_pending() {
        let state = run(&[
            (permission_request("Bash", Some("tu-1")), T0),
            (event(events::STOP), T0 + 1),
        ]);
        assert_eq!(state.phase, Phase::Completed);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn a_new_prompt_restarts_a_completed_session() {
        let state = run(&[
            (event(events::STOP), T0),
            (event(events::USER_PROMPT_SUBMIT), T0 + 5),
        ]);
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn a_sent_decision_returns_the_session_to_running() {
        let mut state = run(&[(permission_request("Bash", Some("tu-1")), T0)]);
        assert!(state.resolve("tu-1"));
        assert_eq!(state.phase, Phase::Running);
        assert!(!state.resolve("tu-1"), "resolving twice is a no-op");
    }

    #[test]
    fn resolving_one_of_two_keeps_the_session_waiting() {
        let mut state = run(&[
            (permission_request("Bash", Some("tu-1")), T0),
            (permission_request("Read", Some("tu-2")), T0 + 1),
        ]);
        assert_eq!(state.pending.len(), 2);

        state.resolve("tu-1");
        assert_eq!(state.phase, Phase::WaitingForApproval);
        assert_eq!(state.pending.len(), 1);

        state.resolve("tu-2");
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn resolve_oldest_takes_them_in_order() {
        let mut state = run(&[
            (permission_request("Bash", Some("tu-1")), T0),
            (permission_request("Read", Some("tu-2")), T0 + 1),
        ]);
        assert_eq!(state.resolve_oldest().unwrap().key, "tu-1");
        assert_eq!(state.resolve_oldest().unwrap().key, "tu-2");
        assert!(state.resolve_oldest().is_none());
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn an_out_of_order_post_tool_use_is_ignored() {
        // PostToolUse for a tool we never saw approved — Atoll started
        // mid-session. It must not clear the approval we *are* tracking.
        let mut state = run(&[(permission_request("Bash", Some("tu-1")), T0)]);
        state.apply(&post_tool_use("Read", Some("tu-9")), T0 + 1);

        assert_eq!(state.phase, Phase::WaitingForApproval);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_pending().unwrap().key, "tu-1");
    }

    #[test]
    fn a_stray_post_tool_use_on_a_running_session_changes_nothing() {
        let mut state = run(&[(event(events::USER_PROMPT_SUBMIT), T0)]);
        state.apply(&post_tool_use("Bash", Some("tu-1")), T0 + 1);
        assert_eq!(state.phase, Phase::Running);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn stale_pending_approvals_expire() {
        let mut state = run(&[(permission_request("Bash", Some("tu-1")), T0)]);
        assert_eq!(state.phase, Phase::WaitingForApproval);

        // One second short of the TTL: still outstanding.
        state.prune_pending(T0 + PENDING_TTL_SECS - 1, PENDING_TTL_SECS);
        assert_eq!(state.pending.len(), 1);

        // At the TTL the agent has long since fallen back to its own prompt.
        state.prune_pending(T0 + PENDING_TTL_SECS, PENDING_TTL_SECS);
        assert!(state.pending.is_empty());
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn applying_a_later_event_also_expires_stale_pending() {
        let state = run(&[
            (permission_request("Bash", Some("tu-1")), T0),
            (event(events::NOTIFICATION), T0 + PENDING_TTL_SECS + 1),
        ]);
        assert!(state.pending.is_empty());
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn a_repeated_request_replaces_rather_than_stacks() {
        let state = run(&[
            (permission_request("Bash", Some("tu-1")), T0),
            (permission_request("Bash", Some("tu-1")), T0 + 1),
        ]);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_pending().unwrap().requested_at, T0 + 1);
    }

    #[test]
    fn correlation_falls_back_to_the_tool_name_without_an_id() {
        let state = run(&[(permission_request("Bash", None), T0)]);
        assert_eq!(state.current_pending().unwrap().key, "Bash");

        let mut state = state;
        state.apply(&post_tool_use("Bash", None), T0 + 1);
        assert!(state.pending.is_empty());
        assert_eq!(state.phase, Phase::Running);
    }

    #[test]
    fn correlation_prefers_an_explicit_id_over_the_tool_name() {
        assert_eq!(correlation_key(&pre_tool_use("Bash", Some("tu-7"))), "tu-7");
        assert_eq!(correlation_key(&pre_tool_use("Bash", None)), "Bash");
        assert_eq!(
            correlation_key(&payload(json!({"hook_event_name": "Stop"}))),
            "Stop:-"
        );
    }

    #[test]
    fn identity_fields_are_never_overwritten_with_nothing() {
        let state = run(&[
            (event(events::SESSION_START), T0),
            // A later payload with no cwd must not erase the one we have.
            (
                payload(json!({"hook_event_name": "Notification", "session_id": "s-1"})),
                T0 + 1,
            ),
        ]);
        assert_eq!(state.cwd.as_deref(), Some(r"C:\synthetic\project"));
        assert_eq!(
            state.transcript_path.as_deref(),
            Some(r"C:\synthetic\t.jsonl")
        );
    }

    #[test]
    fn terminal_metadata_is_captured_from_the_payload() {
        let mut raw = json!({"hook_event_name": "SessionStart", "session_id": "s-1"});
        raw[crate::protocol::TERMINAL_META_KEY] = json!({
            "env": {"WT_SESSION": "synthetic-guid"},
            "hookPid": 4242,
        });
        let state = run(&[(payload(raw), T0)]);

        let terminal = state.terminal.expect("terminal metadata");
        assert_eq!(terminal.hook_pid, 4242);
        assert_eq!(terminal.env["WT_SESSION"], json!("synthetic-guid"));
    }

    // ------------------------------------------------------------ the table

    fn table_payload(session: &str, name: &str) -> HookPayload {
        payload(json!({
            "hook_event_name": name,
            "session_id": session,
            "cwd": r"C:\synthetic\project",
        }))
    }

    #[test]
    fn the_table_creates_a_session_on_first_sight() {
        let mut table = SessionTable::new();
        let id = table
            .apply(
                &table_payload("s-a", events::SESSION_START),
                HookSource::Claude,
                T0,
            )
            .unwrap();

        assert_eq!(id, "s-a");
        assert_eq!(table.len(), 1);
        assert_eq!(table.get("s-a").unwrap().source, HookSource::Claude);
    }

    #[test]
    fn the_table_ignores_a_payload_with_no_session_id() {
        let mut table = SessionTable::new();
        let orphan = payload(json!({"hook_event_name": "Stop"}));
        assert!(table.apply(&orphan, HookSource::Claude, T0).is_none());
        assert!(table.is_empty());
    }

    #[test]
    fn session_end_removes_the_session() {
        let mut table = SessionTable::new();
        table.apply(
            &table_payload("s-a", events::SESSION_START),
            HookSource::Claude,
            T0,
        );
        assert_eq!(table.len(), 1);

        assert!(
            table
                .apply(
                    &table_payload("s-a", events::SESSION_END),
                    HookSource::Claude,
                    T0 + 1
                )
                .is_none()
        );
        assert!(table.is_empty());
    }

    #[test]
    fn silent_sessions_age_out() {
        let mut table = SessionTable::new().with_stale_after(60);
        table.apply(
            &table_payload("s-a", events::SESSION_START),
            HookSource::Claude,
            T0,
        );
        table.apply(
            &table_payload("s-b", events::SESSION_START),
            HookSource::Claude,
            T0 + 50,
        );

        // At T0 + 61, s-a has been silent for 61 s and s-b for 11 s.
        table.sweep(T0 + 61);
        assert_eq!(table.len(), 1);
        assert!(table.get("s-b").is_some());
    }

    #[test]
    fn counts_split_sessions_by_phase() {
        let mut table = SessionTable::new();
        table.apply(
            &table_payload("s-a", events::USER_PROMPT_SUBMIT),
            HookSource::Claude,
            T0,
        );
        table.apply(&table_payload("s-b", events::STOP), HookSource::Claude, T0);

        let mut waiting = permission_request("Bash", Some("tu-1"));
        waiting.session_id = Some("s-c".into());
        table.apply(&waiting, HookSource::Codex, T0);

        let counts = table.counts(T0);
        assert_eq!(counts.total, 3);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.waiting, 1);
        assert_eq!(counts.stale, 0);
    }

    #[test]
    fn waiting_sessions_come_back_oldest_request_first() {
        let mut table = SessionTable::new();

        let mut first = permission_request("Bash", Some("tu-1"));
        first.session_id = Some("s-z".into());
        table.apply(&first, HookSource::Claude, T0);

        let mut second = permission_request("Read", Some("tu-2"));
        second.session_id = Some("s-a".into());
        table.apply(&second, HookSource::Claude, T0 + 5);

        let waiting = table.waiting();
        assert_eq!(waiting.len(), 2);
        // Sorted by request time, not by the id the BTreeMap orders on.
        assert_eq!(waiting[0].session_id, "s-z");
        assert_eq!(waiting[1].session_id, "s-a");
    }

    #[test]
    fn two_sessions_do_not_share_pending_approvals() {
        let mut table = SessionTable::new();

        let mut a = permission_request("Bash", Some("tu-1"));
        a.session_id = Some("s-a".into());
        table.apply(&a, HookSource::Claude, T0);

        let mut b = post_tool_use("Bash", Some("tu-1"));
        b.session_id = Some("s-b".into());
        table.apply(&b, HookSource::Claude, T0 + 1);

        assert_eq!(table.get("s-a").unwrap().phase, Phase::WaitingForApproval);
        assert_eq!(table.get("s-b").unwrap().phase, Phase::Running);
    }
}
