//! Drafts stay on the card when navigating between Codex questions.
use atoll_core::questions::{Answer, Answers, InputRequest};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub choice: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    pub request: InputRequest,
    pub page: usize,
    pub drafts: Vec<Draft>,
}

impl Form {
    pub fn new(request: InputRequest) -> Option<Self> {
        request.valid().then(|| Self {
            drafts: vec![Draft::default(); request.questions.len()],
            request,
            page: 0,
        })
    }
    pub fn value(&self, page: usize) -> Option<String> {
        let question = self.request.questions.get(page)?;
        let draft = self.drafts.get(page)?;
        if question.free_text() && !draft.text.trim().is_empty() {
            return Some(draft.text.clone());
        }
        Some(question.options.as_ref()?.get(draft.choice?)?.label.clone())
    }
    pub fn answers(&self) -> Option<Answers> {
        let answers: Answers = self
            .request
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                Some((
                    q.id.clone(),
                    Answer {
                        answers: vec![self.value(i)?],
                    },
                ))
            })
            .collect::<Option<_>>()?;
        self.request.accepts(&answers).then_some(answers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_default_submission_and_edits_survive_back_navigation() {
        let request = serde_json::from_value(serde_json::json!({"threadId":"t", "turnId":"u", "itemId":"i", "questions":[
            {"id":"one","header":"","question":"One?","options":[{"label":"A","description":"A description"}],"isOther":true},
            {"id":"two","header":"","question":"Two?"}
        ]})).unwrap();
        let mut form = Form::new(request).unwrap();
        assert!(form.answers().is_none());
        form.drafts[0].choice = Some(0);
        form.page = 1;
        form.drafts[1].text = "中文\nsecond line".into();
        form.page = 0;
        assert_eq!(form.value(0).as_deref(), Some("A"));
        form.drafts[0].text = "自定义".into();
        assert_eq!(form.answers().unwrap()["one"].answers, ["自定义"]);
        assert_eq!(
            form.answers().unwrap()["two"].answers,
            ["中文\nsecond line"]
        );
    }
}
