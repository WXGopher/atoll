//! Dismiss the details like a popup, even when Windows did not activate it.
//! Inputs come from the existing pointer timer; no global mouse hook is needed.

pub const PEEK_LIMIT: usize = 6;
const PEEK_DELAY_MS: u64 = 500;
const LEAVE_DELAY_MS: u64 = 180;

#[derive(Debug, PartialEq, Eq)]
pub enum HoverAction {
    Open,
    Close,
}

/// A small grace period lets the pointer cross the gap into the preview.
#[derive(Default)]
pub struct Hover {
    entered: Option<u64>,
    left: Option<u64>,
    open: bool,
    suppressed: bool,
}

impl Hover {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn suppress_until_exit(&mut self) {
        self.reset();
        self.suppressed = true;
    }

    pub fn update(
        &mut self,
        now: u64,
        launcher: bool,
        panel: bool,
        enabled: bool,
    ) -> Option<HoverAction> {
        if self.suppressed {
            if !launcher {
                self.reset();
            }
            return None;
        }
        if !enabled {
            let close = self.open;
            self.reset();
            return close.then_some(HoverAction::Close);
        }
        if self.open {
            if launcher || panel {
                self.left = None;
            } else if now.saturating_sub(*self.left.get_or_insert(now)) >= LEAVE_DELAY_MS {
                self.reset();
                return Some(HoverAction::Close);
            }
        } else if launcher {
            if now.saturating_sub(*self.entered.get_or_insert(now)) >= PEEK_DELAY_MS {
                self.open = true;
                return Some(HoverAction::Open);
            }
        } else {
            self.entered = None;
        }
        None
    }
}

pub fn peek_height(rows: usize) -> f32 {
    28.0 + 14.0 + 10.0 + rows as f32 * 32.0 + rows.saturating_sub(1) as f32 * 9.0 + 30.0
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PointerTarget {
    Panel,
    Launcher,
    Outside,
}

pub struct Dismissal {
    foreground: Option<isize>,
    buttons: u8,
    launcher_pressed: bool,
}

impl Dismissal {
    pub fn new(foreground: Option<isize>, buttons: u8) -> Self {
        Self {
            foreground,
            buttons,
            launcher_pressed: false,
        }
    }

    /// Adopting the native window can briefly hide it to remove its taskbar
    /// button. That internal focus change is not a dismissal gesture.
    pub fn rebase_foreground(&mut self, foreground: Option<isize>) {
        self.foreground = foreground;
    }

    pub fn should_close(
        &mut self,
        foreground: Option<isize>,
        buttons: u8,
        target: PointerTarget,
        panel_focused: bool,
    ) -> bool {
        let pressed = buttons & !self.buttons != 0;
        let focus_changed = foreground.is_some() && foreground != self.foreground;
        self.buttons = buttons;
        if foreground.is_some() {
            self.foreground = foreground;
        }

        // The launcher toggles on release. Closing on its press would make
        // that release reopen the panel immediately.
        let launcher_gesture =
            self.launcher_pressed || (target == PointerTarget::Launcher && buttons & 1 != 0);
        self.launcher_pressed = launcher_gesture && buttons & 1 != 0;
        if launcher_gesture {
            return false;
        }
        (pressed && target == PointerTarget::Outside) || (focus_changed && !panel_focused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PointerTarget::*;

    #[test]
    fn peek_waits_for_dwell_and_bridges_the_gap_without_sticking() {
        let mut hover = Hover::default();
        assert_eq!(hover.update(0, true, false, true), None);
        assert_eq!(hover.update(499, true, false, true), None);
        assert_eq!(
            hover.update(500, true, false, true),
            Some(HoverAction::Open)
        );
        assert_eq!(hover.update(600, false, false, true), None);
        assert_eq!(hover.update(700, false, true, true), None);
        assert_eq!(hover.update(1000, false, false, true), None);
        assert_eq!(
            hover.update(1180, false, false, true),
            Some(HoverAction::Close)
        );
    }

    #[test]
    fn disabled_peeks_and_brief_passes_never_open() {
        let mut hover = Hover::default();
        hover.update(0, true, false, true);
        hover.update(400, false, false, true);
        assert_eq!(hover.update(501, true, false, true), None);
        assert_eq!(hover.update(2000, true, false, false), None);
        assert_eq!(hover.update(2001, true, false, true), None);
        assert_eq!(
            hover.update(2501, true, false, true),
            Some(HoverAction::Open)
        );
        assert_eq!(
            hover.update(2502, true, false, false),
            Some(HoverAction::Close)
        );
    }

    #[test]
    fn explicit_close_requires_leaving_the_launcher_before_another_peek() {
        let mut hover = Hover::default();
        hover.suppress_until_exit();
        assert_eq!(hover.update(0, true, false, true), None);
        assert_eq!(hover.update(5000, true, false, true), None);
        assert_eq!(hover.update(5001, false, false, true), None);
        assert_eq!(hover.update(5002, true, false, true), None);
        assert_eq!(
            hover.update(5502, true, false, true),
            Some(HoverAction::Open)
        );
    }

    #[test]
    fn clicking_the_desktop_closes_even_without_a_focus_change() {
        for button in [1, 2, 4] {
            let mut state = Dismissal::new(Some(1), 0);
            assert!(state.should_close(Some(1), button, Outside, false));
        }
    }

    #[test]
    fn switching_windows_with_the_keyboard_closes_the_panel() {
        let mut state = Dismissal::new(Some(1), 0);
        assert!(state.should_close(Some(2), 0, Panel, false));
    }

    #[test]
    fn panel_interaction_and_pointer_leaving_do_not_dismiss() {
        let mut state = Dismissal::new(Some(1), 0);
        assert!(!state.should_close(Some(2), 1, Panel, true));
        assert!(!state.should_close(Some(2), 0, Outside, true));
        assert!(!state.should_close(Some(2), 0, Outside, true));
    }

    #[test]
    fn launcher_press_leaves_the_release_to_toggle_once() {
        let mut state = Dismissal::new(Some(1), 0);
        assert!(!state.should_close(Some(2), 1, Launcher, false));
        assert!(!state.should_close(Some(2), 0, Launcher, false));
        let mut delayed = Dismissal::new(Some(1), 0);
        assert!(!delayed.should_close(Some(1), 1, Launcher, true));
        assert!(!delayed.should_close(Some(2), 0, Launcher, false));
        assert!(delayed.should_close(Some(3), 0, Launcher, false));
    }

    #[test]
    fn an_opening_press_is_ignored_but_the_next_outside_click_is_not() {
        let mut state = Dismissal::new(Some(1), 1);
        assert!(!state.should_close(Some(1), 1, Outside, false));
        assert!(!state.should_close(Some(1), 0, Outside, false));
        assert!(state.should_close(Some(1), 1, Outside, false));
    }

    #[test]
    fn internal_adoption_and_transient_empty_focus_do_not_close_the_panel() {
        let mut state = Dismissal::new(Some(1), 0);
        state.rebase_foreground(Some(2));
        assert!(!state.should_close(Some(2), 0, Outside, false));
        assert!(!state.should_close(None, 0, Outside, false));
        assert!(!state.should_close(Some(2), 0, Outside, false));
        assert!(state.should_close(Some(3), 0, Outside, false));
    }
}
