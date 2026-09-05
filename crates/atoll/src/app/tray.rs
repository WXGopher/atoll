//! The notification-area icon and its menu.
//!
//! # Why this polls
//!
//! `tray-icon` needs a Win32 message loop on the thread that creates it, and it
//! delivers what it hears on a crossbeam channel rather than through a callback.
//! Slint's winit backend already pumps every message for its thread, so creating
//! the icon on the UI thread — once the loop is running — is enough to make the
//! icon work. Reading the channel is then the only part left, and a 100 ms poll
//! from a Slint timer is both simpler and more robust than trying to hook winit's
//! own dispatch.

use std::cell::Cell;

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::icon::{self, IconState};
use super::win::Rect;

const ID_SETUP: &str = "atoll.setup";
const ID_QUIT: &str = "atoll.quit";

/// What the user asked the tray for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    OpenSettings,
    Quit,
    /// Left click: open or close the session panel. Carries the icon's own
    /// screen rectangle, which is where the panel has to appear.
    ToggleFlyout(Rect),
}

pub struct Tray {
    // Dropping this removes the icon from the notification area, so the field is
    // load-bearing beyond the calls made on it.
    handle: TrayIcon,
    /// The last state drawn, so an unchanged icon is not redrawn 10 times a
    /// second — every redraw is a Shell_NotifyIcon round trip.
    drawn: Cell<Option<IconState>>,
    size: u32,
}

impl Tray {
    /// Build the icon. Must be called on the thread running the event loop, and
    /// only once that loop has started.
    pub fn new(icon_size: u32) -> Result<Self, String> {
        let menu = Menu::new();
        let setup_item = MenuItem::with_id(ID_SETUP, "Settings…", true, None);
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit Atoll", true, None);
        menu.append_items(&[&setup_item, &PredefinedMenuItem::separator(), &quit_item])
            .map_err(|error| error.to_string())?;

        let state = IconState {
            sessions: 0,
            waiting: 0,
            pulse: 0.0,
        };
        let handle = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Atoll")
            // Left click belongs to the session panel; the menu is on the right,
            // which is where Windows users look for it.
            .with_menu_on_left_click(false)
            .with_icon(build_icon(state, icon_size)?)
            .build()
            .map_err(|error| error.to_string())?;

        Ok(Self {
            handle,
            drawn: Cell::new(Some(state)),
            size: icon_size,
        })
    }

    /// Redraw the icon, but only if it would actually look different.
    pub fn refresh(&self, state: IconState) {
        if self.drawn.get() == Some(state) {
            return;
        }
        if let Ok(icon) = build_icon(state, self.size) {
            let _ = self.handle.set_icon(Some(icon));
            self.drawn.set(Some(state));
        }
    }

    pub fn set_tooltip(&self, text: &str) {
        let _ = self.handle.set_tooltip(Some(text));
    }

    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        self.handle.rect().is_some_and(|rect| {
            let left = rect.position.x.round() as i32;
            let top = rect.position.y.round() as i32;
            x >= left
                && y >= top
                && x < left + rect.size.width as i32
                && y < top + rect.size.height as i32
        })
    }

    /// Everything the tray has heard since the last call.
    pub fn poll(&self) -> Vec<TrayCommand> {
        let mut commands = Vec::new();

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(command) = menu_command(&event.id) {
                commands.push(command);
            }
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            // Act on the release, not the press: a press that turns into a drag
            // or a menu should not also have toggled the panel.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                commands.push(TrayCommand::ToggleFlyout(Rect {
                    left: rect.position.x.round() as i32,
                    top: rect.position.y.round() as i32,
                    right: rect.position.x.round() as i32 + rect.size.width as i32,
                    bottom: rect.position.y.round() as i32 + rect.size.height as i32,
                }));
            }
        }

        commands
    }
}

fn menu_command(id: &MenuId) -> Option<TrayCommand> {
    match id.as_ref() {
        ID_SETUP => Some(TrayCommand::OpenSettings),
        ID_QUIT => Some(TrayCommand::Quit),
        _ => None,
    }
}

fn build_icon(state: IconState, size: u32) -> Result<Icon, String> {
    Icon::from_rgba(icon::render(state, size), size, size).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_id_maps_to_the_command_it_names() {
        assert_eq!(
            menu_command(&MenuId::new(ID_SETUP)),
            Some(TrayCommand::OpenSettings)
        );
        assert_eq!(menu_command(&MenuId::new(ID_QUIT)), Some(TrayCommand::Quit));
        assert_eq!(menu_command(&MenuId::new("something else")), None);
    }
}
