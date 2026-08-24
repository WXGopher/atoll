//! The card's own window: its size, and where on the screen it opens.
//!
//! Atoll has no resting window any more. The numbers live in the taskbar
//! readout and the details behind a click; the card exists only while a session
//! is actually asking a human something, and the window is created, placed, and
//! taken away with it.
//!
//! Where it opens is the interesting part. The card belongs to the readout, so
//! it appears beside the taskbar — clear of the bar, level with the readout,
//! opening toward the middle of the screen.
//! Once the user has dragged it, it opens where they left it instead: somebody
//! who moved a card moved it because that corner was in the way.
//!
//! The arithmetic is in free functions with no window in sight, so the
//! interesting half is testable without a display.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::ComponentHandle;

use super::ui::CardWindow;
use super::win::{self, Rect};

/// Matches `title:` in `ui/card.slint`; the Win32 lookup keys off it.
pub const WINDOW_TITLE: &str = "Atoll Card";

/// All logical pixels, and all of them mirrored in `ui/card.slint`.
pub const CARD_WIDTH: f32 = 340.0;
/// How far a card stays from the taskbar, and from the screen's edges.
const MARGIN: i32 = 10;

/// Which card is on screen, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Approval,
    Question {
        /// One button per option.
        options: usize,
        /// How many lines the question text wraps to. Rust has to know, because
        /// Rust is what sizes the window the text is laid out inside.
        lines: usize,
    },
    Completed,
}

impl CardKind {
    /// The discriminant `ui/card.slint` switches on.
    pub fn as_int(self) -> i32 {
        match self {
            CardKind::Approval => 1,
            CardKind::Question { .. } => 2,
            CardKind::Completed => 3,
        }
    }
}

/// The card body's usable width, in characters of the 13 px body font. Used to
/// predict how many lines a question will wrap to; see [`card_height`].
pub const BODY_COLUMNS: usize = 44;
/// Past this the body is elided rather than grown.
pub const MAX_BODY_LINES: usize = 3;
const BODY_LINE: f32 = 18.0;

/// The card's height, which with [`CARD_WIDTH`] is the whole window.
pub fn card_height(kind: CardKind) -> f32 {
    // padding + heading + spacing + body …
    let base = 28.0 + 16.0 + 8.0;
    match kind {
        // … + spacing + one row of buttons. The summary of a tool input is
        // always elided to one line: it is a reminder of what was asked, not the
        // thing itself.
        CardKind::Approval => base + BODY_LINE + 8.0 + 30.0,
        // … + spacing + one button per option.
        CardKind::Question { options, lines } => {
            let options = options.max(1) as f32;
            let lines = lines.clamp(1, MAX_BODY_LINES) as f32;
            base + lines * BODY_LINE + 8.0 + options * 30.0 + (options - 1.0) * 6.0
        }
        CardKind::Completed => base + BODY_LINE,
    }
}

/// How many lines `text` will wrap to inside a card.
///
/// An estimate, and the reason is that the window has to be sized before Slint
/// lays the text out inside it. Greedy word wrapping over [`BODY_COLUMNS`],
/// counting a non-ASCII character as two columns because that is roughly how
/// wide CJK glyphs render at this size.
pub fn body_lines(text: &str) -> usize {
    let mut lines = 1usize;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let width = text_columns(word);
        if used == 0 {
            used = width;
        } else if used + 1 + width <= BODY_COLUMNS {
            used += 1 + width;
        } else {
            lines += 1;
            used = width;
        }
        // A single unbroken word — a long path, a URL — wraps mid-word.
        while used > BODY_COLUMNS {
            lines += 1;
            used -= BODY_COLUMNS;
        }
        if lines >= MAX_BODY_LINES {
            return MAX_BODY_LINES;
        }
    }
    lines
}

fn text_columns(text: &str) -> usize {
    text.chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum()
}

/// Where a card opens when the user has never moved one: beside the taskbar
/// readout, clear of the bar, opening toward the middle of the screen.
///
/// `anchor` is the readout's own rectangle and `bar` the taskbar's, both in
/// physical pixels. A bar down the left edge puts the card to its right; one
/// along the bottom puts it above. Everything is clamped into the work area,
/// because a card that opened off-screen would hold a session open with no way
/// to answer it.
pub fn place_beside(anchor: Rect, bar: Rect, size: (i32, i32), area: Rect) -> (i32, i32) {
    let (width, height) = size;
    let vertical = (bar.bottom - bar.top) > (bar.right - bar.left);

    let (x, y) = if vertical {
        // Out from whichever edge the bar is docked to, and level with the
        // readout — the card lines up with the number it is about.
        let on_the_left = (bar.left + bar.right) / 2 < (area.left + area.right) / 2;
        let x = if on_the_left {
            bar.right + MARGIN
        } else {
            bar.left - MARGIN - width
        };
        (x, anchor.top)
    } else {
        let on_top = (bar.top + bar.bottom) / 2 < (area.top + area.bottom) / 2;
        let y = if on_top {
            bar.bottom + MARGIN
        } else {
            bar.top - MARGIN - height
        };
        (anchor.left, y)
    };

    (
        clamp_span(x, width, area.left, area.right),
        clamp_span(y, height, area.top, area.bottom),
    )
}

/// Keep a card the user dragged somewhere on the screen they can reach it.
pub fn clamp_to_screen(position: (i32, i32), size: (i32, i32), area: Rect) -> (i32, i32) {
    (
        clamp_span(position.0, size.0, area.left, area.right),
        clamp_span(position.1, size.1, area.top, area.bottom),
    )
}

fn clamp_span(start: i32, length: i32, low: i32, high: i32) -> i32 {
    let first = low + MARGIN;
    if length + MARGIN * 2 >= high - low {
        return low;
    }
    start.clamp(first, high - length - MARGIN)
}

/// The card's window, plus the position state Slint does not keep for us.
pub struct CardView {
    /// The window, which exists only while a card does.
    ///
    /// Created per card and dropped with it, rather than kept and hidden. That
    /// is not tidiness: a Slint window that is hidden while the pointer is
    /// inside it — which is always, since a card is answered by clicking a
    /// button on it — leaves the pointer with the hidden window, and no other
    /// window of ours receives a click again. Dropping the component destroys
    /// the window outright, and the pointer goes back to whatever is under it.
    ///
    /// The symptom, before this: answer one approval and the taskbar readout
    /// stops opening the detail panel until Atoll is restarted.
    ui: RefCell<Option<CardWindow>>,
    /// The window's top-left in physical pixels.
    position: Cell<(i32, i32)>,
    /// Whether the user has ever dragged a card. Once they have, cards open
    /// where they put the last one rather than back beside the taskbar.
    placed: Cell<bool>,
    kind: Cell<Option<CardKind>>,
    /// Found once the event loop has really created the window.
    handle: Cell<Option<isize>>,
    /// What the monitor's scale was the last time a window existed to ask.
    /// Kept so a card can be sized before there is a window to measure with.
    scale: Cell<f32>,
}

impl CardView {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            ui: RefCell::new(None),
            position: Cell::new((0, 0)),
            placed: Cell::new(false),
            kind: Cell::new(None),
            handle: Cell::new(None),
            scale: Cell::new(1.0),
        })
    }

    /// The open card's window, if there is one.
    pub fn ui(&self) -> Option<CardWindow> {
        self.ui.borrow().as_ref().map(ComponentHandle::clone_strong)
    }

    pub fn kind(&self) -> Option<CardKind> {
        self.kind.get()
    }

    pub fn is_shown(&self) -> bool {
        self.ui.borrow().is_some()
    }

    /// Where the user left the last card, for the config to remember.
    pub fn position(&self) -> (i32, i32) {
        self.position.get()
    }

    /// Restore a remembered position. Cards then open there rather than beside
    /// the taskbar.
    pub fn restore(&self, position: (i32, i32)) {
        self.position.set(position);
        self.placed.set(true);
    }

    fn physical_size(&self, kind: CardKind) -> (i32, i32) {
        let scale = self.scale.get();
        (
            (CARD_WIDTH * scale).round() as i32,
            (card_height(kind) * scale).round() as i32,
        )
    }

    /// Put a card on screen, or take the one that is there away.
    ///
    /// `anchor` and `bar` are the taskbar readout's rectangle and the taskbar's,
    /// which is what a card that has never been dragged opens beside. `wire` is
    /// called on a window the moment it is created, to hook up its callbacks —
    /// each card gets a new window, so each one has to be wired.
    pub fn set_card(
        &self,
        kind: Option<CardKind>,
        anchor: Option<(Rect, Rect)>,
        wire: impl FnOnce(&CardWindow),
    ) -> Option<CardWindow> {
        let Some(kind) = kind else {
            self.hide();
            return None;
        };
        let fresh = self.ui.borrow().is_none();
        if fresh {
            let Ok(window) = CardWindow::new() else {
                return None;
            };
            wire(&window);
            *self.ui.borrow_mut() = Some(window);
            self.handle.set(None);
        }
        let window = self.ui()?;
        self.kind.set(Some(kind));
        let scale = window.window().scale_factor();
        if scale > 0.0 {
            self.scale.set(scale);
        }

        let size = self.physical_size(kind);
        let area = win::work_area_at(self.position.get().0, self.position.get().1);
        let position = if self.placed.get() {
            clamp_to_screen(self.position.get(), size, area)
        } else {
            match anchor {
                Some((readout, bar)) => {
                    let area = win::work_area_at(bar.left, bar.top);
                    place_beside(readout, bar, size, area)
                }
                // No taskbar to open beside: the middle of the work area beats
                // a corner nobody is looking at.
                None => (
                    (area.left + area.right - size.0) / 2,
                    (area.top + area.bottom - size.1) / 2,
                ),
            }
        };
        self.position.set(position);

        // Geometry first, then show: a window that is mapped before it knows
        // its size draws one frame at the wrong one, and at this size that is a
        // flash of the wrong shape rather than a subtlety.
        window
            .window()
            .set_size(slint::LogicalSize::new(CARD_WIDTH, card_height(kind)));
        window
            .window()
            .set_position(slint::PhysicalPosition::new(position.0, position.1));
        if fresh {
            let _ = window.show();
        }
        if let Some(handle) = self.handle.get() {
            win::keep_on_top(handle);
        }
        Some(window)
    }

    /// Move by a pointer delta in logical pixels, mid-drag.
    pub fn drag_by(&self, dx: f32, dy: f32) {
        let Some(kind) = self.kind.get() else { return };
        let Some(window) = self.ui() else {
            return;
        };
        let scale = self.scale.get();
        let (x, y) = self.position.get();
        let moved = (
            x + (dx * scale).round() as i32,
            y + (dy * scale).round() as i32,
        );
        let area = win::work_area_at(moved.0, moved.1);
        let position = clamp_to_screen(moved, self.physical_size(kind), area);
        self.position.set(position);
        self.placed.set(true);
        window
            .window()
            .set_position(slint::PhysicalPosition::new(position.0, position.1));
    }

    /// Take the card out of the taskbar and the Alt-Tab list, once.
    ///
    /// Slint only creates the window once the event loop is spinning, so the
    /// caller polls this from a timer. It is a poll rather than a one-shot
    /// because the window may in principle be recreated — but the tweak itself
    /// runs only when the handle is one it has not seen, because the tweak is a
    /// hide-and-show cycle and doing that to a window that is already up is a
    /// visible blink.
    pub fn adopt_window(&self) {
        let Some(handle) = win::window_by_title(WINDOW_TITLE) else {
            return;
        };
        if self.handle.get() == Some(handle) {
            return;
        }
        self.handle.set(Some(handle));
        win::hide_from_taskbar(handle);
    }

    /// Take the card away, destroying its window.
    ///
    /// Dropping the component rather than hiding it: see the note on [`ui`].
    ///
    /// [`ui`]: Self::ui
    pub fn hide(&self) {
        self.kind.set(None);
        self.handle.set(None);
        let Some(window) = self.ui.borrow_mut().take() else {
            return;
        };
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3840 × 2160 screen with a 131-pixel taskbar down the left edge — the
    /// machine this was built against.
    const SCREEN: Rect = Rect {
        left: 131,
        top: 0,
        right: 3840,
        bottom: 2160,
    };
    const LEFT_BAR: Rect = Rect {
        left: 0,
        top: 0,
        right: 131,
        bottom: 2160,
    };
    const READOUT: Rect = Rect {
        left: 31,
        top: 1724,
        right: 99,
        bottom: 1783,
    };

    #[test]
    fn a_card_opens_out_from_the_taskbar_and_level_with_the_readout() {
        let size = (510, 213);
        let (x, y) = place_beside(READOUT, LEFT_BAR, size, SCREEN);
        assert_eq!(x, LEFT_BAR.right + MARGIN, "clear of the bar");
        assert_eq!(y, READOUT.top, "level with the number it is about");
    }

    #[test]
    fn a_bar_on_the_other_edge_opens_the_card_the_other_way() {
        let right_bar = Rect {
            left: 3709,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        let area = Rect {
            right: 3709,
            ..SCREEN
        };
        let size = (510, 213);
        let (x, _) = place_beside(READOUT, right_bar, size, area);
        assert_eq!(x, right_bar.left - MARGIN - size.0);
    }

    #[test]
    fn a_horizontal_taskbar_opens_the_card_above_or_below_itself() {
        let bottom_bar = Rect {
            left: 0,
            top: 1392,
            right: 2560,
            bottom: 1440,
        };
        let area = Rect {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1392,
        };
        let readout = Rect {
            left: 2100,
            top: 1400,
            right: 2200,
            bottom: 1432,
        };
        let size = (340, 140);
        let (x, y) = place_beside(readout, bottom_bar, size, area);
        assert_eq!(x, readout.left);
        assert_eq!(y, bottom_bar.top - MARGIN - size.1);

        // And a bar along the top drops the card below it instead.
        let top_bar = Rect {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 48,
        };
        let area = Rect { top: 48, ..area };
        let (_, y) = place_beside(readout, top_bar, size, area);
        assert_eq!(y, top_bar.bottom + MARGIN);
    }

    /// A card that opened off-screen would hold a session open with no way to
    /// answer it, so nothing may put one there — not a taskbar in an odd place,
    /// and not a drag.
    #[test]
    fn a_card_never_opens_where_it_cannot_be_answered() {
        let size = (510, 213);
        // A readout near the bottom of a tall bar: the card would hang off.
        let low = Rect {
            top: 2140,
            bottom: 2159,
            ..READOUT
        };
        let (_, y) = place_beside(low, LEFT_BAR, size, SCREEN);
        assert!(y + size.1 <= SCREEN.bottom, "got {y}");

        assert_eq!(clamp_to_screen((-9_000, -9_000), size, SCREEN), (141, 10));
        let (x, y) = clamp_to_screen((9_000, 9_000), size, SCREEN);
        assert_eq!((x + size.0, y + size.1), (3830, 2150));
        // A position that already fits is left exactly where it was put.
        assert_eq!(clamp_to_screen((500, 500), size, SCREEN), (500, 500));
    }

    #[test]
    fn a_wrapped_question_gets_a_taller_card() {
        let short = card_height(CardKind::Question {
            options: 2,
            lines: 1,
        });
        let wrapped = card_height(CardKind::Question {
            options: 2,
            lines: 2,
        });
        assert_eq!(wrapped - short, 18.0);
        // The clamp holds even if a caller reports nonsense.
        assert_eq!(
            card_height(CardKind::Question {
                options: 2,
                lines: 99
            }),
            card_height(CardKind::Question {
                options: 2,
                lines: MAX_BODY_LINES
            })
        );
    }

    #[test]
    fn a_question_card_grows_one_row_per_option() {
        let one = card_height(CardKind::Question {
            options: 1,
            lines: 1,
        });
        let four = card_height(CardKind::Question {
            options: 4,
            lines: 1,
        });
        assert_eq!(four - one, 3.0 * 36.0);
        assert!(card_height(CardKind::Completed) < card_height(CardKind::Approval));
        // Even a malformed question with no options gets a card with room for
        // one, rather than a zero-height sliver.
        assert_eq!(
            card_height(CardKind::Question {
                options: 0,
                lines: 1,
            }),
            one
        );
    }

    #[test]
    fn the_body_line_estimate_follows_the_wrapping() {
        assert_eq!(body_lines(""), 1);
        assert_eq!(body_lines("Which database?"), 1);
        // 43 columns: still one line.
        assert_eq!(
            body_lines("Which database should the pipeline write to?"),
            1
        );
        assert_eq!(
            body_lines(
                "Which database should the pipeline write to, given that it has to survive a restart?"
            ),
            2
        );
        // A CJK question is twice as wide per character.
        assert_eq!(body_lines("这个流水线应该写到哪个数据库里去呢"), 1);
        assert_eq!(
            body_lines("这个流水线应该写到哪个数据库里去呢这个流水线应该写到哪个数据库"),
            2
        );
        // One unbroken token still wraps rather than running off the card.
        assert!(body_lines(&"x".repeat(100)) > 1);
        // And nothing grows past the cap.
        assert_eq!(body_lines(&"word ".repeat(200)), MAX_BODY_LINES);
    }
}
