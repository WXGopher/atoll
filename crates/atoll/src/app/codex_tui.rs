//! Status-bar draft state and the native editor. Opening/toggling never writes;
//! only Apply and Restore Codex defaults touch Codex's user configuration.

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

use atoll_core::install::codex_tui::{self as config, StatusLine};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::ui;

pub(super) struct Draft {
    saved: StatusLine,
    order: Vec<String>,
    selected: HashSet<String>,
}

impl Draft {
    fn new(saved: StatusLine) -> Self {
        let mut order = saved.visible_items();
        let selected = order.iter().cloned().collect();
        for component in config::COMPONENTS {
            if !order.iter().any(|id| id == component.id) {
                order.push(component.id.to_owned());
            }
        }
        Self {
            saved,
            order,
            selected,
        }
    }

    fn items(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| self.selected.contains(*id))
            .cloned()
            .collect()
    }

    fn dirty(&self) -> bool {
        self.items() != self.saved.visible_items()
    }

    fn toggle(&mut self, id: &str, enabled: bool) {
        if !self.order.iter().any(|item| item == id) {
            return;
        }
        if enabled {
            self.selected.insert(id.to_owned());
        } else {
            self.selected.remove(id);
        }
    }

    fn rows(&self) -> Vec<ui::StatusComponent> {
        let mut rows = config::COMPONENTS
            .iter()
            .map(|component| ui::StatusComponent {
                id: component.id.into(),
                label: component.label.into(),
                description: component.description.into(),
                selected: self.selected.contains(component.id),
            })
            .collect::<Vec<_>>();
        // Keep frequently used choices visible before the less common details.
        let quick = [
            "model-with-reasoning",
            "current-dir",
            "thread-name",
            "git-branch",
            "context-remaining",
            "five-hour-limit",
            "weekly-limit",
            "task-progress",
            "used-tokens",
            "codex-version",
        ];
        rows.sort_by_key(|row| {
            quick
                .iter()
                .position(|id| row.id == *id)
                .unwrap_or(quick.len())
        });
        for id in &self.order {
            if !rows.iter().any(|row| row.id == id.as_str()) {
                rows.push(ui::StatusComponent {
                    id: id.into(),
                    label: id.into(),
                    description: "Existing custom item · preview unavailable".into(),
                    selected: self.selected.contains(id),
                });
            }
        }
        rows
    }
}

pub(super) struct Editor {
    pub window: ui::CodexTuiWindow,
    path: PathBuf,
    draft: RefCell<Option<Draft>>,
    left_rows: Rc<VecModel<ui::StatusComponent>>,
    right_rows: Rc<VecModel<ui::StatusComponent>>,
}

impl Editor {
    pub fn new(path: PathBuf) -> Result<Rc<Self>, slint::PlatformError> {
        let editor = Rc::new(Self {
            window: ui::CodexTuiWindow::new()?,
            path,
            draft: RefCell::new(None),
            left_rows: Rc::new(VecModel::default()),
            right_rows: Rc::new(VecModel::default()),
        });
        editor
            .window
            .set_left_components(ModelRc::from(editor.left_rows.clone()));
        editor
            .window
            .set_right_components(ModelRc::from(editor.right_rows.clone()));
        editor
            .window
            .set_config_path(editor.path.to_string_lossy().as_ref().into());
        editor
            .window
            .set_catalog_version(config::CATALOG_VERSION.into());
        let weak = Rc::downgrade(&editor);
        editor.window.on_toggle(move |id, enabled| {
            if let Some(editor) = weak.upgrade() {
                if let Some(draft) = editor.draft.borrow_mut().as_mut() {
                    draft.toggle(&id, enabled);
                }
                editor.refresh();
                if !editor.window.get_error() {
                    editor.note(if editor.window.get_dirty() {
                        "Preview updated. Apply to save, then restart Codex CLI."
                    } else {
                        "Restart Codex CLI to load saved changes. Use codex resume to continue a session."
                    }, false);
                }
            }
        });
        let weak = Rc::downgrade(&editor);
        editor.window.on_apply(move || {
            if let Some(editor) = weak.upgrade() {
                editor.save(false);
            }
        });
        let weak = Rc::downgrade(&editor);
        editor.window.on_restore_defaults(move || {
            if let Some(editor) = weak.upgrade() {
                editor.save(true);
            }
        });
        Ok(editor)
    }

    pub fn show(&self) -> Result<(), slint::PlatformError> {
        if !self.window.window().is_visible() {
            match config::read(&self.path) {
                Ok(saved) => {
                    *self.draft.borrow_mut() = Some(Draft::new(saved));
                    self.note("Restart Codex CLI after applying. Project or profile overrides may take priority.", false);
                }
                Err(error) => {
                    *self.draft.borrow_mut() = None;
                    self.note(
                        &format!("Could not read Codex configuration: {error}"),
                        true,
                    );
                }
            }
            self.refresh();
        }
        self.window.show()
    }

    fn note(&self, message: &str, error: bool) {
        self.window.set_message(message.into());
        self.window.set_error(error);
    }

    fn refresh(&self) {
        let state = self.draft.borrow();
        self.window.set_ready(state.is_some());
        let Some(draft) = state.as_ref() else {
            self.left_rows.set_vec(vec![]);
            self.right_rows.set_vec(vec![]);
            return;
        };
        let items = draft.items();
        self.window.set_preview(config::preview(&items).into());
        self.window.set_selected_count(items.len() as i32);
        self.window.set_dirty(draft.dirty());
        self.window.set_custom(draft.saved.items.is_some());
        let rows = draft.rows();
        // Read across each row, keeping the first/default components together.
        update_rows(&self.left_rows, rows.iter().step_by(2).cloned().collect());
        update_rows(
            &self.right_rows,
            rows.iter().skip(1).step_by(2).cloned().collect(),
        );
    }

    fn save(&self, restore: bool) {
        let result = {
            let state = self.draft.borrow();
            let Some(draft) = state.as_ref() else {
                return;
            };
            let items = draft.items();
            config::save(
                &self.path,
                &draft.saved,
                if restore { None } else { Some(&items) },
            )
        };
        match result {
            Ok(saved) => {
                *self.draft.borrow_mut() = Some(Draft::new(saved));
                self.note(
                    if restore {
                        "Codex defaults restored. Restart Codex CLI to load them."
                    } else {
                        "Status bar saved. Restart Codex CLI to load it; use codex resume to continue a session."
                    },
                    false,
                );
                self.refresh();
            }
            Err(error) => self.note(&format!("Could not save: {error}"), true),
        }
    }
}

fn update_rows(model: &VecModel<ui::StatusComponent>, rows: Vec<ui::StatusComponent>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    // Keep delegates alive: replacing the entire model on each checkbox click
    // loses keyboard focus and interrupts subsequent Space/Tab interaction.
    for (index, row) in rows.into_iter().enumerate() {
        if model.row_data(index).is_none_or(|previous| previous != row) {
            model.set_row_data(index, row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggling_off_and_back_restores_order_and_clears_dirty_state() {
        let mut draft = Draft::new(StatusLine {
            items: Some(vec![
                "git-branch".into(),
                "future-item".into(),
                "model".into(),
            ]),
        });
        let initial = draft.items();
        draft.toggle("git-branch", false);
        assert!(draft.dirty());
        draft.toggle("git-branch", true);
        assert_eq!(draft.items(), initial);
        assert!(!draft.dirty());
        draft.toggle("current-dir", true);
        assert_eq!(&draft.items()[..3], initial.as_slice());
        assert!(
            draft
                .rows()
                .iter()
                .any(|row| row.id == "future-item" && row.selected)
        );
    }

    #[test]
    fn all_components_can_be_hidden_without_mutating_saved_settings() {
        let saved = StatusLine { items: None };
        let mut draft = Draft::new(saved.clone());
        for id in config::DEFAULT_ITEMS {
            draft.toggle(id, false);
        }
        assert!(draft.items().is_empty());
        assert!(draft.dirty());
        assert_eq!(draft.saved, saved);
        draft.toggle("not-a-component", true);
        assert!(draft.items().is_empty());
    }
}
