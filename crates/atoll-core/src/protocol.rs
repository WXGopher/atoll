//! Wire format shared between the hook binary and the Atoll app: hook payloads, approval requests, and decisions.
//!
//! # Transport
//!
//! Newline-delimited JSON over the named pipe `\\.\pipe\atoll` (see
//! [`crate::pipe`]). Every line is one [`Envelope`]. There is no request id:
//! one connection carries one request, and the reply — if the event needs one —
//! comes back on the same connection.
//!
//! # Hook payload passthrough
//!
//! Hook payloads are forwarded *verbatim*. [`HookPayload`] strongly types only
//! the fields Atoll actually reads and captures everything else in
//! [`HookPayload::extra`], so unknown or future fields survive a round trip.
//!
//! # Decision output
//!
//! Decisions the hook prints on stdout are built as [`serde_json::Value`]
//! objects rather than structs. `serde_json::Map` is a `BTreeMap` by default,
//! so this gives byte-stable, key-sorted output; the unit tests below lock the
//! exact bytes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which agent produced a hook payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookSource {
    Claude,
    Codex,
}

impl HookSource {
    pub fn as_str(self) -> &'static str {
        match self {
            HookSource::Claude => "claude",
            HookSource::Codex => "codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(HookSource::Claude),
            "codex" => Some(HookSource::Codex),
            _ => None,
        }
    }
}

/// Top-level frame. One per line on the pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Envelope {
    /// Sent by a peer right after connecting to identify itself.
    Hello { hello: Hello },
    /// A fire-and-forget notification that needs no reply.
    Event { event: Event },
    /// A request. The sender keeps the connection open if it wants a response.
    Command { command: Command },
    /// A reply to a [`Envelope::Command`] on the same connection.
    Response { response: Response },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    /// The app is going away; connected peers should stop waiting.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Command {
    /// A hook fired. `claude_hook` is the agent's stdin JSON plus Atoll's
    /// injected terminal metadata.
    #[serde(rename_all = "camelCase")]
    ProcessClaudeHook {
        claude_hook: HookPayload,
        source: HookSource,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Response {
    /// Received, nothing further expected.
    Ack,
    /// The user's (or the app's) answer to a blocking hook.
    Decision { decision: HookDecision },
    /// The app could not produce a decision; the hook fails open.
    Error { message: String },
}

/// Key under which Atoll injects terminal metadata into a hook payload.
///
/// Nested under a single namespaced key so the passthrough payload keeps
/// exactly the agent's own top-level shape plus one clearly-ours addition.
pub const TERMINAL_META_KEY: &str = "atollTerminal";

/// Environment variables the hook forwards, when set, so the app can jump back
/// to the terminal or editor that owns the session.
pub const TERMINAL_ENV_VARS: &[&str] = &[
    "ConEmuPID",
    "SESSIONNAME",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "VSCODE_GIT_ASKPASS_MAIN",
    "VSCODE_GIT_IPC_HANDLE",
    "VSCODE_INJECTION",
    "VSCODE_PID",
    "WT_PROFILE_ID",
    "WT_SESSION",
];

/// A hook's stdin payload: typed where Atoll reads it, verbatim everywhere else.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Every other key, preserved as-is.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl HookPayload {
    /// The event name, or `""` when the agent did not send one.
    pub fn event_name(&self) -> &str {
        self.hook_event_name.as_deref().unwrap_or_default()
    }

    /// Whether this event makes the agent wait for a decision on stdout.
    pub fn is_blocking(&self) -> bool {
        matches!(
            self.event_name(),
            events::PRE_TOOL_USE | events::PERMISSION_REQUEST
        )
    }

    /// Attach terminal metadata under [`TERMINAL_META_KEY`].
    pub fn set_terminal_meta(&mut self, meta: TerminalMeta) {
        let value = serde_json::to_value(meta).unwrap_or(Value::Null);
        self.extra.insert(TERMINAL_META_KEY.to_string(), value);
    }

    /// Read back terminal metadata, if the hook injected any.
    pub fn terminal_meta(&self) -> Option<TerminalMeta> {
        let raw = self.extra.get(TERMINAL_META_KEY)?;
        serde_json::from_value(raw.clone()).ok()
    }
}

/// One process in the hook's ancestry, captured while the chain was alive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRef {
    pub pid: u32,
    /// Executable file name only, lowercased: `"windowsterminal.exe"`.
    pub exe: String,
}

/// Where the session lives, as seen from inside the hook process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalMeta {
    /// The subset of [`TERMINAL_ENV_VARS`] that was actually set.
    pub env: Map<String, Value>,
    /// PID of the hook process. Useless by the time anyone clicks — the hook
    /// exits within milliseconds — but kept on the wire for diagnostics.
    pub hook_pid: u32,
    /// The hook's ancestry, nearest first: the transient shell the agent
    /// spawned the hook through, the agent CLI, the user's shell, the
    /// terminal host. Captured at event time because that is the one moment
    /// every link is certainly alive — the hook's own parent is typically a
    /// `cmd.exe` that dies milliseconds later, so a click resolves against
    /// this list rather than against the process tree of the past. Ends
    /// before `explorer.exe`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<ProcessRef>,
}

/// Hook event names Atoll knows about.
pub mod events {
    pub const SESSION_START: &str = "SessionStart";
    pub const SESSION_END: &str = "SessionEnd";
    pub const USER_PROMPT_SUBMIT: &str = "UserPromptSubmit";
    pub const STOP: &str = "Stop";
    pub const NOTIFICATION: &str = "Notification";
    pub const PRE_TOOL_USE: &str = "PreToolUse";
    pub const POST_TOOL_USE: &str = "PostToolUse";
    pub const PERMISSION_REQUEST: &str = "PermissionRequest";
}

/// How long a hook blocks waiting for a decision before failing open.
pub mod timeouts {
    use std::time::Duration;

    /// Budget for opening the pipe. Generous enough to ride out the app being
    /// busy, short enough that a missing app costs the session nothing.
    pub const CONNECT: Duration = Duration::from_millis(300);
    /// Budget for pushing a non-blocking event out and leaving.
    pub const SEND: Duration = Duration::from_millis(500);
    /// `PreToolUse` blocks the tool call itself, so it stays short.
    pub const PRE_TOOL_USE: Duration = Duration::from_secs(45);
    /// A `PermissionRequest` is a human-facing prompt: wait effectively forever.
    pub const PERMISSION_REQUEST_CLAUDE: Duration = Duration::from_secs(86_400);
    /// Codex caps its own permission prompts an hour out.
    pub const PERMISSION_REQUEST_CODEX: Duration = Duration::from_secs(3_600);

    /// The wait budget for `event_name`, or `None` if it does not block.
    pub fn for_event(event_name: &str, source: super::HookSource) -> Option<Duration> {
        match event_name {
            super::events::PRE_TOOL_USE => Some(PRE_TOOL_USE),
            super::events::PERMISSION_REQUEST => Some(match source {
                super::HookSource::Claude => PERMISSION_REQUEST_CLAUDE,
                super::HookSource::Codex => PERMISSION_REQUEST_CODEX,
            }),
            _ => None,
        }
    }
}

/// A decision the app sends back to a blocked hook.
///
/// `PermissionRequest` has exactly one shape here — the *object* form,
/// `decision: {"behavior": ...}`. See [`PermissionRequestDecision`] for why the
/// competing flat form is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HookDecision {
    PreToolUse(PreToolUseDecision),
    PermissionRequest(PermissionRequestDecision),
}

impl HookDecision {
    /// Render the decision exactly as the agent expects it on the hook's stdout:
    /// key-sorted JSON with a trailing newline.
    pub fn to_stdout_json(&self) -> String {
        let value = match self {
            HookDecision::PreToolUse(decision) => decision.to_value(),
            HookDecision::PermissionRequest(decision) => decision.to_value(),
        };
        let mut out = serde_json::to_string(&value).unwrap_or_default();
        out.push('\n');
        out
    }

    /// An unconditional approval for `event_name`, or `None` for events that
    /// take no decision.
    pub fn allow_for(event_name: &str, reason: Option<String>) -> Option<Self> {
        Self::allow_for_with_input(event_name, reason, None)
    }

    /// An approval that also rewrites the tool's input — how an answered
    /// `AskUserQuestion` gets its answer back to the agent.
    pub fn allow_for_with_input(
        event_name: &str,
        reason: Option<String>,
        updated_input: Option<Value>,
    ) -> Option<Self> {
        match event_name {
            events::PRE_TOOL_USE => Some(HookDecision::PreToolUse(PreToolUseDecision {
                permission_decision: PermissionDecision::Allow,
                permission_decision_reason: reason,
                updated_input,
            })),
            events::PERMISSION_REQUEST => {
                Some(HookDecision::PermissionRequest(PermissionRequestDecision {
                    behavior: PermissionBehavior::Allow,
                    updated_input,
                    message: reason,
                    interrupt: None,
                }))
            }
            _ => None,
        }
    }

    /// A refusal for `event_name`, or `None` for events that take no decision.
    ///
    /// A denial never interrupts the turn: the agent is told "not this call" and
    /// left free to try something else, which is what a user tapping Deny on one
    /// card means.
    pub fn deny_for(event_name: &str, reason: Option<String>) -> Option<Self> {
        match event_name {
            events::PRE_TOOL_USE => Some(HookDecision::PreToolUse(PreToolUseDecision {
                permission_decision: PermissionDecision::Deny,
                permission_decision_reason: reason,
                updated_input: None,
            })),
            events::PERMISSION_REQUEST => {
                Some(HookDecision::PermissionRequest(PermissionRequestDecision {
                    behavior: PermissionBehavior::Deny,
                    updated_input: None,
                    message: reason,
                    interrupt: Some(false),
                }))
            }
            _ => None,
        }
    }
}

/// `hookSpecificOutput.permissionDecision` for `PreToolUse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    /// Run the tool without prompting.
    Allow,
    /// Block the tool call.
    Deny,
    /// Hand the choice back to the agent's own permission flow. Claude Code
    /// spells this `ask`; the upstream macOS project calls it "escalate".
    #[serde(alias = "escalate")]
    Ask,
}

impl PermissionDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionDecision::Allow => "allow",
            PermissionDecision::Deny => "deny",
            PermissionDecision::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreToolUseDecision {
    pub permission_decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_decision_reason: Option<String>,
    /// Replacement `tool_input`. Only meaningful alongside `allow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
}

impl PreToolUseDecision {
    /// ```json
    /// {"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse",
    ///  "permissionDecision":"allow","permissionDecisionReason":"..."},"suppressOutput":true}
    /// ```
    pub fn to_value(&self) -> Value {
        let mut specific = Map::new();
        specific.insert(
            "hookEventName".into(),
            Value::String(events::PRE_TOOL_USE.into()),
        );
        specific.insert(
            "permissionDecision".into(),
            Value::String(self.permission_decision.as_str().into()),
        );
        if let Some(reason) = &self.permission_decision_reason {
            specific.insert(
                "permissionDecisionReason".into(),
                Value::String(reason.clone()),
            );
        }
        if let Some(updated) = &self.updated_input {
            specific.insert("updatedInput".into(), updated.clone());
        }

        let mut root = Map::new();
        root.insert("continue".into(), Value::Bool(true));
        root.insert("hookSpecificOutput".into(), Value::Object(specific));
        root.insert("suppressOutput".into(), Value::Bool(true));
        Value::Object(root)
    }
}

/// `decision.behavior` for `PermissionRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

impl PermissionBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionBehavior::Allow => "allow",
            PermissionBehavior::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestDecision {
    pub behavior: PermissionBehavior,
    /// Replacement tool input. Only meaningful alongside `allow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
    /// Why the request was denied. Only meaningful alongside `deny`. Emitted as
    /// both `message` and `reason` — see [`PermissionRequestDecision::to_value`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether a denial should also interrupt the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<bool>,
}

impl PermissionRequestDecision {
    /// ```json
    /// {"hookSpecificOutput":{"decision":{"behavior":"allow","updatedInput":{}},
    ///  "hookEventName":"PermissionRequest"},"suppressOutput":true}
    /// ```
    ///
    /// **Verdict (2026-08-23, settled):** this object form is the only shape
    /// Atoll ever sends. The installed Claude Code binary's handler reads
    /// `hookSpecificOutput.decision.behavior` and `decision.updatedInput`; under
    /// the flat form that Claude Code's written hook reference shows —
    /// `"decision": "allow"` with a sibling `reason` — `decision.behavior` is
    /// undefined, which that handler maps to **deny**. Emitting it would
    /// silently reject the user's own approval, so the flat shape is gone from
    /// this crate entirely rather than left as a reachable option.
    ///
    /// The denial reason goes out under both `message` and `reason`: the two
    /// sources disagree on the spelling and an ignored key costs nothing.
    pub fn to_value(&self) -> Value {
        let mut decision = Map::new();
        decision.insert(
            "behavior".into(),
            Value::String(self.behavior.as_str().into()),
        );
        match self.behavior {
            PermissionBehavior::Allow => {
                // An empty object means "run it exactly as requested".
                decision.insert(
                    "updatedInput".into(),
                    self.updated_input
                        .clone()
                        .unwrap_or_else(|| Value::Object(Map::new())),
                );
            }
            PermissionBehavior::Deny => {
                if let Some(interrupt) = self.interrupt {
                    decision.insert("interrupt".into(), Value::Bool(interrupt));
                }
                let reason = self.message.clone().unwrap_or_default();
                decision.insert("message".into(), Value::String(reason.clone()));
                decision.insert("reason".into(), Value::String(reason));
            }
        }

        let mut specific = Map::new();
        specific.insert("decision".into(), Value::Object(decision));
        specific.insert(
            "hookEventName".into(),
            Value::String(events::PERMISSION_REQUEST.into()),
        );

        let mut root = Map::new();
        root.insert("hookSpecificOutput".into(), Value::Object(specific));
        root.insert("suppressOutput".into(), Value::Bool(true));
        Value::Object(root)
    }
}

/// Serialize an envelope as one newline-terminated line.
pub fn encode_line(envelope: &Envelope) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(envelope)?;
    line.push('\n');
    Ok(line)
}

/// Parse one line from the pipe.
pub fn decode_line(line: &str) -> serde_json::Result<Envelope> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_tool_use_allow_is_byte_stable() {
        let decision = HookDecision::PreToolUse(PreToolUseDecision {
            permission_decision: PermissionDecision::Allow,
            permission_decision_reason: Some("approved in Atoll".into()),
            updated_input: None,
        });
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","#,
                r#""permissionDecision":"allow","permissionDecisionReason":"approved in Atoll"},"#,
                r#""suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn pre_tool_use_skips_none_fields() {
        let decision = HookDecision::PreToolUse(PreToolUseDecision {
            permission_decision: PermissionDecision::Deny,
            permission_decision_reason: None,
            updated_input: None,
        });
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","#,
                r#""permissionDecision":"deny"},"suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn pre_tool_use_carries_updated_input() {
        let decision = HookDecision::PreToolUse(PreToolUseDecision {
            permission_decision: PermissionDecision::Ask,
            permission_decision_reason: None,
            updated_input: Some(serde_json::json!({ "command": "ls", "cwd": "." })),
        });
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","#,
                r#""permissionDecision":"ask","updatedInput":{"command":"ls","cwd":"."}},"#,
                r#""suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn permission_request_allow_is_byte_stable() {
        let decision = HookDecision::PermissionRequest(PermissionRequestDecision {
            behavior: PermissionBehavior::Allow,
            updated_input: None,
            message: None,
            interrupt: None,
        });
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedInput":{}},"#,
                r#""hookEventName":"PermissionRequest"},"suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn permission_request_deny_is_byte_stable() {
        let decision = HookDecision::PermissionRequest(PermissionRequestDecision {
            behavior: PermissionBehavior::Deny,
            updated_input: None,
            message: Some("denied in Atoll".into()),
            interrupt: Some(true),
        });
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"hookSpecificOutput":{"decision":{"behavior":"deny","interrupt":true,"#,
                r#""message":"denied in Atoll","reason":"denied in Atoll"},"#,
                r#""hookEventName":"PermissionRequest"},"suppressOutput":true}"#,
                "\n"
            )
        );
    }

    /// The 2026-08-23 verdict, locked down: whatever else changes, a
    /// `PermissionRequest` approval must keep nesting the verdict under a
    /// `decision` *object*. The flat spelling reads as a denial to Claude Code.
    #[test]
    fn a_permission_request_verdict_is_never_a_bare_string() {
        for decision in [
            HookDecision::allow_for(events::PERMISSION_REQUEST, None).unwrap(),
            HookDecision::deny_for(events::PERMISSION_REQUEST, Some("no".into())).unwrap(),
        ] {
            let value: Value = serde_json::from_str(&decision.to_stdout_json()).unwrap();
            let verdict = &value["hookSpecificOutput"]["decision"];
            assert!(
                verdict.is_object(),
                "the verdict must be an object, got {verdict}"
            );
            assert!(verdict["behavior"].is_string());
        }
    }

    #[test]
    fn permission_request_deny_does_not_interrupt_the_turn() {
        let decision = HookDecision::deny_for(events::PERMISSION_REQUEST, Some("denied".into()))
            .expect("PermissionRequest takes a decision");
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"hookSpecificOutput":{"decision":{"behavior":"deny","interrupt":false,"#,
                r#""message":"denied","reason":"denied"},"#,
                r#""hookEventName":"PermissionRequest"},"suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn an_answered_question_rides_back_as_updated_input() {
        let decision = HookDecision::allow_for_with_input(
            events::PRE_TOOL_USE,
            None,
            Some(serde_json::json!({"answers": {"Which?": "Postgres"}})),
        )
        .expect("PreToolUse takes a decision");
        assert_eq!(
            decision.to_stdout_json(),
            concat!(
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","#,
                r#""permissionDecision":"allow","updatedInput":{"answers":{"Which?":"Postgres"}}},"#,
                r#""suppressOutput":true}"#,
                "\n"
            )
        );
    }

    #[test]
    fn deny_ignores_non_blocking_events() {
        assert!(HookDecision::deny_for(events::STOP, None).is_none());
    }

    #[test]
    fn hook_payload_round_trips_unknown_keys() {
        let raw = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "permission_mode": "default",
            "future_field": {"nested": [1, 2, 3]},
            "another": "kept"
        }"#;
        let payload: HookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(payload.event_name(), "PreToolUse");
        assert_eq!(payload.session_id.as_deref(), Some("abc123"));
        assert_eq!(payload.tool_name.as_deref(), Some("Bash"));
        assert_eq!(payload.permission_mode.as_deref(), Some("default"));
        assert!(payload.is_blocking());
        assert_eq!(payload.extra.len(), 2);

        let round_tripped: Value = serde_json::to_value(&payload).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(round_tripped, original);
    }

    #[test]
    fn absent_fields_are_not_serialized() {
        let payload: HookPayload = serde_json::from_str(r#"{"hook_event_name":"Stop"}"#).unwrap();
        assert!(!payload.is_blocking());
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"hook_event_name":"Stop"}"#
        );
    }

    #[test]
    fn command_envelope_matches_upstream_shape() {
        let payload: HookPayload =
            serde_json::from_str(r#"{"hook_event_name":"SessionStart","session_id":"s1"}"#)
                .unwrap();
        let envelope = Envelope::Command {
            command: Command::ProcessClaudeHook {
                claude_hook: payload,
                source: HookSource::Claude,
            },
        };
        let line = encode_line(&envelope).unwrap();
        assert_eq!(
            line,
            concat!(
                r#"{"type":"command","command":{"type":"processClaudeHook","#,
                r#""claudeHook":{"hook_event_name":"SessionStart","session_id":"s1"},"#,
                r#""source":"claude"}}"#,
                "\n"
            )
        );

        let decoded = decode_line(line.trim_end()).unwrap();
        let Envelope::Command {
            command: Command::ProcessClaudeHook { claude_hook, .. },
        } = decoded
        else {
            panic!("expected a processClaudeHook command");
        };
        assert_eq!(claude_hook.event_name(), "SessionStart");
    }

    #[test]
    fn response_envelope_round_trips() {
        let response = Envelope::Response {
            response: Response::Decision {
                decision: HookDecision::allow_for(events::PRE_TOOL_USE, None).unwrap(),
            },
        };
        let line = encode_line(&response).unwrap();
        let Envelope::Response {
            response: Response::Decision { decision },
        } = decode_line(line.trim_end()).unwrap()
        else {
            panic!("expected a decision response");
        };
        assert!(decision.to_stdout_json().contains(r#""allow""#));
    }

    #[test]
    fn allow_for_ignores_non_blocking_events() {
        assert!(HookDecision::allow_for(events::STOP, None).is_none());
        assert!(HookDecision::allow_for(events::POST_TOOL_USE, None).is_none());
    }

    #[test]
    fn terminal_meta_round_trips_through_extra() {
        let mut payload = HookPayload::default();
        let mut env = Map::new();
        env.insert("WT_SESSION".into(), Value::String("guid".into()));
        payload.set_terminal_meta(TerminalMeta {
            env,
            hook_pid: 42,
            ancestors: vec![
                ProcessRef {
                    pid: 41,
                    exe: "cmd.exe".into(),
                },
                ProcessRef {
                    pid: 40,
                    exe: "windowsterminal.exe".into(),
                },
            ],
        });

        // Key order inside the flattened extra map is serde_json's business,
        // not ours: assert presence, and shapes via the decode below.
        let encoded = serde_json::to_string(&payload).unwrap();
        assert!(encoded.contains(r#""atollTerminal""#));
        assert!(encoded.contains(r#""ancestors""#));
        assert!(encoded.contains(r#""windowsterminal.exe""#));

        let decoded: HookPayload = serde_json::from_str(&encoded).unwrap();
        let meta = decoded.terminal_meta().unwrap();
        assert_eq!(meta.hook_pid, 42);
        assert_eq!(meta.ancestors.len(), 2);
        assert_eq!(meta.ancestors[1].exe, "windowsterminal.exe");
        assert_eq!(meta.env["WT_SESSION"], Value::String("guid".into()));
    }

    #[test]
    fn terminal_meta_from_an_older_hook_still_parses() {
        // A hook built before ancestors existed sends meta without them; the
        // session must simply come out not jumpable, not fail to parse.
        let raw = serde_json::json!({
            "atollTerminal": {"env": {}, "hookPid": 7}
        });
        let payload: HookPayload = serde_json::from_value(raw).unwrap();
        let meta = payload.terminal_meta().unwrap();
        assert_eq!(meta.hook_pid, 7);
        assert!(meta.ancestors.is_empty());
    }

    #[test]
    fn blocking_timeouts_follow_the_source() {
        assert_eq!(
            timeouts::for_event(events::PRE_TOOL_USE, HookSource::Claude),
            Some(timeouts::PRE_TOOL_USE)
        );
        assert_eq!(
            timeouts::for_event(events::PERMISSION_REQUEST, HookSource::Claude),
            Some(timeouts::PERMISSION_REQUEST_CLAUDE)
        );
        assert_eq!(
            timeouts::for_event(events::PERMISSION_REQUEST, HookSource::Codex),
            Some(timeouts::PERMISSION_REQUEST_CODEX)
        );
        assert_eq!(
            timeouts::for_event(events::SESSION_START, HookSource::Claude),
            None
        );
    }

    #[test]
    fn escalate_is_accepted_as_ask() {
        let decision: PermissionDecision = serde_json::from_str(r#""escalate""#).unwrap();
        assert_eq!(decision, PermissionDecision::Ask);
        assert_eq!(
            serde_json::to_string(&decision).unwrap(),
            r#""ask""#,
            "we normalize back to the name Claude Code documents"
        );
    }
}
