//! Dismiss the details like a popup, even when Windows did not activate it.
//! Inputs come from the existing pointer timer; no global mouse hook is needed.

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
