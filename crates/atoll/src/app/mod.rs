//! The app: the taskbar readout, the detail panel, the cards, the tray icon,
//! the settings window, and the pipe server that feeds them all.
//!
//! # Shape
//!
//! One thread runs tokio and the named pipe ([`bridge`]); one thread runs Slint
//! and everything else. They meet at a channel of [`bridge::HookEvent`]s and a
//! wake-up through `invoke_from_event_loop`. Every piece of mutable state below
//! lives on the UI thread behind `RefCell`, which is safe precisely because
//! there is only ever one thread touching it.
//!
//! # Cards
//!
//! The card is the only window Atoll opens on its own initiative, and it exists
//! only while a session is actually asking a human something. There is no
//! resting window: the numbers live in the taskbar readout, and the details are
//! one click behind it.
//!
//! At most one card is on screen at a time. Events that need a human queue up
//! behind it, so an agent that fires three approvals in a row does not stack
//! three windows on the desktop — the user works through them one at a time, and
//! the card says how many are still behind it.
//!
//! Only `PermissionRequest` raises a card. `PreToolUse` is acked the moment it
//! reaches the pipe thread and used for nothing but keeping the session table
//! current; see [`bridge::Forwarder::on_envelope`] for why.

mod bridge;
mod card;
mod cardview;
pub(crate) mod config;
mod icon;
pub mod net;
mod settings;
mod taskbar;
mod tray;
mod win;

/// The Slint markup, compiled by `build.rs`.
pub mod ui {
    slint::include_modules!();
}

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atoll_core::now_unix_secs;
use atoll_core::protocol::{Envelope, HookSource, Response, events};
use atoll_core::server::ConnectionHandle;
use atoll_core::state::{Phase, SessionTable};
use atoll_core::transcript;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use self::bridge::HookEvent;
use self::card::Card;
use self::cardview::{CardKind, CardView};
use self::config::Config;
use self::icon::IconState;
use self::taskbar::TaskbarView;
use self::tray::{Tray, TrayCommand};
use self::win::Rect;
use crate::out::errln;
use crate::usage_cache::UsageSnapshot;
use crate::util::{clean_title, project_name, truncate};

/// The housekeeping beat: sweeps dead sessions, re-reads usage, keeps the tray
/// count honest.
const TICK: Duration = Duration::from_millis(500);
/// How often the tray's channels are read. Short, because this is the latency
/// between clicking a menu item and it happening.
const TRAY_POLL: Duration = Duration::from_millis(100);
/// How often a new window is looked for so it can be taken out of the taskbar.
const ADOPT_POLL: Duration = Duration::from_millis(150);
/// One full breath of the waiting animation.
const PULSE_PERIOD_MS: u128 = 1_400;

/// Every agent the panel has a row for, in the order the rows appear.
///
/// Adding an agent here is all it takes for the panel to include it: nothing
/// below names an agent of its own.
const AGENTS: [HookSource; 2] = [HookSource::Claude, HookSource::Codex];

const FLYOUT_TITLE: &str = "Atoll Sessions";
const FLYOUT_WIDTH: f32 = 320.0;
const FLYOUT_MARGIN: i32 = 8;

thread_local! {
    /// The running app, reachable from the `Send` closure the pipe thread posts
    /// to the event loop. Only ever set on the UI thread.
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Drain whatever the pipe thread has queued. Called from the event loop.
pub fn pump() {
    let app = APP.with(|slot| slot.borrow().clone());
    if let Some(app) = app {
        app.drain();
    }
}

pub fn run() -> io::Result<()> {
    // `atoll` is a console binary so `atoll setup` and `atoll headless` can
    // print. The app has nothing to say on a terminal, so it lets go of the one
    // it was started with rather than leaving a black window behind.
    // Bind the pipe before anything else: it is Atoll's identity, and starting
    // takes it from whatever Atoll already has it — see [`crate::single`].
    let bridge = bridge::start()?;
    if bridge.replaced {
        errln!("atoll: an Atoll was already running; it stood down");
    }
    errln!("atoll: listening on {}", bridge.path);
    win::detach_console();

    let app = App::new(bridge)?;
    APP.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&app)));
    app.start();

    // Not `run_event_loop`: that returns once the last window closes, and a
    // tray-only Atoll has no windows open at all.
    let result = slint::run_event_loop_until_quit().map_err(io::Error::other);

    APP.with(|slot| slot.borrow_mut().take());
    result
}

struct App {
    /// The card's window, created empty and shown only when there is a card.
    card_view: Rc<CardView>,
    /// The readout in the taskbar. Its own module owns the reparenting.
    bar: Rc<TaskbarView>,
    /// The taskbar as it was last seen, so a shell restart shows up as a change
    /// rather than as a readout that quietly stops being anywhere.
    host: Cell<Option<win::Taskbar>>,
    flyout: ui::FlyoutWindow,
    flyout_open: Cell<bool>,
    /// The panel's OS window, once it has been taken out of the taskbar. Kept
    /// so that is done once and not once per opening: the tweak is a hide-and-
    /// show cycle, and running it on a window the user has just opened is a
    /// visible blink a moment after it appears.
    flyout_handle: Cell<Option<isize>>,
    settings_window: RefCell<Option<ui::SettingsWindow>>,
    tray: RefCell<Option<Tray>>,

    table: RefCell<SessionTable>,
    /// Hooks that are blocked on us, keyed by session and correlation key.
    /// Holding the handle is what keeps the agent waiting.
    blocked: RefCell<HashMap<(String, String), ConnectionHandle>>,
    queue: RefCell<VecDeque<Card>>,
    current: RefCell<Option<Card>>,
    /// Whether the pointer has ever been on the current card. A card the user
    /// has looked at collapses on its own; one they have not stays.
    touched: Cell<bool>,
    /// Session titles read from transcripts. Filled in from a worker, so the
    /// panel opens with whatever was already known and gains the rest a moment
    /// later rather than waiting for a directory of files to be read.
    titles: RefCell<HashMap<String, String>>,
    titles_tx: std::sync::mpsc::Sender<HashMap<String, String>>,
    titles_rx: std::sync::mpsc::Receiver<HashMap<String, String>>,
    /// What the last scan read, so the next one only opens what has changed.
    /// Shared with the worker, which is the only thread that touches it.
    transcripts: Arc<Mutex<transcript::TranscriptCache>>,
    /// Whether a scan is already running. One at a time: a second would read the
    /// same files to the same answer.
    scanning: Cell<bool>,

    usage: RefCell<UsageSnapshot>,
    /// Claude's usage arrives over the network, so it comes back on a channel
    /// rather than being read inline: an eight-second timeout on the UI thread
    /// would be eight seconds of frozen interface.
    limits_tx: std::sync::mpsc::Sender<atoll_core::usage::ClaudeLimits>,
    limits_rx: std::sync::mpsc::Receiver<atoll_core::usage::ClaudeLimits>,
    fetching: Cell<bool>,
    config: RefCell<Config>,
    events: std::sync::mpsc::Receiver<HookEvent>,

    started: Instant,
    tick: slint::Timer,
    /// Samples the pointer over the taskbar readout; see [`taskbar`].
    pointer_timer: slint::Timer,
    tray_timer: slint::Timer,
    adopt_timer: slint::Timer,
    dismiss_timer: slint::Timer,
}

impl App {
    fn new(bridge: bridge::Bridge) -> io::Result<Rc<Self>> {
        let bar = ui::TaskbarBar::new().map_err(io::Error::other)?;
        let flyout = ui::FlyoutWindow::new().map_err(io::Error::other)?;
        let config = Config::load();
        let (limits_tx, limits_rx) = std::sync::mpsc::channel();
        let (titles_tx, titles_rx) = std::sync::mpsc::channel();

        let app = Rc::new(Self {
            card_view: CardView::new(),
            bar: TaskbarView::new(bar),
            host: Cell::new(None),
            flyout,
            flyout_open: Cell::new(false),
            flyout_handle: Cell::new(None),
            settings_window: RefCell::new(None),
            tray: RefCell::new(None),
            table: RefCell::new(SessionTable::new()),
            blocked: RefCell::new(HashMap::new()),
            queue: RefCell::new(VecDeque::new()),
            current: RefCell::new(None),
            touched: Cell::new(false),
            titles: RefCell::new(HashMap::new()),
            titles_tx,
            titles_rx,
            transcripts: Arc::new(Mutex::new(transcript::TranscriptCache::new())),
            scanning: Cell::new(false),
            usage: RefCell::new(UsageSnapshot::default()),
            limits_tx,
            limits_rx,
            fetching: Cell::new(false),
            config: RefCell::new(config),
            events: bridge.events,
            started: Instant::now(),
            tick: slint::Timer::default(),
            pointer_timer: slint::Timer::default(),
            tray_timer: slint::Timer::default(),
            adopt_timer: slint::Timer::default(),
            dismiss_timer: slint::Timer::default(),
        });

        Ok(app)
    }

    // ------------------------------------------------------------ start-up

    fn start(self: &Rc<Self>) {
        {
            let config = self.config.borrow();
            if let Some(position) = config.card_position() {
                self.card_view.restore(position);
            }
            if config.taskbar.enabled {
                self.bar.show();
            }
        }
        self.refresh();
        self.migrate_startup_shortcut();

        self.start_adopting();
        self.start_watching_the_readout();
        self.start_ticking();
        self.start_tray();
    }

    /// Take whatever window is currently open out of the taskbar and the
    /// Alt-Tab list.
    ///
    /// Slint creates the OS window only once the event loop is running, and the
    /// card's window comes and goes with the cards, so this runs for as long as
    /// the app does rather than stopping after the first success.
    fn start_adopting(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.adopt_timer
            .start(slint::TimerMode::Repeated, ADOPT_POLL, move || {
                let Some(app) = app.upgrade() else { return };
                if app.card_view.is_shown() {
                    app.card_view.adopt_window();
                }
                app.adopt_flyout();
            });
    }

    /// Take the detail panel out of the taskbar and the Alt-Tab list, once.
    ///
    /// The panel's native window exists from startup — it is created ahead of
    /// its first opening so a click opens it instantly — and a hidden window
    /// with the app-window style is a phantom taskbar button waiting to
    /// happen. So it is adopted as soon as it can be found, while it is still
    /// hidden: the style bits go on silently and take effect at the first
    /// `show`, with no hide-and-show cycle and no button ever appearing. The
    /// cycle is only needed for the case that should no longer arise — a
    /// window that got shown before it could be adopted.
    ///
    /// Once, and not once per opening: Slint hides the native window rather
    /// than destroying it, so the handle stays good from one opening to the
    /// next.
    fn adopt_flyout(&self) {
        let Some(handle) = win::window_by_title(FLYOUT_TITLE) else {
            return;
        };
        if self.flyout_handle.get() == Some(handle) {
            return;
        }
        self.flyout_handle.set(Some(handle));
        if self.flyout_open.get() {
            win::hide_from_taskbar(handle);
        } else {
            win::mark_tool_window(handle);
        }
    }

    /// Keep the readout attached to whatever taskbar currently exists.
    ///
    /// Explorer restarts — on a crash, on a settings change, on an update — and
    /// takes `Shell_TrayWnd` with it. Our window survives the loss of its
    /// parent, orphaned and invisible, and the only way back is to notice and
    /// re-adopt. Polling rather than listening for `TaskbarCreated`: the
    /// broadcast needs a window procedure of our own, which Slint owns, and this
    /// costs one `FindWindow` per tick.
    fn watch_the_taskbar(&self) {
        if !self.bar.is_shown() {
            return;
        }
        let taskbar = win::taskbar();
        let changed = self.host.get().map(|host| host.handle) != taskbar.map(|found| found.handle);
        self.host.set(taskbar);
        if changed && taskbar.is_none() {
            // The shell is mid-restart. Leave the window where it is; the next
            // tick that finds a taskbar will re-adopt it.
            return;
        }
        let Some(taskbar) = taskbar else { return };
        // `attach` re-places the readout every tick, which is what keeps it
        // stacked clear of a notification area that grows and shrinks.
        self.bar.attach(Some(taskbar));
    }

    fn start_ticking(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.tick.start(slint::TimerMode::Repeated, TICK, move || {
            let Some(app) = app.upgrade() else { return };
            app.housekeeping();
        });
    }

    /// The tray icon is created from a timer for the same reason the taskbar
    /// tweak is: `tray-icon` wants a running message loop on this thread.
    fn start_tray(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.tray_timer
            .start(slint::TimerMode::Repeated, TRAY_POLL, move || {
                let Some(app) = app.upgrade() else { return };
                if app.tray.borrow().is_none() {
                    match Tray::new(win::small_icon_size()) {
                        Ok(tray) => {
                            *app.tray.borrow_mut() = Some(tray);
                        }
                        Err(error) => {
                            // Losing the tray is a degraded app, not a dead one:
                            // the readout and the cards still work. Say so once
                            // and stop retrying, rather than failing every
                            // 100 ms forever.
                            errln!("atoll: could not create the tray icon: {error}");
                            app.tray_timer.stop();
                            return;
                        }
                    }
                }
                app.handle_tray();
                app.refresh_tray_icon();
                // The readout's breathing dot rides the same 100 ms beat; a
                // readout with no task line ignores this without repainting.
                app.bar.breathe(app.pulse());
            });
    }

    // --------------------------------------------------------- card callbacks

    /// Hook up one card window's callbacks.
    ///
    /// Called per window rather than once, because each card gets a window of
    /// its own; see [`cardview::CardView::ui`].
    fn wire_card(self: &Rc<Self>, ui: &ui::CardWindow) {
        let app = Rc::downgrade(self);
        ui.on_drag_delta(move |dx, dy| {
            if let Some(app) = app.upgrade() {
                app.card_view.drag_by(dx, dy);
            }
        });

        let app = Rc::downgrade(self);
        ui.on_drag_done(move || {
            if let Some(app) = app.upgrade() {
                let (x, y) = app.card_view.position();
                let mut config = app.config.borrow_mut();
                config.set_card_position(x, y);
                config.save();
            }
        });

        let app = Rc::downgrade(self);
        ui.on_decide(move |allow| {
            if let Some(app) = app.upgrade() {
                app.decide(allow);
            }
        });

        let app = Rc::downgrade(self);
        ui.on_answer(move |index| {
            if let Some(app) = app.upgrade() {
                app.answer(index);
            }
        });

        let app = Rc::downgrade(self);
        ui.on_hover(move |inside| {
            if let Some(app) = app.upgrade() {
                app.on_hover(inside);
            }
        });
    }

    /// Watch the pointer over the taskbar readout: a click opens the detail
    /// panel, a drag moves the readout along the bar.
    ///
    /// A poll rather than a callback, and the reason is worth knowing: see the
    /// header of [`taskbar`].
    fn start_watching_the_readout(self: &Rc<Self>) {
        let app = Rc::downgrade(self);
        self.pointer_timer.start(
            slint::TimerMode::Repeated,
            taskbar::POINTER_POLL,
            move || {
                let Some(app) = app.upgrade() else { return };
                if !app.bar.is_shown() {
                    return;
                }
                match app.bar.poll_click() {
                    Some(taskbar::Click::Toggle) => app.toggle_flyout_beside_bar(),
                    Some(taskbar::Click::Menu) => app.readout_menu(),
                    None => {}
                }
            },
        );
    }

    /// The readout's right-click menu: the same two commands the tray offers,
    /// in the same words. A native menu, so it dismisses like every other
    /// taskbar menu and never fights the panel for space.
    fn readout_menu(self: &Rc<Self>) {
        let Some(handle) = self.bar.window_handle() else {
            return;
        };
        match win::popup_menu(handle, &["Settings…", "-", "Quit Atoll"]) {
            Some(0) => self.open_settings(),
            Some(2) => {
                self.close_flyout();
                slint::quit_event_loop().ok();
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------- events

    /// Take everything the pipe thread has queued.
    fn drain(self: &Rc<Self>) {
        // The title worker posts to the event loop the same way the pipe thread
        // does, so this is where its answers land too.
        let titles_arrived = self.collect_titles();

        let mut received = 0usize;
        while let Ok(event) = self.events.try_recv() {
            self.on_hook(event);
            received += 1;
        }
        if received > 0 {
            self.promote();
        }
        if received > 0 || titles_arrived {
            self.refresh();
        }
    }

    /// Take whatever the title worker has finished.
    fn collect_titles(&self) -> bool {
        let mut arrived = false;
        while let Ok(titles) = self.titles_rx.try_recv() {
            *self.titles.borrow_mut() = titles;
            self.scanning.set(false);
            arrived = true;
        }
        arrived
    }

    fn on_hook(self: &Rc<Self>, event: HookEvent) {
        let HookEvent {
            payload,
            source,
            reply,
        } = event;
        let now = now_unix_secs();

        // Register the waiting hook before anything else can fail: a card with
        // no connection behind it is a button that does nothing. Only a
        // `PermissionRequest` arrives with one — see [`bridge::Forwarder`] and
        // [`Card::for_request`] for why `PreToolUse` is already gone by now.
        if let Some(handle) = reply
            && let Some(card) = Card::for_request(&payload, source, now)
        {
            self.blocked
                .borrow_mut()
                .insert((card.session_id.clone(), card.key.clone()), handle);
            self.queue.borrow_mut().push_back(card);
        } else if payload.event_name() == events::STOP && payload.session_id.is_some() {
            let summary = transcript_summary(payload.transcript_path.as_deref());
            self.queue.borrow_mut().push_back(Card::completed(
                &payload,
                source,
                summary.as_deref(),
                now,
            ));
        }

        self.table.borrow_mut().apply(&payload, source, now);

        // A `PostToolUse`, a `Stop`, or a new turn can settle the very approval
        // the open card is asking about — the user answered in the terminal, or
        // the agent gave up. Take the card down rather than leaving a button
        // that would answer a question nobody is asking any more.
        if self.current_is_stale() {
            self.dismiss();
        }
    }

    fn current_is_stale(&self) -> bool {
        let current = self.current.borrow();
        let Some(card) = current.as_ref() else {
            return false;
        };
        if !card.needs_an_answer() {
            return false;
        }
        let settled = self
            .table
            .borrow()
            .get(&card.session_id)
            .map(|state| !state.pending.iter().any(|pending| pending.key == card.key))
            .unwrap_or(true);
        let hung_up = self
            .blocked
            .borrow()
            .get(&(card.session_id.clone(), card.key.clone()))
            .map(|handle| !handle.is_open())
            .unwrap_or(true);
        settled || hung_up
    }

    // --------------------------------------------------------------- cards

    /// Put the next queued card on screen, if nothing is there already.
    fn promote(self: &Rc<Self>) {
        if self.current.borrow().is_some() {
            return;
        }
        loop {
            let Some(card) = self.queue.borrow_mut().pop_front() else {
                return;
            };
            // Skip a card whose hook has already given up waiting.
            if card.needs_an_answer() {
                let open = self
                    .blocked
                    .borrow()
                    .get(&(card.session_id.clone(), card.key.clone()))
                    .map(ConnectionHandle::is_open)
                    .unwrap_or(false);
                if !open {
                    continue;
                }
            }
            self.touched.set(false);
            let kind = card.kind;
            *self.current.borrow_mut() = Some(card);
            self.arm_dismissal(kind);
            return;
        }
    }

    /// A finished turn takes itself down after a moment. An unanswered approval
    /// does not: it is the whole reason Atoll exists.
    fn arm_dismissal(self: &Rc<Self>, kind: CardKind) {
        if kind != CardKind::Completed {
            self.dismiss_timer.stop();
            return;
        }
        let app = Rc::downgrade(self);
        self.dismiss_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(card::COMPLETED_DWELL_SECS),
            move || {
                if let Some(app) = app.upgrade() {
                    app.dismiss();
                }
            },
        );
    }

    fn on_hover(self: &Rc<Self>, inside: bool) {
        let Some(kind) = self.card_view.kind() else {
            return;
        };
        if kind == CardKind::Completed {
            return;
        }
        if inside {
            self.touched.set(true);
            self.dismiss_timer.stop();
            return;
        }
        if !self.touched.get() {
            return;
        }
        // The user has seen this card and moved on. Give them a while to come
        // back, then get out of the way — the approval itself stays pending, and
        // the tray icon keeps counting it.
        let app = Rc::downgrade(self);
        self.dismiss_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(card::HOVER_DWELL_SECS),
            move || {
                if let Some(app) = app.upgrade() {
                    app.dismiss();
                }
            },
        );
    }

    /// Take the current card down and show whatever was behind it.
    fn dismiss(self: &Rc<Self>) {
        self.dismiss_timer.stop();
        self.current.borrow_mut().take();
        self.show_card(None);
        self.promote();
        self.refresh();
    }

    fn decide(self: &Rc<Self>, allow: bool) {
        let Some(card) = self.current.borrow().clone() else {
            return;
        };
        // A card for an event that takes no decision should not exist; if one
        // somehow does, taking it down beats leaving a button that lies.
        if let Some(decision) = card.decision(allow) {
            self.reply(&card, decision);
        }
        self.settle_after_the_click(card);
    }

    fn answer(self: &Rc<Self>, option: i32) {
        let Some(card) = self.current.borrow().clone() else {
            return;
        };
        if option < 0 {
            return;
        }
        if let Some(decision) = card.answer(option as usize) {
            self.reply(&card, decision);
        }
        self.settle_after_the_click(card);
    }

    /// Resolve a card that was answered by clicking one of its buttons, on the
    /// next turn of the event loop rather than inside the click itself.
    ///
    /// Answering destroys the card's window — each card has one of its own; see
    /// [`cardview::CardView::ui`] — and the click being handled is that window's
    /// own callback, running inside that window's own element tree. Dropping the
    /// last handle to a component from inside it is not something to do on
    /// purpose.
    ///
    /// The agent has already been released by the time this is queued: [`reply`]
    /// runs inline, and only the window's teardown waits.
    ///
    /// [`reply`]: Self::reply
    fn settle_after_the_click(self: &Rc<Self>, card: Card) {
        let app = Rc::downgrade(self);
        slint::Timer::single_shot(Duration::from_millis(0), move || {
            if let Some(app) = app.upgrade() {
                app.settle(&card);
            }
        });
    }

    fn reply(&self, card: &Card, decision: atoll_core::protocol::HookDecision) {
        let key = (card.session_id.clone(), card.key.clone());
        let Some(handle) = self.blocked.borrow_mut().remove(&key) else {
            return;
        };
        if let Err(error) = handle.send(&Envelope::Response {
            response: Response::Decision { decision },
        }) {
            // The hook timed out and left. The agent has already fallen back to
            // asking in its own terminal, so there is nothing to retry.
            errln!("atoll: the hook was no longer listening: {error}");
        }
    }

    /// Mark the approval resolved locally and move on to the next card.
    fn settle(self: &Rc<Self>, card: &Card) {
        if let Some(state) = self.table.borrow_mut().get_mut(&card.session_id) {
            state.resolve(&card.key);
        }
        self.blocked
            .borrow_mut()
            .remove(&(card.session_id.clone(), card.key.clone()));
        self.dismiss();
    }

    // ------------------------------------------------------------- painting

    fn refresh(self: &Rc<Self>) {
        self.refresh_card();
        self.refresh_bar();
        if self.flyout_open.get() {
            self.refresh_flyout();
        }
    }

    /// Put the current card's text in its window, and open or close that window
    /// to match.
    fn refresh_card(self: &Rc<Self>) {
        let current = self.current.borrow().clone();
        let Some(card) = current else {
            if self.card_view.is_shown() {
                self.card_view.hide();
                self.heal_readout();
            }
            return;
        };

        let had_window = self.card_view.is_shown();
        let Some(ui) = self.show_card(Some(card.kind)) else {
            return;
        };
        if !had_window {
            self.heal_readout();
        }
        ui.set_card(card.kind.as_int());
        ui.set_card_source(card.source.as_str().into());
        ui.set_card_title(card.title.clone().into());
        ui.set_card_tool(card.tool.clone().into());
        ui.set_card_detail(card.detail.clone().into());
        ui.set_card_options(ModelRc::new(VecModel::from(
            card.options
                .iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        )));
        ui.set_card_queued(self.queue.borrow().len() as i32);
    }

    /// Open the window for a card, creating and wiring one if there is none, or
    /// take away the window that is there.
    fn show_card(self: &Rc<Self>, kind: Option<CardKind>) -> Option<ui::CardWindow> {
        let app = Rc::clone(self);
        self.card_view
            .set_card(kind, self.card_anchor(), move |window| {
                app.wire_card(window)
            })
    }

    /// Where a card that has never been dragged opens: beside the taskbar
    /// readout. `None` when there is no taskbar to open beside.
    fn card_anchor(&self) -> Option<(Rect, Rect)> {
        let taskbar = self.host.get()?;
        Some((self.bar.screen_rect(taskbar), taskbar.rect))
    }

    /// Put the current numbers in the taskbar readout.
    fn refresh_bar(&self) {
        if !self.bar.is_shown() {
            return;
        }
        let along = self
            .host
            .get()
            .map(|taskbar| taskbar::Along::of(taskbar.rect))
            .unwrap_or(taskbar::Along::Vertical);
        let (lines, good_at, warn_at) = {
            let config = self.config.borrow();
            let table = self.table.borrow();
            let now = now_unix_secs();
            let lines = [
                taskbar::AgentLine {
                    agent: HookSource::Claude,
                    show: config.taskbar.claude,
                    tasks: table.tasks(HookSource::Claude, now),
                },
                taskbar::AgentLine {
                    agent: HookSource::Codex,
                    show: config.taskbar.codex,
                    tasks: table.tasks(HookSource::Codex, now),
                },
            ];
            let (good_at, warn_at) = config.taskbar.thresholds();
            (lines, good_at, warn_at)
        };
        let chips = taskbar::chips(&self.usage.borrow(), &lines, good_at, warn_at);
        self.bar.set_chips(&chips, along);
    }

    /// Which agents have a live session, in first-seen order.
    ///
    /// The detail panel gives an agent a section for having a session even when
    /// it has reported no usage at all, because "this is running and I cannot
    /// see its quota" is worth saying.
    fn live_agents(&self) -> Vec<HookSource> {
        self.table
            .borrow()
            .sessions()
            .map(|state| state.source)
            .collect()
    }

    /// The session rows, oldest session first so the blocks do not shuffle every
    /// time a phase changes.
    fn session_rows(&self, limit: usize, rich: bool) -> Vec<ui::SessionRow> {
        let table = self.table.borrow();
        let titles = self.titles.borrow();
        table
            .sessions()
            .take(limit)
            .map(|state| {
                let project = state
                    .cwd
                    .as_deref()
                    .map(project_name)
                    .filter(|name| !name.is_empty());
                let summary = rich
                    .then(|| titles.get(&state.session_id))
                    .flatten()
                    .map(String::as_str);
                ui::SessionRow {
                    id: state.session_id.clone().into(),
                    title: session_title(project.as_deref(), summary, &state.session_id).into(),
                    detail: describe_phase(state.phase, state.current_tool()).into(),
                    phase: state.phase.as_str().into(),
                    source: state.source.as_str().into(),
                }
            })
            .collect()
    }

    // ---------------------------------------------------------- housekeeping

    fn housekeeping(self: &Rc<Self>) {
        let now = now_unix_secs();
        // Cheap, and the only thing that notices explorer coming back.
        self.watch_the_taskbar();
        let refreshed = {
            let mut usage = self.usage.borrow_mut();
            let before = usage.refreshed_at;
            usage.refreshed(now);
            usage.refreshed_at != before
        };
        let limits_arrived = self.collect_claude_limits(now);

        let before = self.table.borrow().counts(now);
        self.table.borrow_mut().sweep(now);
        let after = self.table.borrow().counts(now);

        if self.current_is_stale() {
            self.dismiss();
        } else if refreshed || limits_arrived || before != after {
            self.refresh();
        }

        // Anything queued while a card was up, or while the queue head's hook
        // was still alive, gets its turn now.
        if self.current.borrow().is_none() && !self.queue.borrow().is_empty() {
            self.promote();
            self.refresh();
        }
    }

    /// Take whatever the usage worker has finished, and start another if the
    /// reading has gone stale.
    fn collect_claude_limits(&self, now: u64) -> bool {
        let mut arrived = false;
        while let Ok(limits) = self.limits_rx.try_recv() {
            self.usage.borrow_mut().claude = limits;
            self.fetching.set(false);
            arrived = true;
        }

        let stale = self
            .usage
            .borrow()
            .claude
            .is_stale(now, atoll_core::usage::CLAUDE_USAGE_TTL_SECS);
        if stale {
            self.spawn_claude_fetch(atoll_core::usage::CLAUDE_USAGE_TTL_SECS);
        }
        arrived
    }

    /// Fetch Claude's limits on a worker, wanting a reading no older than
    /// `min_age_secs`, unless a fetch is already in flight.
    fn spawn_claude_fetch(&self, min_age_secs: u64) {
        if self.fetching.get() {
            return;
        }
        self.fetching.set(true);
        let tx = self.limits_tx.clone();
        // A detached worker: the result is wanted, but nothing waits for it,
        // and a request that never returns costs one thread and no more.
        let spawned = std::thread::Builder::new()
            .name("atoll-usage".to_string())
            .spawn(move || {
                let _ = tx.send(crate::usage_cache::fetch_claude_limits(
                    now_unix_secs(),
                    min_age_secs,
                ));
            });
        if spawned.is_err() {
            self.fetching.set(false);
        }
    }

    fn refresh_tray_icon(&self) {
        let tray = self.tray.borrow();
        let Some(tray) = tray.as_ref() else { return };

        let now = now_unix_secs();
        let (sessions, waiting) = {
            let table = self.table.borrow();
            (table.len(), table.counts(now).waiting)
        };
        tray.refresh(IconState {
            sessions,
            waiting,
            // Quantised so the icon is redrawn a handful of times a second
            // rather than ten: every redraw is a Shell_NotifyIcon round trip.
            // With nothing waiting the pulse is not drawn at all, so pinning it
            // keeps the comparison from asking for a redraw that looks the same.
            pulse: if waiting > 0 {
                quantise(self.pulse())
            } else {
                0.0
            },
        });
        tray.set_tooltip(&tray_tooltip(
            sessions,
            waiting,
            &self.usage.borrow().compact(),
        ));
    }

    /// 0 → 1 → 0 over [`PULSE_PERIOD_MS`].
    fn pulse(&self) -> f32 {
        let phase = (self.started.elapsed().as_millis() % PULSE_PERIOD_MS) as f32
            / (PULSE_PERIOD_MS / 2) as f32;
        if phase <= 1.0 { phase } else { 2.0 - phase }
    }

    // ----------------------------------------------------------------- tray

    fn handle_tray(self: &Rc<Self>) {
        let commands = match self.tray.borrow().as_ref() {
            Some(tray) => tray.poll(),
            None => return,
        };
        for command in commands {
            match command {
                TrayCommand::OpenSettings => self.open_settings(),
                TrayCommand::Quit => {
                    self.close_flyout();
                    slint::quit_event_loop().ok();
                }
                TrayCommand::ToggleFlyout(rect) => self.toggle_flyout(rect, Anchor::Tray),
            }
        }
    }

    fn set_taskbar_enabled(self: &Rc<Self>, enabled: bool) {
        if enabled {
            self.bar.show();
            self.refresh_bar();
            // The window is brand new, so it has to be found and re-adopted
            // before it is anywhere at all.
            self.host.set(None);
            self.watch_the_taskbar();
        } else {
            self.bar.hide();
        }
        {
            let mut config = self.config.borrow_mut();
            config.taskbar.enabled = enabled;
            config.save();
        }
        self.refresh_settings();
    }

    /// What the readout managed to do, in words, for the settings window.
    fn taskbar_status(&self) -> &'static str {
        if !self.bar.is_shown() {
            return "Atoll's usage numbers stay in the tray icon's tooltip and its panel.";
        }
        match (self.host.get().is_some(), self.bar.is_embedded()) {
            (true, true) => {
                "Sitting in the taskbar, above the notification area. Drag it along the bar to \
                 move it; click it for the details."
            }
            (true, false) => {
                "This shell would not take a child window, so the readout floats against the \
                 taskbar instead. It still drags and still clicks."
            }
            (false, _) => "Waiting for a taskbar to attach to.",
        }
    }

    // --------------------------------------------------------------- flyout

    /// The taskbar readout was clicked: the detail panel, opened beside the
    /// readout — which, since the readout is *in* the taskbar, means opening
    /// away from the taskbar's edge.
    fn toggle_flyout_beside_bar(self: &Rc<Self>) {
        let Some(taskbar) = self.host.get() else {
            crate::util::debug_log("clicked, but no taskbar host yet");
            return;
        };
        self.toggle_flyout(self.bar.screen_rect(taskbar), Anchor::Readout);
    }

    fn toggle_flyout(self: &Rc<Self>, anchor: Rect, from: Anchor) {
        if self.flyout_open.get() {
            self.close_flyout();
            return;
        }
        // Everything between here and `show` has to be cheap: this is a click,
        // and the window has to be up before the user has finished releasing the
        // mouse. So the panel is drawn from what is already in memory, and the
        // one slow thing it wants — the session titles, which live at the end of
        // a directory of transcripts — is fetched afterwards and filled in when
        // it arrives.
        // Geometry first, then show. A window that is mapped before it knows
        // where it goes draws its first frame in the wrong place, and the eye
        // catches that as a flicker even when it lasts one frame.
        self.refresh_flyout();
        self.place_flyout(anchor, from);
        match self.flyout.show() {
            Ok(()) => {
                self.flyout.window().request_redraw();
                self.flyout_open.set(true);
                self.start_title_scan();
                // Somebody is about to read the numbers: get fresher ones than
                // the routine cadence keeps, and the open panel repaints when
                // they arrive.
                self.spawn_claude_fetch(crate::usage_cache::CLICK_FRESH_SECS);
            }
            Err(error) => {
                crate::util::debug_log(&format!("flyout show failed: {error}"));
                // A half-shown window would be a blank rectangle nobody can
                // close; put it back to hidden so the next click starts clean.
                let _ = self.flyout.hide();
            }
        }
        self.heal_readout();
    }

    fn close_flyout(&self) {
        if self.flyout_open.get() {
            let _ = self.flyout.hide();
            self.flyout_open.set(false);
            self.heal_readout();
        }
    }

    /// Repaint the readout, now and again in a moment.
    ///
    /// Another window being mapped or unmapped can cost the readout its last
    /// frame — see [`taskbar::TaskbarView::request_redraw`]. Once immediately,
    /// and once after the dust settles, for the frame the first repaint races
    /// against the window that is still coming or going.
    fn heal_readout(&self) {
        self.bar.request_redraw();
        let bar = Rc::clone(&self.bar);
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            bar.request_redraw();
        });
    }

    fn refresh_flyout(&self) {
        let rows = self.session_rows(usize::MAX, true);
        let live = self.live_agents();
        let (good_at, warn_at) = self.config.borrow().taskbar.thresholds();
        let usage = usage_sections(
            &self.usage.borrow(),
            &live,
            now_unix_secs(),
            win::local_offset_secs(),
            good_at,
            warn_at,
        );
        self.flyout.set_sessions(ModelRc::new(VecModel::from(rows)));
        self.flyout
            .set_usage_rows(ModelRc::new(VecModel::from(usage)));
    }

    fn place_flyout(&self, anchor: Rect, from: Anchor) {
        let rows = self.flyout.get_sessions().row_count();
        let usage: Vec<ui::UsageRow> = self.flyout.get_usage_rows().iter().collect();
        let height = flyout_height(rows, usage_block_height(&usage));
        let scale = {
            let scale = self.flyout.window().scale_factor();
            if scale > 0.0 { scale } else { 1.0 }
        };
        self.flyout
            .window()
            .set_size(slint::LogicalSize::new(FLYOUT_WIDTH, height));

        let area = win::work_area_at(anchor.left, anchor.top);
        let size = (
            (FLYOUT_WIDTH * scale).round() as i32,
            (height * scale).round() as i32,
        );
        let (x, y) = match from {
            Anchor::Tray => place_flyout(anchor, size, area),
            Anchor::Readout => place_flyout_beside(anchor, size, area),
        };
        self.flyout
            .window()
            .set_position(slint::PhysicalPosition::new(x, y));
    }

    /// Read the session titles out of the transcripts, on a thread of its own.
    ///
    /// A transcript is a whole-file read of something an agent writes to all
    /// day, and there can be forty of them. Doing that on the UI thread is what
    /// made opening the panel feel like it had stuck: the click landed, and the
    /// window appeared when the last file had been read. Now the panel opens
    /// with the titles from last time and the new ones arrive through
    /// [`Self::collect_titles`], which is a repaint rather than a wait.
    ///
    /// The scan itself only opens what has changed since the last one; see
    /// [`transcript::scan_claude_cached`].
    fn start_title_scan(self: &Rc<Self>) {
        if self.scanning.get() {
            return;
        }
        let Some(home) = crate::util::home_dir() else {
            return;
        };
        self.scanning.set(true);

        let tx = self.titles_tx.clone();
        let cache = Arc::clone(&self.transcripts);
        let spawned = std::thread::Builder::new()
            .name("atoll-titles".to_string())
            .spawn(move || {
                let options = transcript::ScanOptions::new(home);
                let found = {
                    // Poisoned only if a previous scan panicked mid-read, in
                    // which case the worst the cache holds is a stale entry.
                    let mut cache = cache.lock().unwrap_or_else(|held| held.into_inner());
                    transcript::scan_claude_cached(&options, &mut cache).unwrap_or_default()
                };
                // Stored raw: the cleaning belongs with the rendering, so that
                // changing how a label is built does not mean rescanning every
                // transcript.
                let titles: HashMap<String, String> = found
                    .into_iter()
                    .filter_map(|summary| {
                        let title = summary.title.filter(|title| !title.trim().is_empty())?;
                        Some((summary.session_id, title))
                    })
                    .collect();
                if tx.send(titles).is_ok() {
                    let _ = slint::invoke_from_event_loop(pump);
                }
            });
        if spawned.is_err() {
            self.scanning.set(false);
        }
    }

    // ------------------------------------------------------------- settings

    fn open_settings(self: &Rc<Self>) {
        self.close_flyout();
        if let Some(window) = self.settings_window.borrow().as_ref() {
            let _ = window.show();
            self.heal_readout();
            self.refresh_settings();
            return;
        }

        let Ok(window) = ui::SettingsWindow::new() else {
            errln!("atoll: could not open the settings window");
            return;
        };
        window.set_taskbar_enabled(self.bar.is_shown());

        let app = Rc::downgrade(self);
        window.on_install(move || {
            if let Some(app) = app.upgrade() {
                app.run_install(true);
            }
        });
        let app = Rc::downgrade(self);
        window.on_uninstall(move || {
            if let Some(app) = app.upgrade() {
                app.run_install(false);
            }
        });
        let app = Rc::downgrade(self);
        window.on_set_taskbar_enabled(move |enabled| {
            if let Some(app) = app.upgrade() {
                app.set_taskbar_enabled(enabled);
            }
        });
        let app = Rc::downgrade(self);
        window.on_set_run_at_login(move |enabled| {
            if let Some(app) = app.upgrade() {
                app.apply_run_at_login(enabled);
            }
        });
        let app = Rc::downgrade(self);
        window.on_set_agent_shown(move |agent, shown| {
            if let Some(app) = app.upgrade() {
                app.set_agent_shown(&agent, shown);
            }
        });
        let app = Rc::downgrade(self);
        window.on_set_thresholds(move |good, warn| {
            if let Some(app) = app.upgrade() {
                app.set_thresholds(good as i64, warn as i64);
            }
        });

        // Closing the window with its own titlebar button unmaps it behind the
        // app's back, which is one of the moments the readout needs a repaint.
        let app = Rc::downgrade(self);
        window.window().on_close_requested(move || {
            if let Some(app) = app.upgrade() {
                app.heal_readout();
            }
            slint::CloseRequestResponse::HideWindow
        });

        let _ = window.show();
        *self.settings_window.borrow_mut() = Some(window);
        self.heal_readout();
        self.refresh_settings();
    }

    /// Put one line under the settings window's buttons.
    fn note_settings(&self, message: &str) {
        if let Some(window) = self.settings_window.borrow().as_ref() {
            window.set_message(message.into());
        }
    }

    fn refresh_settings(&self) {
        let open = self.settings_window.borrow();
        let Some(window) = open.as_ref() else { return };
        window.set_taskbar_enabled(self.bar.is_shown());
        window.set_taskbar_status(self.taskbar_status().into());
        window.set_run_at_login(win::runs_at_login());
        {
            let config = self.config.borrow();
            window.set_show_claude(config.taskbar.claude);
            window.set_show_codex(config.taskbar.codex);
            window.set_good_at(config.taskbar.good_at as i32);
            window.set_warn_at(config.taskbar.warn_at as i32);
        }

        // A machine with only one of the agents is a perfectly normal machine;
        // the window says which it found rather than offering an install that
        // has nothing to install into.
        let claude_present = agent_present(".claude");
        window.set_claude_present(claude_present);
        if !claude_present {
            window.set_claude_installed(false);
            window.set_claude_status("not found on this machine".into());
            return;
        }

        match atoll_core::install::claude_settings_path()
            .and_then(|path| settings::read_status(&path))
        {
            Ok(status) => {
                window.set_claude_installed(status.is_installed());
                window.set_claude_status(settings::describe(&status).into());
            }
            Err(error) => {
                window.set_claude_installed(false);
                window.set_claude_status(settings::describe_error(&error).into());
            }
        }
    }

    /// One agent's block in the readout, on or off — saved, and applied at once.
    fn set_agent_shown(&self, agent: &str, shown: bool) {
        {
            let mut config = self.config.borrow_mut();
            match agent {
                "claude" => config.taskbar.claude = shown,
                "codex" => config.taskbar.codex = shown,
                _ => return,
            }
            config.save();
        }
        self.refresh_bar();
    }

    /// The colour thresholds — saved, and applied everywhere a tier shows.
    fn set_thresholds(self: &Rc<Self>, good_at: i64, warn_at: i64) {
        {
            let mut config = self.config.borrow_mut();
            config.taskbar.good_at = good_at;
            config.taskbar.warn_at = warn_at;
            config.save();
        }
        self.refresh();
    }

    /// Wire or unwire the login launch, pointing the registry at the installed
    /// copy — or at the running one when nothing has been installed yet.
    ///
    /// Either direction also sweeps away the Startup-folder shortcut an
    /// earlier setup left behind, so two mechanisms can never fight.
    fn apply_run_at_login(&self, enabled: bool) {
        remove_legacy_startup_shortcut();
        let exe = match atoll_core::install::stable_bin_dir() {
            Ok(dir) if dir.join("atoll.exe").exists() => Ok(dir.join("atoll.exe")),
            _ => std::env::current_exe().map_err(|error| error.to_string()),
        };
        let outcome = exe.and_then(|exe| win::set_run_at_login(enabled, &exe));
        match outcome {
            Ok(()) => self.note_settings(if enabled {
                "Atoll starts at login, running the installed copy."
            } else {
                "Atoll no longer starts at login."
            }),
            Err(error) => {
                self.note_settings(&format!("Could not update the login launch: {error}"));
            }
        }
    }

    /// An earlier Atoll wired run-at-login as a hand-made Startup shortcut.
    /// Move that to the registry Run key, once, so the settings checkbox and
    /// the mechanism agree from the first time the window opens.
    fn migrate_startup_shortcut(&self) {
        let Some(link) = legacy_startup_shortcut() else {
            return;
        };
        if !link.exists() {
            return;
        }
        let exe = match atoll_core::install::stable_bin_dir() {
            Ok(dir) if dir.join("atoll.exe").exists() => dir.join("atoll.exe"),
            _ => match std::env::current_exe() {
                Ok(exe) => exe,
                Err(_) => return,
            },
        };
        if win::set_run_at_login(true, &exe).is_ok() {
            let _ = std::fs::remove_file(link);
            crate::util::debug_log("moved the Startup shortcut to the Run key");
        }
    }

    fn run_install(&self, install: bool) {
        let outcome = atoll_core::install::claude_settings_path().and_then(|path| {
            if install {
                settings::install(&path)
            } else {
                settings::uninstall(&path)
            }
        });
        match &outcome {
            Ok(message) => self.note_settings(message),
            Err(error) => self.note_settings(&format!("Failed: {error}")),
        }
        self.refresh_settings();
    }
}

/// Which thing the session list was opened from, and so where it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    Tray,
    Readout,
}

/// How tall one detail-panel row is, mirrored in `ui/flyout.slint`.
const HEADING_ROW: f32 = 26.0;
const WINDOW_ROW: f32 = 17.0;

/// The detail panel's usage block: an agent per section, its tightest number
/// large in the heading, and one bar per window under it.
///
/// The bar is the point. The panel used to be a column of sentences in one
/// shade and one weight, and finding the window that was about to run out meant
/// reading every line; a short bar is short from across the room.
fn usage_sections(
    usage: &UsageSnapshot,
    live: &[HookSource],
    now: u64,
    offset_secs: i64,
    good_at: i64,
    warn_at: i64,
) -> Vec<ui::UsageRow> {
    let mut rows = Vec::new();
    for agent in AGENTS {
        let windows = usage.windows(agent);
        if windows.is_empty() && !live.contains(&agent) {
            continue;
        }

        // The heading carries the number the panel was opened for: the window
        // that will stop this agent first.
        let tightest = usage.tightest_window(agent);
        rows.push(ui::UsageRow {
            heading: true,
            agent: agent.as_str().into(),
            label: agent.as_str().into(),
            value: tightest
                .as_ref()
                .map(|window| format!("{}%", window.left))
                .unwrap_or_default()
                .into(),
            tier: tightest
                .as_ref()
                .map(|window| crate::usage_cache::left_tier(window.left, good_at, warn_at))
                .unwrap_or_default()
                .into(),
            fill: 0.0,
            resets: Default::default(),
        });

        if windows.is_empty() {
            rows.push(ui::UsageRow {
                heading: false,
                agent: Default::default(),
                label: "no data".into(),
                value: Default::default(),
                tier: Default::default(),
                fill: 0.0,
                resets: Default::default(),
            });
            continue;
        }

        for window in windows {
            rows.push(ui::UsageRow {
                heading: false,
                agent: Default::default(),
                label: window.label.clone().into(),
                value: format!("{}%", window.left).into(),
                tier: crate::usage_cache::left_tier(window.left, good_at, warn_at).into(),
                fill: window.left as f32 / 100.0,
                resets: crate::usage_cache::reset_label(window.resets_at, now, offset_secs)
                    .map(|when| format!("resets {when}"))
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    rows
}

/// The detail panel's usage block, in logical pixels.
fn usage_block_height(rows: &[ui::UsageRow]) -> f32 {
    if rows.is_empty() {
        return 15.0;
    }
    rows.iter()
        .map(|row| if row.heading { HEADING_ROW } else { WINDOW_ROW })
        .sum()
}

/// The longest a session's label may run before it is elided.
const TITLE_LIMIT: usize = 40;

/// What to call one session in the detail panel.
///
/// The project folder first, always: it is the thing the user recognises, it is
/// short, and it never changes mid-session. Then, if a transcript has been read,
/// what the agent last said — stripped of the Markdown it was written in, which
/// a one-line label renders as noise rather than emphasis.
///
/// With neither, the first few characters of the session id: unhelpful, but a
/// row with no name at all is worse.
fn session_title(project: Option<&str>, summary: Option<&str>, session_id: &str) -> String {
    let summary = summary.map(clean_title).filter(|text| !text.is_empty());
    let label = match (project, summary) {
        (Some(project), Some(summary)) => format!("{project} · {summary}"),
        (Some(project), None) => project.to_string(),
        (None, Some(summary)) => summary,
        (None, None) => session_id.chars().take(8).collect(),
    };
    truncate(&label, TITLE_LIMIT)
}

/// The phase, in words, for the tray panel.
fn describe_phase(phase: Phase, tool: Option<&str>) -> String {
    match phase {
        Phase::Running => "Working".to_string(),
        Phase::WaitingForApproval => match tool {
            Some(tool) => format!("Waiting for approval · {tool}"),
            None => "Waiting for approval".to_string(),
        },
        Phase::WaitingForAnswer => "Waiting for an answer".to_string(),
        Phase::Completed => "Done".to_string(),
    }
}

fn tray_tooltip(sessions: usize, waiting: usize, usage: &str) -> String {
    let mut text = match sessions {
        0 => "Atoll · no sessions".to_string(),
        1 => "Atoll · 1 session".to_string(),
        many => format!("Atoll · {many} sessions"),
    };
    if waiting > 0 {
        text.push_str(&format!(", {waiting} waiting"));
    }
    if !usage.is_empty() {
        text.push_str(&format!("\n{usage}"));
    }
    text
}

/// Round to eighths, so a smoothly advancing pulse only produces a few distinct
/// icons per second.
fn quantise(value: f32) -> f32 {
    (value.clamp(0.0, 1.0) * 8.0).round() / 8.0
}

/// The detail panel's height for this many session rows and this much usage
/// block. Mirrors the paddings and spacings in `ui/flyout.slint`.
fn flyout_height(rows: usize, usage_height: f32) -> f32 {
    let rows_height = if rows == 0 {
        0.0
    } else {
        rows as f32 * 32.0 + (rows - 1) as f32 * 9.0
    };
    // padding + header + spacing + rows + spacing + rule + spacing + usage
    28.0 + 14.0 + 10.0 + rows_height + 10.0 + 1.0 + 10.0 + usage_height
}

/// Put the panel beside the tray icon, on the same side of the screen the
/// taskbar is.
fn place_flyout(anchor: Rect, size: (i32, i32), area: Rect) -> (i32, i32) {
    let (width, height) = size;
    let x = (anchor.right - width).clamp(
        area.left + FLYOUT_MARGIN,
        (area.right - width - FLYOUT_MARGIN).max(area.left + FLYOUT_MARGIN),
    );
    // A tray icon in the top half of the screen means the taskbar is up there,
    // so the panel hangs below it instead of above.
    let top_taskbar = anchor.top < (area.top + area.bottom) / 2;
    let y = if top_taskbar {
        area.top + FLYOUT_MARGIN
    } else {
        (area.bottom - height - FLYOUT_MARGIN).max(area.top + FLYOUT_MARGIN)
    };
    (x, y)
}

/// Put the panel beside the taskbar readout, opening toward the middle of the
/// screen the way a card does.
///
/// The readout is always in the taskbar and the taskbar is always docked to an
/// edge, so "toward the middle" is always where the room is: a bar on the right
/// opens its panel to the left, and the panel's top lines up with the readout's
/// rather than jumping to a corner.
fn place_flyout_beside(anchor: Rect, size: (i32, i32), area: Rect) -> (i32, i32) {
    let (width, height) = size;
    let on_right = (anchor.left + anchor.right) / 2 > (area.left + area.right) / 2;
    let x = if on_right {
        anchor.left - FLYOUT_MARGIN - width
    } else {
        anchor.right + FLYOUT_MARGIN
    };
    (
        clamp_between(x, width, area.left, area.right),
        clamp_between(anchor.top, height, area.top, area.bottom),
    )
}

/// `start`, moved as little as it takes to fit a `length`-long span inside
/// `low..high` with a [`FLYOUT_MARGIN`] to spare at each end.
fn clamp_between(start: i32, length: i32, low: i32, high: i32) -> i32 {
    let first = low + FLYOUT_MARGIN;
    start.clamp(first, (high - length - FLYOUT_MARGIN).max(first))
}

/// Whether an agent's own directory exists under the home directory — the
/// cheapest honest test for "is this agent on this machine at all".
fn agent_present(dir: &str) -> bool {
    crate::util::home_dir()
        .map(|home| home.join(dir).exists())
        .unwrap_or(false)
}

/// Where the hand-made Startup shortcut lived, while it existed.
fn legacy_startup_shortcut() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup\Atoll.lnk"),
    )
}

/// Remove the legacy shortcut, so the registry Run key is the one mechanism.
fn remove_legacy_startup_shortcut() {
    if let Some(link) = legacy_startup_shortcut() {
        let _ = std::fs::remove_file(link);
    }
}

/// The last thing the agent said, from its transcript.
///
/// Read on `Stop` only. It is a whole-file scan, and `Stop` is both the one time
/// there is a finished message worth quoting and a moment when nothing else is
/// competing for this thread.
fn transcript_summary(path: Option<&str>) -> Option<String> {
    let path = Path::new(path?);
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    transcript::read_transcript(path, modified)
        .ok()
        .flatten()?
        .title
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };

    const CODEX: HookSource = HookSource::Codex;

    const NOW: u64 = 1_787_000_000;

    fn limit(label: &str, percent: f64, resets_at: Option<u64>) -> atoll_core::usage::UsageLimit {
        atoll_core::usage::UsageLimit {
            kind: label.to_lowercase(),
            label: label.to_string(),
            percent,
            resets_at,
        }
    }

    /// Claude with three windows and Codex with two — an ordinary day for
    /// someone running both.
    fn both_agents() -> UsageSnapshot {
        UsageSnapshot {
            claude: atoll_core::usage::ClaudeLimits {
                limits: vec![
                    limit("Session", 8.0, Some(NOW + 7_200)),
                    limit("Week", 31.0, Some(NOW + 5 * 86_400)),
                    limit("Fable", 27.0, Some(NOW + 5 * 86_400)),
                ],
                fetched_at: Some(NOW),
            },
            codex: Some(atoll_core::usage::CodexUsage {
                primary: Some(atoll_core::usage::WindowUsage {
                    used_percent: 7.4,
                    resets_at: None,
                    window_minutes: Some(299),
                }),
                secondary: Some(atoll_core::usage::WindowUsage {
                    used_percent: 85.0,
                    resets_at: None,
                    window_minutes: Some(10_080),
                }),
                plan_type: Some("prolite".into()),
                source: None,
            }),
            ..UsageSnapshot::default()
        }
    }

    /// The usage block is sized from what it actually draws: headings are
    /// taller than the window rows under them, so a count is not enough.
    #[test]
    fn the_usage_block_is_measured_row_by_row() {
        let rows = usage_sections(&both_agents(), &[], NOW, 0, 50, 20);
        // claude + 3 windows, codex + 2 windows.
        assert_eq!(rows.len(), 7);
        assert_eq!(rows.iter().filter(|row| row.heading).count(), 2);
        assert_eq!(
            usage_block_height(&rows),
            2.0 * HEADING_ROW + 5.0 * WINDOW_ROW
        );
        // An empty block still leaves room for the line that says so.
        assert_eq!(usage_block_height(&[]), 15.0);
    }

    /// The heading carries the number the panel was opened for, and each row
    /// carries a bar whose length is the number.
    #[test]
    fn every_section_leads_with_its_tightest_window() {
        let rows = usage_sections(&both_agents(), &[], NOW, 0, 50, 20);

        assert_eq!(rows[0].label, "claude");
        assert_eq!(rows[0].value, "69%", "the week, not the session");
        assert_eq!(rows[0].tier, "good");

        assert_eq!(rows[1].label, "Session");
        assert_eq!(rows[1].value, "92%");
        assert!((rows[1].fill - 0.92).abs() < 0.001);
        assert!(rows[1].resets.starts_with("resets "));

        assert_eq!(rows[4].label, "codex");
        assert_eq!(
            (rows[4].value.as_str(), rows[4].tier.as_str()),
            ("15%", "low")
        );
        // Codex reported no reset times, so those rows carry none rather than
        // an empty "resets".
        assert_eq!(rows[6].resets, "");
        // And nothing says what plan anybody is on any more.
        assert!(
            !rows.iter().any(|row| row.label.contains("plan")),
            "the plan line was noise in a panel about limits"
        );
    }

    #[test]
    fn a_section_appears_for_a_running_agent_with_nothing_to_report() {
        let rows = usage_sections(&UsageSnapshot::default(), &[CODEX], NOW, 0, 50, 20);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].heading && rows[0].label == "codex");
        assert_eq!(rows[0].value, "");
        assert_eq!(rows[1].label, "no data");

        // And an idle machine with no readings gets no sections at all; the
        // panel says "No usage data yet" itself.
        assert!(usage_sections(&UsageSnapshot::default(), &[], NOW, 0, 50, 20).is_empty());
    }

    /// The label the panel shows for a session: the project it is in, then what
    /// the agent last said, with the Markdown taken out of it.
    #[test]
    fn a_session_is_labelled_by_its_project_and_its_last_word() {
        assert_eq!(
            session_title(
                Some("atoll"),
                Some("**Done.** Wired the parser."),
                "abc12345"
            ),
            "atoll · Done. Wired the parser."
        );
        // No transcript read yet: the project alone.
        assert_eq!(session_title(Some("atoll"), None, "abc12345"), "atoll");
        // No cwd either: something rather than a blank row.
        assert_eq!(session_title(None, None, "abc12345-0000"), "abc12345");
        // A summary that was nothing but markup does not leave a dangling dot.
        assert_eq!(session_title(Some("atoll"), Some("***"), "abc"), "atoll");

        // And the whole thing is cut to something a narrow panel can show.
        let long = session_title(
            Some("open-vibe-island-win"),
            Some("安排好了： 1. **悬浮球 v3 暂停**——已做的进度都在 commit 里"),
            "abc",
        );
        assert!(long.chars().count() <= TITLE_LIMIT, "got {long:?}");
        assert!(long.ends_with('…'), "got {long:?}");
        assert!(!long.contains('*'));
    }

    #[test]
    fn the_panel_opens_beside_its_icon() {
        // The usual case: taskbar along the bottom, icon at the right.
        let anchor = Rect {
            left: 1700,
            top: 1044,
            right: 1724,
            bottom: 1068,
        };
        let (x, y) = place_flyout(anchor, (320, 200), SCREEN);
        assert_eq!(x, 1724 - 320);
        assert_eq!(y, 1040 - 200 - FLYOUT_MARGIN);

        // Taskbar at the top: the panel hangs down instead.
        let top = Rect {
            left: 1700,
            top: 4,
            right: 1724,
            bottom: 28,
        };
        let (_, y) = place_flyout(top, (320, 200), SCREEN);
        assert_eq!(y, SCREEN.top + FLYOUT_MARGIN);
    }

    #[test]
    fn a_panel_beside_a_low_readout_is_pulled_back_onto_the_screen() {
        let low = Rect {
            left: 1848,
            top: 1000,
            right: 1912,
            bottom: 1064,
        };
        let (x, y) = place_flyout_beside(low, (320, 200), SCREEN);
        assert!(x >= SCREEN.left + FLYOUT_MARGIN);
        assert_eq!(y, 1040 - 200 - FLYOUT_MARGIN);

        // A panel taller than the whole screen still starts on it.
        let (_, y) = place_flyout_beside(low, (320, 4_000), SCREEN);
        assert_eq!(y, SCREEN.top + FLYOUT_MARGIN);
    }

    #[test]
    fn the_panel_never_hangs_off_the_left_edge() {
        let anchor = Rect {
            left: 10,
            top: 1044,
            right: 34,
            bottom: 1068,
        };
        let (x, _) = place_flyout(anchor, (320, 200), SCREEN);
        assert_eq!(x, SCREEN.left + FLYOUT_MARGIN);
    }
}
