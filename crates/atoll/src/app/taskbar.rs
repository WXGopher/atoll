//! The usage readout that lives in the taskbar.
//!
//! A vertical taskbar has a long empty stretch between the last running app and
//! the notification area, and a horizontal one has the same stretch on its
//! right. Atoll parks a two-chip readout there — `● 92%` for Claude, `● 15%` for
//! Codex — so that the number a user actually checks is on screen without a
//! floating window in the way of anything.
//!
//! # Being inside the taskbar
//!
//! The readout is reparented into `Shell_TrayWnd` rather than floated over it.
//! That is what makes it behave like part of the taskbar: it hides when the
//! taskbar auto-hides, it sits at the taskbar's z-order rather than fighting
//! every full-screen window for topmost, and it moves with the taskbar between
//! monitors. See [`super::win::embed_in`].
//!
//! It is also somebody else's window, so it is best-effort by construction. The
//! shell restarts, third-party taskbars lay themselves out differently, and a
//! shell that paints over its children would leave the readout invisible. Every
//! step degrades: no taskbar, or an embed that will not take, means a small
//! always-on-top strip pinned against the taskbar's inside edge instead — same
//! contents, same click.
//!
//! # A fixed control, not a movable one
//!
//! The readout is not draggable. It behaves like the task-view button: it sits
//! where the taskbar's own layout puts it — stacked just clear of the
//! notification area — and that place is recomputed every tick, so a tray that
//! grows a few icons pushes the readout along rather than crawling underneath
//! it.
//!
//! # Why its clicks come from Windows and not from Slint
//!
//! The readout is the one window of Atoll's that lives for the whole session.
//! Everything else — a card, the detail panel — is opened and closed again, and
//! measurably, on this machine: once a card window has been shown and clicked,
//! Slint stops delivering pointer events to this window and never resumes.
//! Cards keep working (each is a fresh window), so the fault is not fatal, but
//! the readout would go quietly dead the first time somebody answered an
//! approval — and stay dead until Atoll restarted.
//!
//! Reparenting is not the cause; the same thing happens to an unembedded
//! readout, and it survives neither destroying the card's window instead of
//! hiding it, nor re-parenting the readout, nor telling Slint the pointer left
//! first. So the readout's own input is read from Windows: the button state and
//! the cursor position, sampled, with `WindowFromPoint` to confirm the readout
//! is really what is under the pointer. Two facts a poll cannot get wrong.
//!
//! The arithmetic is in free functions with no window in sight, so the
//! interesting half is testable without a shell.

use std::cell::Cell;
use std::rc::Rc;

use atoll_core::protocol::HookSource;
use slint::ComponentHandle;

use super::ui::TaskbarBar;
use super::win::{self, Rect, Taskbar};
use crate::usage_cache::UsageSnapshot;

/// Matches `title:` in `ui/taskbar.slint`; the Win32 lookup keys off it.
pub const WINDOW_TITLE: &str = "Atoll Usage";

/// All logical pixels, and all of them mirrored in `ui/taskbar.slint`.
const PADDING: f32 = 4.0;
/// One chip's row height, and the gap between two of them stacked.
const CHIP_HEIGHT: f32 = 15.0;
const CHIP_GAP: f32 = 1.0;
/// The agent dot, and the gap between it and its number.
const DOT: f32 = 7.0;
const DOT_GAP: f32 = 4.0;
/// Room for the widest number the readout ever shows.
const VALUE_WIDTH: f32 = 26.0;
/// The gap between two chips side by side on a horizontal taskbar.
const CHIP_SPACING: f32 = 10.0;

/// How far the readout stays from the notification area and from the taskbar's
/// own edges.
const MARGIN: i32 = 6;

/// How often the pointer is sampled over the readout. Fast enough that a click
/// is never missed between a press and its release, slow enough to be free.
pub const POINTER_POLL: std::time::Duration = std::time::Duration::from_millis(30);

/// Which way the taskbar runs, and so which way the chips stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Along {
    /// Docked left or right: the bar is tall and narrow, so the chips stack.
    Vertical,
    /// Docked top or bottom: the chips sit side by side.
    Horizontal,
}

impl Along {
    /// Read off the taskbar's own shape rather than from a docking constant,
    /// because a shell replacement is free to have opinions about both.
    pub fn of(taskbar: Rect) -> Self {
        let width = taskbar.right - taskbar.left;
        let height = taskbar.bottom - taskbar.top;
        if height > width {
            Along::Vertical
        } else {
            Along::Horizontal
        }
    }

    pub fn is_vertical(self) -> bool {
        self == Along::Vertical
    }
}

/// One agent's number in the readout.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    /// `None` for the placeholder shown when no agent has reported anything.
    pub agent: Option<HookSource>,
    /// `92%`, or `--` for the placeholder.
    pub value: String,
    /// `""` | `"good"` | `"warn"` | `"low"`; see [`crate::usage_cache::left_tier`].
    pub tier: &'static str,
}

/// The readout's contents: one chip per agent that has a reading, showing the
/// window that will stop it first.
///
/// An agent nobody has run contributes nothing — the taskbar has room for what
/// is true and no room for a placeholder per agent. When that leaves nothing at
/// all, one dash stands in, because a readout that vanishes reads as broken.
pub fn chips(usage: &UsageSnapshot) -> Vec<Chip> {
    let mut chips: Vec<Chip> = super::AGENTS
        .iter()
        .filter_map(|agent| {
            let window = usage.tightest_window(*agent)?;
            Some(Chip {
                agent: Some(*agent),
                value: format!("{}%", window.left),
                tier: crate::usage_cache::left_tier(window.left),
            })
        })
        .collect();
    if chips.is_empty() {
        chips.push(Chip {
            agent: None,
            value: "--".to_string(),
            tier: "",
        });
    }
    chips
}

/// How big the readout is, in logical pixels, for this many chips.
pub fn bar_size(count: usize, along: Along) -> (f32, f32) {
    let count = count.max(1) as f32;
    let chip_width = DOT + DOT_GAP + VALUE_WIDTH;
    match along {
        Along::Vertical => (
            PADDING * 2.0 + chip_width,
            PADDING * 2.0 + count * CHIP_HEIGHT + (count - 1.0) * CHIP_GAP,
        ),
        Along::Horizontal => (
            PADDING * 2.0 + count * chip_width + (count - 1.0) * CHIP_SPACING,
            PADDING * 2.0 + CHIP_HEIGHT,
        ),
    }
}

/// Where the readout goes: in the empty run of the task list, just clear of
/// the notification area.
///
/// "Just clear of" and not "at the far end", because the far end is where the
/// clock is and where a shell replacement is most likely to put something of
/// its own. Recomputed on every placement rather than remembered, so a
/// notification area that grows an icon pushes the readout along instead of
/// spreading underneath it. Coordinates are the taskbar's own, so they are
/// equally right for an embedded child and for a strip drawn against the bar.
pub fn default_offset(taskbar: Taskbar, size: (i32, i32), along: Along) -> (i32, i32) {
    let (width, height) = size;
    let bar = taskbar.rect;
    let notify = taskbar.notify;
    match along {
        Along::Vertical => {
            // Centred across the bar, and stacked above the notification area.
            let x = (bar.right - bar.left - width) / 2;
            let y = notify.top - bar.top - MARGIN - height;
            (x, y.max(MARGIN))
        }
        Along::Horizontal => {
            let y = (bar.bottom - bar.top - height) / 2;
            let x = notify.left - bar.left - MARGIN - width;
            (x.max(MARGIN), y)
        }
    }
}

/// Keep the readout inside the taskbar it belongs to.
///
/// The one place it must not end up is half outside the bar — an embedded
/// window is clipped by its parent, so "outside" means "invisible" rather than
/// "somewhere else". A guard for the degenerate bars: one narrower than the
/// readout, or a notification area that fills the whole thing.
pub fn clamp_to_taskbar(offset: (i32, i32), size: (i32, i32), taskbar: Rect) -> (i32, i32) {
    let span = |value: i32, length: i32, total: i32| {
        if length + MARGIN * 2 >= total {
            (total - length) / 2
        } else {
            value.clamp(MARGIN, total - length - MARGIN)
        }
    };
    (
        span(offset.0, size.0, taskbar.right - taskbar.left),
        span(offset.1, size.1, taskbar.bottom - taskbar.top),
    )
}

/// The readout's window, plus the placement state Slint does not keep for us.
pub struct TaskbarView {
    ui: TaskbarBar,
    /// Found once the event loop has really created the window.
    handle: Cell<Option<isize>>,
    /// The taskbar we are currently a child of. Cleared when the shell restarts
    /// under us, which is what triggers a re-embed.
    host: Cell<Option<isize>>,
    /// Whether the embed took. False means the floating fallback.
    embedded: Cell<bool>,
    /// Whether a press that started on the readout is still held; see the note
    /// in this module's header about why its input comes from Windows rather
    /// than from Slint.
    pressed: Cell<bool>,
    /// Its logical size, from [`bar_size`].
    size: Cell<(f32, f32)>,
    shown: Cell<bool>,
}

impl TaskbarView {
    pub fn new(ui: TaskbarBar) -> Rc<Self> {
        Rc::new(Self {
            ui,
            handle: Cell::new(None),
            host: Cell::new(None),
            embedded: Cell::new(false),
            pressed: Cell::new(false),
            size: Cell::new(bar_size(2, Along::Vertical)),
            shown: Cell::new(false),
        })
    }

    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    pub fn is_embedded(&self) -> bool {
        self.embedded.get()
    }

    /// Ask Slint to paint the readout again.
    ///
    /// On this machine, another Atoll window being mapped or unmapped can cost
    /// the readout its last frame — the same fault family as the pointer-event
    /// loss described in this module's header, but on the rendering side. The
    /// renderer itself stays healthy, so one repaint on request is a full
    /// recovery.
    pub fn request_redraw(&self) {
        self.ui.window().request_redraw();
    }

    fn scale(&self) -> f32 {
        let scale = self.ui.window().scale_factor();
        if scale > 0.0 { scale } else { 1.0 }
    }

    /// The readout's on-screen size in physical pixels.
    pub fn physical_size(&self) -> (i32, i32) {
        let (width, height) = self.size.get();
        let scale = self.scale();
        (
            (width * scale).round() as i32,
            (height * scale).round() as i32,
        )
    }

    pub fn show(&self) {
        if self.shown.get() {
            return;
        }
        if self.ui.show().is_ok() {
            self.shown.set(true);
            // The OS window is new, so whatever we did to the old one is gone.
            self.handle.set(None);
            self.host.set(None);
            self.embedded.set(false);
        }
    }

    pub fn hide(&self) {
        if !self.shown.get() {
            return;
        }
        let _ = self.ui.hide();
        self.shown.set(false);
        self.handle.set(None);
        self.host.set(None);
        self.embedded.set(false);
    }

    /// Put `chips` in the window and resize it to fit them.
    pub fn set_chips(&self, chips: &[Chip], along: Along) {
        use slint::{Model, ModelRc, VecModel};

        let rows: Vec<super::ui::UsageChip> = chips
            .iter()
            .map(|chip| super::ui::UsageChip {
                agent: chip
                    .agent
                    .map(HookSource::as_str)
                    .unwrap_or_default()
                    .into(),
                value: chip.value.clone().into(),
                tier: chip.tier.into(),
            })
            .collect();
        // Nothing changed, so nothing is redrawn: this runs on every tick.
        let unchanged = self.ui.get_chips().row_count() == rows.len()
            && self
                .ui
                .get_chips()
                .iter()
                .zip(rows.iter())
                .all(|(old, new)| old.value == new.value && old.agent == new.agent);
        if unchanged && self.ui.get_vertical() == along.is_vertical() {
            return;
        }

        self.ui.set_vertical(along.is_vertical());
        self.ui.set_chips(ModelRc::new(VecModel::from(rows)));

        let size = bar_size(chips.len(), along);
        self.size.set(size);
        self.ui
            .window()
            .set_size(slint::LogicalSize::new(size.0, size.1));
    }

    /// Find our own window, attach it to the taskbar, and place it.
    ///
    /// Called from a timer rather than once: Slint creates the OS window only
    /// after the event loop is spinning, and the shell can restart at any time
    /// and take our parent with it. Returns whether the readout is currently
    /// where it should be, which is what lets the caller stop polling hard.
    pub fn attach(&self, taskbar: Option<Taskbar>) -> bool {
        let handle = match self.handle.get() {
            Some(handle) => handle,
            None => {
                let Some(handle) = win::window_by_title(WINDOW_TITLE) else {
                    return false;
                };
                self.handle.set(Some(handle));
                win::hide_from_taskbar(handle);
                handle
            }
        };

        let Some(taskbar) = taskbar else {
            // No taskbar to be part of. Float, and keep looking.
            self.embedded.set(false);
            self.host.set(None);
            return false;
        };

        // A new taskbar handle means explorer restarted and took our parent
        // with it; the window survives, orphaned, and has to be re-adopted.
        let host_changed = self.host.get() != Some(taskbar.handle);
        let orphaned = self.embedded.get() && win::parent_of(handle) != Some(taskbar.handle);
        if host_changed || orphaned {
            self.embedded.set(win::embed_in(handle, taskbar.handle));
            self.host.set(Some(taskbar.handle));
        }

        self.place(taskbar);
        self.embedded.get()
    }

    /// Sample the pointer, and say whether the readout has just been clicked.
    ///
    /// Called from a timer at [`POINTER_POLL`]. See this module's header for
    /// why the readout's clicks are read from Windows rather than delivered by
    /// Slint.
    ///
    /// A press only counts if the readout is the window under the pointer when
    /// the button goes down — `WindowFromPoint`, so a window on top of it takes
    /// the click as it should. The release is the click wherever the pointer
    /// has wandered to by then, the way a taskbar button treats one.
    pub fn poll_click(&self) -> bool {
        let down = win::left_button_down();
        match (down, self.pressed.get()) {
            (true, false) => {
                if let Some((x, y)) = win::cursor_position() {
                    let ours = self.handle.get();
                    if ours.is_some() && win::window_at(x, y) == ours {
                        self.pressed.set(true);
                    }
                }
                false
            }
            (false, true) => {
                self.pressed.set(false);
                true
            }
            _ => false,
        }
    }

    /// The readout's offset inside `taskbar`: stacked just clear of the
    /// notification area, clamped inside the bar. Computed fresh every time,
    /// which is what makes a growing tray push the readout along.
    fn offset_in(&self, taskbar: Taskbar) -> (i32, i32) {
        let size = self.physical_size();
        let along = Along::of(taskbar.rect);
        clamp_to_taskbar(default_offset(taskbar, size, along), size, taskbar.rect)
    }

    /// Move the readout to where it belongs, in whichever coordinate space it
    /// is living in.
    pub fn place(&self, taskbar: Taskbar) {
        let Some(handle) = self.handle.get() else {
            return;
        };
        let offset = self.offset_in(taskbar);

        if self.embedded.get() {
            // A child window's position is its parent's client space, and the
            // taskbar's client origin is not always its window origin.
            let (client_x, client_y) =
                win::to_client(taskbar.handle, taskbar.rect.left, taskbar.rect.top);
            win::move_window(handle, offset.0 + client_x, offset.1 + client_y);
        } else {
            win::move_window(
                handle,
                taskbar.rect.left + offset.0,
                taskbar.rect.top + offset.1,
            );
            win::keep_on_top(handle);
        }
    }

    /// Where the readout is on screen, for the detail panel to open beside.
    pub fn screen_rect(&self, taskbar: Taskbar) -> Rect {
        let (width, height) = self.physical_size();
        let (x, y) = self.offset_in(taskbar);
        Rect {
            left: taskbar.rect.left + x,
            top: taskbar.rect.top + y,
            right: taskbar.rect.left + x + width,
            bottom: taskbar.rect.top + y + height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::usage::{ClaudeLimits, CodexUsage, UsageLimit, WindowUsage};

    /// The user's own bar: docked left, 131 physical pixels wide, with the
    /// notification area in the bottom fifth. Measured, not invented.
    const VERTICAL: Taskbar = Taskbar {
        handle: 1,
        rect: Rect {
            left: 0,
            top: 0,
            right: 131,
            bottom: 2160,
        },
        notify: Rect {
            left: 0,
            top: 1753,
            right: 131,
            bottom: 2160,
        },
    };

    const HORIZONTAL: Taskbar = Taskbar {
        handle: 2,
        rect: Rect {
            left: 0,
            top: 1392,
            right: 2560,
            bottom: 1440,
        },
        notify: Rect {
            left: 2300,
            top: 1392,
            right: 2560,
            bottom: 1440,
        },
    };

    fn usage(claude: Option<f64>, codex: Option<f64>) -> UsageSnapshot {
        UsageSnapshot {
            claude: ClaudeLimits {
                limits: claude
                    .map(|percent| {
                        vec![
                            UsageLimit {
                                kind: "session".into(),
                                label: "Session".into(),
                                percent: 2.0,
                                resets_at: None,
                            },
                            UsageLimit {
                                kind: "weekly_all".into(),
                                label: "Week".into(),
                                percent,
                                resets_at: None,
                            },
                        ]
                    })
                    .unwrap_or_default(),
                fetched_at: None,
            },
            codex: codex.map(|percent| CodexUsage {
                primary: Some(WindowUsage {
                    used_percent: percent,
                    resets_at: None,
                    window_minutes: Some(300),
                }),
                secondary: None,
                plan_type: None,
                source: None,
            }),
            ..UsageSnapshot::default()
        }
    }

    #[test]
    fn the_orientation_comes_from_the_bars_own_shape() {
        assert_eq!(Along::of(VERTICAL.rect), Along::Vertical);
        assert_eq!(Along::of(HORIZONTAL.rect), Along::Horizontal);
        // A square bar is nobody's taskbar, but it must still answer.
        assert_eq!(
            Along::of(Rect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100
            }),
            Along::Horizontal
        );
    }

    /// Each agent shows the window that will stop it first, in the same colours
    /// the detail panel uses.
    #[test]
    fn each_agent_shows_its_tightest_window() {
        let both = chips(&usage(Some(31.0), Some(85.0)));
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].agent, Some(HookSource::Claude));
        assert_eq!((both[0].value.as_str(), both[0].tier), ("69%", "good"));
        assert_eq!(both[1].agent, Some(HookSource::Codex));
        assert_eq!((both[1].value.as_str(), both[1].tier), ("15%", "low"));

        // The middle band, and the boundary that decides it.
        assert_eq!(chips(&usage(Some(51.0), None))[0].tier, "warn");
        assert_eq!(chips(&usage(Some(50.0), None))[0].tier, "good");
        assert_eq!(chips(&usage(Some(80.0), None))[0].tier, "warn");
        assert_eq!(chips(&usage(Some(81.0), None))[0].tier, "low");
    }

    #[test]
    fn an_agent_with_no_reading_takes_no_room_at_all() {
        let one = chips(&usage(None, Some(7.0)));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].agent, Some(HookSource::Codex));
        assert_eq!(one[0].value, "93%");

        // And a readout with nothing to say says so rather than vanishing.
        let empty = chips(&UsageSnapshot::default());
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].agent, None);
        assert_eq!((empty[0].value.as_str(), empty[0].tier), ("--", ""));
    }

    #[test]
    fn the_readout_stacks_on_a_vertical_bar_and_lines_up_on_a_horizontal_one() {
        let (tall_w, tall_h) = bar_size(2, Along::Vertical);
        let (wide_w, wide_h) = bar_size(2, Along::Horizontal);
        // Same two chips, laid out for a bar that runs the other way: the
        // stacked one is the narrower and the taller of the pair.
        assert!(
            tall_w < wide_w && tall_h > wide_h,
            "stacked {tall_w}x{tall_h} against side-by-side {wide_w}x{wide_h}"
        );

        // Two chips take one more row than one, and one more column.
        let (one_w, one_h) = bar_size(1, Along::Vertical);
        assert_eq!(tall_w, one_w);
        assert_eq!(tall_h - one_h, CHIP_HEIGHT + CHIP_GAP);
        assert_eq!(bar_size(0, Along::Vertical), (one_w, one_h));

        // And it fits in the user's own 131-pixel bar at 150 % scaling.
        assert!(tall_w * 1.5 < (VERTICAL.rect.right - VERTICAL.rect.left) as f32);
    }

    #[test]
    fn the_readout_parks_just_clear_of_the_notification_area() {
        let size = (70, 50);
        let (x, y) = default_offset(VERTICAL, size, Along::Vertical);
        assert_eq!(y + size.1 + MARGIN, VERTICAL.notify.top);
        assert_eq!(x, (131 - 70) / 2, "centred across the bar");

        let (x, y) = default_offset(HORIZONTAL, (90, 30), Along::Horizontal);
        assert_eq!(x + 90 + MARGIN, HORIZONTAL.notify.left);
        assert_eq!(y, (48 - 30) / 2);
    }

    /// A shell whose notification area fills the whole bar would otherwise put
    /// the readout at a negative offset, which for a child window means
    /// invisible.
    #[test]
    fn a_crowded_taskbar_still_gets_a_readout_on_it() {
        let crowded = Taskbar {
            notify: Rect {
                left: 0,
                top: 0,
                right: 131,
                bottom: 2160,
            },
            ..VERTICAL
        };
        let (_, y) = default_offset(crowded, (70, 50), Along::Vertical);
        assert!(y >= MARGIN, "got {y}");
    }

    #[test]
    fn the_clamp_keeps_the_readout_inside_its_taskbar() {
        let size = (70, 50);
        assert_eq!(
            clamp_to_taskbar((-400, -400), size, VERTICAL.rect),
            (MARGIN, MARGIN)
        );
        assert_eq!(
            clamp_to_taskbar((9_000, 9_000), size, VERTICAL.rect),
            (131 - 70 - MARGIN, 2160 - 50 - MARGIN)
        );
        // A position that already fits is left exactly where it was put.
        assert_eq!(clamp_to_taskbar((30, 900), size, VERTICAL.rect), (30, 900));

        // A readout wider than its bar is centred rather than clamped to a
        // negative span.
        let (x, _) = clamp_to_taskbar((0, 900), (400, 50), VERTICAL.rect);
        assert_eq!(x, (131 - 400) / 2);
    }
}
