//! Codex app-server requestUserInput, independent of hook permission decisions.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRequest {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<OptionItem>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionItem {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub answers: Vec<String>,
}
pub type Answers = BTreeMap<String, Answer>;

impl InputRequest {
    pub fn valid(&self) -> bool {
        let mut ids = BTreeSet::new();
        !self.thread_id.is_empty()
            && !self.turn_id.is_empty()
            && !self.item_id.is_empty()
            && (1..=3).contains(&self.questions.len())
            && self.questions.iter().all(|q| {
                !q.id.is_empty()
                    && ids.insert(&q.id)
                    && !q.question.trim().is_empty()
                    && q.options.as_ref().is_none_or(|options| {
                        options.len() <= 32
                            && options.iter().all(|option| !option.label.trim().is_empty())
                    })
            })
    }

    pub fn accepts(&self, answers: &Answers) -> bool {
        answers.len() == self.questions.len()
            && self.questions.iter().all(|q| {
                let Some(answer) = answers.get(&q.id) else {
                    return false;
                };
                // Native requestUserInput asks for one choice or one free-text answer.
                // Do not invent a multiSelect flag that Codex doesn't send.
                answer.answers.len() == 1
                    && !answer.answers[0].trim().is_empty()
                    && (q.free_text()
                        || q.options.as_ref().is_some_and(|options| {
                            options
                                .iter()
                                .any(|option| option.label == answer.answers[0])
                        }))
            })
    }
}

impl Question {
    pub fn free_text(&self) -> bool {
        self.is_other || self.options.as_ref().is_none_or(Vec::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_every_question_and_preserves_ids_and_unicode() {
        let request: InputRequest = serde_json::from_value(serde_json::json!({
            "threadId":"thread", "turnId":"turn", "itemId":"item", "isBlocking":true,
            "questions":[
                {"id":"choice", "header":"选择", "question":"选哪个？", "options":[{"label":"甲","description":"第一项"}]},
                {"id":"text", "header":"输入", "question":"补充说明", "isSecret":true}
            ]
        })).unwrap();
        assert!(request.valid());
        let mut answers = Answers::new();
        answers.insert(
            "choice".into(),
            Answer {
                answers: vec!["甲".into()],
            },
        );
        assert!(!request.accepts(&answers));
        answers.insert(
            "text".into(),
            Answer {
                answers: vec!["两行\n中文".into()],
            },
        );
        assert!(request.accepts(&answers));
        answers.get_mut("choice").unwrap().answers = vec!["不存在".into()];
        assert!(!request.accepts(&answers));
        let mut duplicate = request.clone();
        duplicate.questions[1].id = "choice".into();
        assert!(!duplicate.valid());
    }
}
