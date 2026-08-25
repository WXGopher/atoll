//! The handful of Win32 calls Slint does not expose.
//!
//! Everything here takes and returns plain integers so the rest of the app never
//! has to name a `HWND`.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ScreenToClient,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR};

/// A screen rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Where the pointer is, in physical screen pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

/// Whether the left mouse button is down right now, asked of the hardware
/// rather than of a message queue.
pub fn left_button_down() -> bool {
    // The high bit is "down now"; the low bit is "was pressed since last asked"
    // and is deliberately ignored — this is a level, not an edge.
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

/// As [`left_button_down`], for the right button.
pub fn right_button_down() -> bool {
    unsafe { GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

/// A native context menu at the cursor, blocking until the user picks or
/// dismisses. Returns the index into `labels` of the pick; a `"-"` label is a
/// separator, which occupies an index but can never be returned.
///
/// Native rather than a Slint window because that is what a context menu on a
/// taskbar control is: it dismisses when the user clicks anywhere else, it
/// clips nowhere, and it matches the tray icon's own menu exactly.
pub fn popup_menu(owner: isize, labels: &[&str]) -> Option<usize> {
    let window = hwnd(owner);
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (index, label) in labels.iter().enumerate() {
            let appended = if *label == "-" {
                AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())
            } else {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                AppendMenuW(menu, MF_STRING, index + 1, PCWSTR(wide.as_ptr()))
            };
            if appended.is_err() {
                let _ = DestroyMenu(menu);
                return None;
            }
        }

        // Without foreground status the menu refuses to dismiss on an outside
        // click; the WM_NULL afterwards is the other half of the same folklore,
        // and both are what the shell itself does for tray menus.
        let _ = SetForegroundWindow(window);
        let (x, y) = cursor_position().unwrap_or((0, 0));
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            x,
            y,
            None,
            window,
            None,
        );
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(window), WM_NULL, Default::default(), Default::default());

        let id = picked.0 as usize;
        if id == 0 { None } else { Some(id - 1) }
    }
}

/// The window under a screen point, or `None` if there is none.
pub fn window_at(x: i32, y: i32) -> Option<isize> {
    let window = unsafe { WindowFromPoint(POINT { x, y }) };
    (!window.is_invalid()).then_some(window.0 as isize)
}

/// A fallback for the machines where the monitor query fails: better a plausible
/// rectangle than a window positioned at the origin of nowhere.
const FALLBACK_WORK_AREA: Rect = Rect {
    left: 0,
    top: 0,
    right: 1920,
    bottom: 1040,
};

struct FindByTitle {
    process: u32,
    wanted: Vec<u16>,
    found: HWND,
}

unsafe extern "system" fn match_title(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut FindByTitle) };
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    if process != context.process {
        return BOOL(1);
    }

    // winit stands up several invisible helper windows that share our process
    // id, so matching on the id alone finds the wrong one and the taskbar tweak
    // lands on a window nobody can see. The title is the only reliable
    // discriminator.
    let mut buffer = [0u16; 256];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if length <= 0 {
        return BOOL(1);
    }
    if buffer[..length as usize] == context.wanted[..] {
        context.found = hwnd;
        return BOOL(0);
    }
    BOOL(1)
}

/// This process' window with exactly this title, if it has been created yet.
///
/// Slint creates the real window only once the event loop is running, so callers
/// have to retry rather than asking once right after `show()`.
pub fn window_by_title(title: &str) -> Option<isize> {
    let mut context = FindByTitle {
        process: std::process::id(),
        wanted: title.encode_utf16().collect(),
        found: HWND(std::ptr::null_mut()),
    };
    unsafe {
        let _ = EnumWindows(Some(match_title), LPARAM(&mut context as *mut _ as isize));
    }
    (!context.found.0.is_null()).then_some(context.found.0 as isize)
}

fn hwnd(handle: isize) -> HWND {
    HWND(handle as *mut std::ffi::c_void)
}

/// Take the window out of the taskbar and the Alt-Tab list, and pin it on top.
///
/// `WS_EX_TOOLWINDOW` is what actually hides it; dropping `WS_EX_APPWINDOW` stops
/// the shell putting it back. Windows only re-reads those bits when the window is
/// hidden and shown again, hence the cycle — `SW_SHOWNOACTIVATE` so a card
/// never steals focus from the editor the user is typing in.
pub fn hide_from_taskbar(handle: isize) {
    let window = hwnd(handle);
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        let _ = ShowWindow(window, SW_HIDE);
        SetWindowLongPtrW(
            window,
            GWL_EXSTYLE,
            (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize),
        );
        let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
    }
    keep_on_top(handle);
}

/// Set the tool-window bits on a window that is not currently visible.
///
/// Windows reads the bits when the window is next shown, so a window that has
/// never been on screen gets them for free — no taskbar button appears, not
/// even for a frame, and no hide-and-show cycle is needed. This is the call
/// for a window created ahead of time; [`hide_from_taskbar`] is the one for a
/// window the user is already looking at.
pub fn mark_tool_window(handle: isize) {
    let window = hwnd(handle);
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(
            window,
            GWL_EXSTYLE,
            (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize),
        );
    }
}

/// Re-assert topmost without moving, resizing, or focusing the window.
///
/// Slint's `always-on-top` sets the style once; a full-screen app or a UAC prompt
/// can still push a window down the z-order, so a card re-states its claim every
/// time it opens.
pub fn keep_on_top(handle: isize) {
    unsafe {
        let _ = SetWindowPos(
            hwnd(handle),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

// ------------------------------------------------------------- the taskbar
//
// Atoll's usage readout lives *inside* the taskbar rather than beside it: it is
// reparented into `Shell_TrayWnd`, which is how TrafficMonitor and its like put
// a readout in the empty run of the task list. Being a child of the taskbar is
// what makes it follow the taskbar's z-order, its auto-hide, and its monitor —
// none of which a topmost strip pretending to be attached can do.
//
// The taskbar is another process's window, so all of this is best-effort by
// construction: the shell can restart, a third-party shell replacement can lay
// its windows out differently, and a future Windows can drop the class name
// altogether. Every function here answers `None` rather than failing, and the
// caller falls back to a floating strip.

/// The classic taskbar's window class. Windows 11's own taskbar keeps it, and
/// so do the shell replacements that restore the Windows 10 layout.
const TASKBAR_CLASS: &str = "Shell_TrayWnd";
/// The notification area inside it — the clock, the tray icons. Atoll's readout
/// parks clear of this, in the empty stretch the task list leaves.
const NOTIFY_CLASS: &str = "TrayNotifyWnd";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_window(class: &str) -> Option<HWND> {
    let class = wide(class);
    let window = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()?;
    (!window.is_invalid()).then_some(window)
}

fn find_child(parent: HWND, class: &str) -> Option<HWND> {
    let class = wide(class);
    let window =
        unsafe { FindWindowExW(Some(parent), None, PCWSTR(class.as_ptr()), PCWSTR::null()) }
            .ok()?;
    (!window.is_invalid()).then_some(window)
}

fn rect_of(window: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(window, &mut rect) }.ok()?;
    Some(Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    })
}

/// The taskbar, if this shell has one Atoll recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taskbar {
    pub handle: isize,
    /// The whole bar, in physical screen pixels.
    pub rect: Rect,
    /// The notification area within it, which is what the readout parks clear
    /// of. Falls back to a zero-width sliver at the bar's far end when the
    /// shell has no window by that name.
    pub notify: Rect,
}

/// Find the taskbar and the notification area inside it.
pub fn taskbar() -> Option<Taskbar> {
    let handle = find_window(TASKBAR_CLASS)?;
    let rect = rect_of(handle)?;
    // A taskbar with no width or height is one that is auto-hidden or mid-
    // restart; placing anything against it would be placing it at nowhere.
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return None;
    }
    let notify = find_child(handle, NOTIFY_CLASS)
        .and_then(rect_of)
        .unwrap_or(Rect {
            left: rect.right,
            top: rect.bottom,
            right: rect.right,
            bottom: rect.bottom,
        });
    Some(Taskbar {
        handle: handle.0 as isize,
        rect,
        notify,
    })
}

/// Make `child` a child window of `parent`, and report whether it took.
///
/// Two calls, not one: `SetParent` alone leaves a popup that is parented but
/// still styled as a top-level window, which the shell paints over the moment
/// it redraws. `WS_CHILD` is what makes it part of the taskbar's own drawing.
pub fn embed_in(child: isize, parent: isize) -> bool {
    let child = hwnd(child);
    let parent = hwnd(parent);
    unsafe {
        let style = GetWindowLongPtrW(child, GWL_STYLE);
        SetWindowLongPtrW(
            child,
            GWL_STYLE,
            (style | WS_CHILD.0 as isize) & !(WS_POPUP.0 as isize),
        );
        if SetParent(child, Some(parent)).is_err() {
            // Put the style back rather than leaving a top-level window
            // claiming to be somebody's child.
            SetWindowLongPtrW(child, GWL_STYLE, style);
            return false;
        }
        // The style change only takes effect on the next frame change.
        let _ = SetWindowPos(
            child,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
    parent_of(child.0 as isize) == Some(parent.0 as isize)
}

/// This window's parent, or `None` for a top-level one.
pub fn parent_of(handle: isize) -> Option<isize> {
    let parent = unsafe { GetParent(hwnd(handle)) }.ok()?;
    (!parent.is_invalid()).then_some(parent.0 as isize)
}

/// Move a window without resizing, activating, or re-ordering it. Coordinates
/// are the parent's client space for an embedded window and the screen's for a
/// top-level one, which is exactly `SetWindowPos`'s own rule.
pub fn move_window(handle: isize, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd(handle),
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Turn a screen point into `window`'s client coordinates.
pub fn to_client(window: isize, x: i32, y: i32) -> (i32, i32) {
    let mut point = POINT { x, y };
    if unsafe { ScreenToClient(hwnd(window), &mut point) }.as_bool() {
        (point.x, point.y)
    } else {
        (x, y)
    }
}

fn work_area_of(monitor: HMONITOR) -> Option<Rect> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then(|| {
            let RECT {
                left,
                top,
                right,
                bottom,
            } = info.rcWork;
            Rect {
                left,
                top,
                right,
                bottom,
            }
        })
}

/// The usable area — screen minus taskbar — of the monitor nearest this point.
pub fn work_area_at(x: i32, y: i32) -> Rect {
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    work_area_of(monitor).unwrap_or(FALLBACK_WORK_AREA)
}

/// The side the shell draws small icons at, which is the size the tray wants.
/// 16 at 100 % scaling, 20 at 125 %, 24 at 150 %.
pub fn small_icon_size() -> u32 {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    if size <= 0 { 16 } else { size as u32 }
}

/// Seconds to add to a UTC Unix timestamp to get local wall-clock time.
///
/// Windows states the offset the other way round — `Bias` is what you *add to
/// local* to get UTC — hence the negation. The daylight bias is folded in
/// according to which season the machine says it is in, which is right for every
/// reset Atoll shows: they are all within a week, and a transition inside that
/// window costs an hour on one line for one day a year.
pub fn local_offset_secs() -> i64 {
    use windows::Win32::System::Time::{
        GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
    };

    /// `GetTimeZoneInformation`'s return value for "daylight saving is in
    /// force". The `windows` crate exposes only the invalid sentinel by name.
    const DAYLIGHT: u32 = 2;

    let mut zone = TIME_ZONE_INFORMATION::default();
    let result = unsafe { GetTimeZoneInformation(&mut zone) };
    // The clock is not something to fail over: an unknown zone reads as UTC.
    if result == TIME_ZONE_ID_INVALID {
        return 0;
    }
    let seasonal = if result == DAYLIGHT {
        zone.DaylightBias
    } else {
        zone.StandardBias
    };
    -((zone.Bias + seasonal) as i64) * 60
}

/// Let go of the console this process was started from.
///
/// `atoll` is a console binary so that `atoll setup` and `atoll headless` can
/// print; the app has nothing to say on a terminal, and a console window sitting
/// behind for the rest of the session would be pure noise. Detaching
/// closes the window we allocated ourselves and leaves an inherited one — the
/// shell the user launched us from — running.
pub fn detach_console() {
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }
}
