//! Turning a hook payload into the card Atoll shows, and a card plus a
//! click back into the decision the agent is waiting for.
//!
//! Nothing here touches the UI or the pipe, so the whole translation — including
//! the shape of an answered `AskUserQuestion` — is testable from literal JSON.

use atoll_core::protocol::{HookDecision, HookPayload, HookSource, events};
use atoll_core::state::{ASK_USER_QUESTION, correlation_key};
use serde_json::{Map, Value};

use super::cardview::CardKind;
use crate::util::{one_line, project_name, truncate};

/// More than four buttons stops being a choice and starts being a list; a
/// question with more options than this is one to answer in the terminal.
pub const MAX_OPTIONS: usize = 4;

/// How long a completed-turn card stays up before it collapses on its own.
pub const COMPLETED_DWELL_SECS: u64 = 3;

/// The grace period after the pointer leaves a card that the user has actually
/// looked at. A card they never touched stays until they deal with it.
pub const HOVER_DWELL_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub kind: CardKind,
    pub session_id: String,
    /// The pending approval this card answers; see
    /// [`atoll_core::state::correlation_key`].
    pub key: String,
    /// The event this card answers, which decides the reply shape. Always
    /// `PermissionRequest` for a card that takes a decision; `Stop` for one that
    /// only reports.
    pub event: String,
    pub source: HookSource,
    /// The project the session is working in.
    pub title: String,
    /// The tool being asked about, shown to the right of the title.
    pub tool: String,
    /// The one line that says what is actually being asked.
    pub detail: String,
    /// Button labels, for a question card.
    pub options: Vec<String>,
    /// The question text, which is also the key an answer is filed under.
    pub question: String,
    /// The agent's original `tool_input`, kept whole so an answer can be merged
    /// into it rather than replacing it.
    pub tool_input: Option<Value>,
    pub created_at: u64,
}

impl Card {
    /// The card for a request that is waiting on the user, or `None` for an
    /// event that is not one.
    ///
    /// **Only `PermissionRequest` raises a card.** `PreToolUse` fires before
    /// every tool call — including everything the user's own settings already
    /// allow — and it fires before Claude Code has decided whether to ask
    /// anybody. A card per `PreToolUse` is a card per `Read`, each one holding
    /// the agent for the hook's 45-second budget. `PermissionRequest` is the
    /// event that means a human is genuinely about to be asked, and its budget
    /// is a day, which is the protocol agreeing.
    pub fn for_request(payload: &HookPayload, source: HookSource, now: u64) -> Option<Self> {
        if payload.event_name() != events::PERMISSION_REQUEST {
            return None;
        }
        let question = payload.tool_name.as_deref() == Some(ASK_USER_QUESTION);
        let (text, options) = if question {
            parse_question(payload.tool_input.as_ref())
        } else {
            (String::new(), Vec::new())
        };

        let detail = if question {
            truncate(&text, 140)
        } else {
            crate::headless::summarize_input(payload)
        };
        let kind = if question {
            CardKind::Question {
                options: options.len(),
                lines: super::cardview::body_lines(&detail),
            }
        } else {
            CardKind::Approval
        };

        Some(Self {
            kind,
            session_id: payload.session_id.clone().unwrap_or_default(),
            key: correlation_key(payload),
            event: payload.event_name().to_string(),
            source,
            title: title_for(payload),
            tool: if question {
                String::new()
            } else {
                payload.tool_name.clone().unwrap_or_else(|| "?".to_string())
            },
            detail,
            options,
            question: text,
            tool_input: payload.tool_input.clone(),
            created_at: now,
        })
    }

    /// The card for a turn that just finished.
    pub fn completed(
        payload: &HookPayload,
        source: HookSource,
        summary: Option<&str>,
        now: u64,
    ) -> Self {
        Self {
            kind: CardKind::Completed,
            session_id: payload.session_id.clone().unwrap_or_default(),
            key: String::new(),
            event: payload.event_name().to_string(),
            source,
            title: title_for(payload),
            tool: "done".to_string(),
            // A transcript Atoll cannot read — a fresh session, a path that
            // moved — still gets a card that says something, rather than an
            // empty line where the summary would have been.
            detail: summary
                .map(|text| truncate(&one_line(text), 90))
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "Turn finished.".to_string()),
            options: Vec::new(),
            question: String::new(),
            tool_input: None,
            created_at: now,
        }
    }

    /// Whether the card is answered by the user rather than by a timer.
    pub fn needs_an_answer(&self) -> bool {
        !matches!(self.kind, CardKind::Completed)
    }

    /// The decision for a tapped Allow or Deny.
    pub fn decision(&self, allow: bool) -> Option<HookDecision> {
        let reason = Some(
            if allow {
                "approved in Atoll"
            } else {
                "denied in Atoll"
            }
            .to_string(),
        );
        if allow {
            HookDecision::allow_for(&self.event, reason)
        } else {
            HookDecision::deny_for(&self.event, reason)
        }
    }

    /// The decision for a chosen option on a question card.
    ///
    /// An answered `AskUserQuestion` is an **approval** whose `updatedInput`
    /// carries the answer: the tool still runs, it just runs already knowing what
    /// the user picked. The answer is filed under the question's own text, since
    /// that is the only stable identifier a question carries.
    pub fn answer(&self, option: usize) -> Option<HookDecision> {
        let label = self.options.get(option)?;
        let mut input = match self.tool_input.clone() {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };

        let mut answers = match input.get("answers") {
            Some(Value::Object(existing)) => existing.clone(),
            _ => Map::new(),
        };
        answers.insert(self.question.clone(), Value::String(label.clone()));
        input.insert("answers".to_string(), Value::Object(answers));

        HookDecision::allow_for_with_input(
            &self.event,
            Some(format!("answered in Atoll: {label}")),
            Some(Value::Object(input)),
        )
    }
}

/// What to call the session on a card: the project folder it is working in.
///
/// Deliberately not the transcript title — a card appears the instant the agent
/// asks, which is exactly when reading a multi-megabyte transcript would be felt
/// as a stutter. The tray panel, which is opened rather than thrown at the user,
/// is where the richer titles go.
fn title_for(payload: &HookPayload) -> String {
    let name = payload
        .cwd
        .as_deref()
        .map(project_name)
        .filter(|name| !name.is_empty());
    match name {
        Some(name) => truncate(&name, 28),
        None => payload
            .session_id
            .as_deref()
            .map(|id| id.chars().take(8).collect())
            .unwrap_or_else(|| "session".to_string()),
    }
}

/// Pull the first question and its option labels out of an `AskUserQuestion`
/// `tool_input`.
///
/// Only the first question: the card answers one thing at a time, and a
/// multi-question payload is rare enough that sending the user to the terminal
/// for the rest beats a card that scrolls.
fn parse_question(input: Option<&Value>) -> (String, Vec<String>) {
    let Some(first) = input
        .and_then(|input| input.get("questions"))
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
    else {
        return (String::new(), Vec::new());
    };

    let text = first
        .get("question")
        .and_then(Value::as_str)
        .map(one_line)
        .unwrap_or_default();

    let options = first
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(option_label)
                .take(MAX_OPTIONS)
                .collect()
        })
        .unwrap_or_default();

    (text, options)
}

/// An option is either `{"label": "…"}` or, on older payloads, the bare string.
fn option_label(option: &Value) -> Option<String> {
    let label = match option {
        Value::String(label) => label.clone(),
        Value::Object(_) => option
            .get("label")
            .or_else(|| option.get("name"))
            .and_then(Value::as_str)?
            .to_string(),
        _ => return None,
    };
    let label = truncate(&one_line(&label), 46);
    (!label.is_empty()).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::protocol::events;
    use serde_json::json;

    const NOW: u64 = 1_787_000_000;

    fn payload(raw: Value) -> HookPayload {
        serde_json::from_value(raw).expect("synthetic payload")
    }

    fn approval() -> HookPayload {
        payload(json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s-1",
            "cwd": r"C:\synthetic\atoll",
            "tool_name": "Bash",
            "tool_input": {"command": "git status\n  --short"},
            "tool_use_id": "tu-1",
        }))
    }

    fn question() -> HookPayload {
        payload(json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s-1",
            "cwd": r"C:\synthetic\atoll",
            "tool_name": ASK_USER_QUESTION,
            "tool_use_id": "tu-9",
            "tool_input": {
                "questions": [{
                    "question": "Which database?",
                    "options": [
                        {"label": "Postgres"},
                        {"label": "SQLite"},
                        {"label": "MySQL"},
                        {"label": "DuckDB"},
                        {"label": "one option too many"},
                    ],
                }],
            },
        }))
    }

    #[test]
    fn a_tool_approval_becomes_an_approval_card() {
        let card = Card::for_request(&approval(), HookSource::Claude, NOW).unwrap();
        assert_eq!(card.kind, CardKind::Approval);
        assert_eq!(card.title, "atoll");
        assert_eq!(card.tool, "Bash");
        assert_eq!(card.detail, "git status --short");
        assert_eq!(card.key, "tu-1");
        assert!(card.needs_an_answer());
    }

    /// The defect this guards: a card per `PreToolUse` is a card per tool call,
    /// each one holding the session for the hook's whole budget.
    #[test]
    fn a_pre_tool_use_never_raises_a_card() {
        let mut raw = approval();
        raw.hook_event_name = Some(events::PRE_TOOL_USE.to_string());
        assert!(Card::for_request(&raw, HookSource::Claude, NOW).is_none());

        // Not even an AskUserQuestion: its card comes from the
        // PermissionRequest that follows, which Atoll can hold open.
        let mut asking = question();
        asking.hook_event_name = Some(events::PRE_TOOL_USE.to_string());
        assert!(Card::for_request(&asking, HookSource::Claude, NOW).is_none());
    }

    #[test]
    fn an_event_that_asks_nothing_gets_no_card() {
        for name in ["Stop", "SessionStart", "PostToolUse", "Notification"] {
            let raw = payload(json!({"hook_event_name": name, "session_id": "s-1"}));
            assert!(
                Card::for_request(&raw, HookSource::Claude, NOW).is_none(),
                "{name} must not raise a card"
            );
        }
    }

    #[test]
    fn a_question_becomes_a_question_card_with_capped_options() {
        let card = Card::for_request(&question(), HookSource::Claude, NOW).unwrap();
        assert_eq!(
            card.kind,
            CardKind::Question {
                options: MAX_OPTIONS,
                lines: 1,
            }
        );
        assert_eq!(card.detail, "Which database?");
        assert_eq!(card.options, ["Postgres", "SQLite", "MySQL", "DuckDB"]);
        assert!(
            card.tool.is_empty(),
            "a question is not a tool call the user needs told about"
        );
    }

    #[test]
    fn an_answer_rides_back_as_updated_input_beside_the_original() {
        let card = Card::for_request(&question(), HookSource::Claude, NOW).unwrap();
        let decision = card.answer(1).expect("SQLite is an option");
        let rendered: Value = serde_json::from_str(&decision.to_stdout_json()).unwrap();

        // A question is answered through the same object shape as any other
        // PermissionRequest: an allow, with the answer riding in updatedInput.
        let decision = &rendered["hookSpecificOutput"]["decision"];
        assert_eq!(decision["behavior"], "allow");
        let updated = &decision["updatedInput"];
        assert_eq!(updated["answers"]["Which database?"], json!("SQLite"));
        assert!(
            updated["questions"].is_array(),
            "the original input must survive alongside the answer: {updated}"
        );
    }

    #[test]
    fn an_answer_to_an_option_that_is_not_there_decides_nothing() {
        let card = Card::for_request(&question(), HookSource::Claude, NOW).unwrap();
        assert!(card.answer(9).is_none());
    }

    #[test]
    fn a_permission_request_answers_in_the_object_shape() {
        let card = Card::for_request(&approval(), HookSource::Claude, NOW).unwrap();

        let allow: Value =
            serde_json::from_str(&card.decision(true).unwrap().to_stdout_json()).unwrap();
        assert_eq!(
            allow["hookSpecificOutput"]["decision"]["behavior"],
            json!("allow")
        );

        let deny: Value =
            serde_json::from_str(&card.decision(false).unwrap().to_stdout_json()).unwrap();
        assert_eq!(
            deny["hookSpecificOutput"]["decision"]["behavior"],
            json!("deny")
        );
    }

    #[test]
    fn a_malformed_question_still_produces_a_usable_card() {
        let raw = payload(json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s-1",
            "tool_name": ASK_USER_QUESTION,
            "tool_input": {"questions": []},
        }));
        let card = Card::for_request(&raw, HookSource::Claude, NOW).unwrap();
        assert_eq!(
            card.kind,
            CardKind::Question {
                options: 0,
                lines: 1,
            }
        );
        assert!(card.options.is_empty());
        // With no cwd the card falls back to the session id rather than being
        // blank.
        assert_eq!(card.title, "s-1");
    }

    #[test]
    fn bare_string_options_are_accepted_too() {
        let raw = payload(json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s-1",
            "tool_name": ASK_USER_QUESTION,
            "tool_input": {"questions": [{"question": "Go on?", "options": ["Yes", "No", 7]}]},
        }));
        let card = Card::for_request(&raw, HookSource::Claude, NOW).unwrap();
        assert_eq!(card.options, ["Yes", "No"]);
    }

    #[test]
    fn a_completed_card_carries_the_last_message_and_expires_on_its_own() {
        let stop = payload(json!({
            "hook_event_name": "Stop",
            "session_id": "s-1",
            "cwd": r"C:\synthetic\atoll",
        }));
        let card = Card::completed(&stop, HookSource::Claude, Some("Rebuilt\n  the index"), NOW);
        assert_eq!(card.kind, CardKind::Completed);
        assert_eq!(card.detail, "Rebuilt the index");
        assert!(!card.needs_an_answer());

        // A session whose transcript said nothing still gets a readable card.
        let silent = Card::completed(&stop, HookSource::Claude, None, NOW);
        assert_eq!(silent.detail, "Turn finished.");
        assert_eq!(
            Card::completed(&stop, HookSource::Claude, Some("  \n "), NOW).detail,
            "Turn finished."
        );
    }
}
