//! The handful of Win32 calls Slint does not expose.
//!
//! Everything here takes and returns plain integers so the rest of the app never
//! has to name a `HWND`.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ScreenToClient,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
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
        let _ = PostMessageW(
            Some(window),
            WM_NULL,
            Default::default(),
            Default::default(),
        );

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

// --------------------------------------------------------- run at login

/// `HKCU`'s per-user Run key: no elevation, no Task Scheduler, exactly what
/// the Startup folder does but manageable without shell COM.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Atoll";

/// Whether Windows will start Atoll at login — asked of the registry itself,
/// which is the mechanism, so the checkbox can never drift from the truth.
pub fn runs_at_login() -> bool {
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

/// Wire or unwire the login launch. `exe` should be the installed copy —
/// pointing the registry at a build directory would break the launch on the
/// next `cargo clean`.
pub fn set_run_at_login(enabled: bool, exe: &std::path::Path) -> Result<(), String> {
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    let result = unsafe {
        if enabled {
            let command = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                REG_SZ.0,
                Some(command.as_ptr() as *const std::ffi::c_void),
                (command.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
            )
        }
    };
    if result.is_ok() || (!enabled && result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(format!("registry error {}", result.0))
    }
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

/// Bring the terminal window that owns a session to the foreground.
///
/// `ancestors` is the hook's process ancestry, nearest first, captured while
/// the whole chain was alive — the transient shell in it is long dead by now,
/// and that is fine: the first entry that is *still* running the *same*
/// executable and owns a real window is the one to raise. For a Windows
/// Terminal tab that is exactly the `WindowsTerminal.exe` hosting it — each
/// window is its own process — which is what picks the right window when
/// several are open. VS Code's integrated terminal resolves the same way,
/// through the windowless pty host to the main `Code.exe`.
///
/// Returns false when nothing in the chain still has a window: the terminal
/// is gone, or the session predates the ancestry-carrying hook.
pub fn activate_terminal_from(
    ancestors: &[atoll_core::protocol::ProcessRef],
    tab_hint: Option<&str>,
) -> bool {
    let alive = process_exes();
    let candidates = live_candidates(&alive, ancestors, std::process::id());
    if candidates.is_empty() {
        crate::util::debug_log("jump: nothing in the chain is still alive");
        return false;
    }
    for pid in candidates {
        let Some(window) = main_window_of(pid) else {
            crate::util::debug_log(&format!("jump: {pid} alive but windowless, next"));
            continue;
        };
        let exe = alive.get(&pid).map(String::as_str).unwrap_or("?");
        let activated = activate(window);
        crate::util::debug_log(&format!(
            "jump: {pid} ({exe}) window \"{}\" -> {}",
            title_of(window),
            if activated {
                "activated"
            } else {
                "foreground refused"
            },
        ));
        // The window is up; in a tabbed, split terminal the session may still
        // be behind another tab or another pane. Pane first — focusing the
        // pane brings its tab along, which the reverse cannot say — then tab
        // by title as the fallback. Best-effort, and only ever after a
        // successful activation: focus flipped in a background window would
        // be spooky.
        if activated
            && exe == "windowsterminal.exe"
            && let Some(hint) = tab_hint
            && !focus_terminal_pane(window, hint)
        {
            select_terminal_tab(window, hint);
        }
        return activated;
    }
    crate::util::debug_log("jump: chain alive but nobody owns a window");
    false
}

/// Put the tab whose title matches `hint` in front, via UI Automation.
///
/// Windows Terminal exposes its tab strip as UIA tab items whose names are
/// the tab titles, and Claude Code titles its tab after the task it is on —
/// the same summary Atoll's session row shows. Best-effort by design: no
/// match, an ambiguous match, or UIA failing outright all leave the window
/// showing whatever tab it had, which is where window-level activation left
/// things anyway. Every outcome is logged.
fn select_terminal_tab(window: isize, hint: &str) {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationSelectionItemPattern, TreeScope_Descendants, UIA_ControlTypePropertyId,
        UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
    };
    use windows::core::Interface;

    let result: windows::core::Result<()> = (|| unsafe {
        let automation = ui_automation()?;
        let root = automation.ElementFromHandle(hwnd(window))?;
        let condition = automation.CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &variant_i4(UIA_TabItemControlTypeId.0),
        )?;
        let tabs = root.FindAll(TreeScope_Descendants, &condition)?;
        let count = tabs.Length()?;
        let mut names = Vec::with_capacity(count as usize);
        for index in 0..count {
            names.push(tabs.GetElement(index)?.CurrentName()?.to_string());
        }
        let Some(picked) = pick_tab(&names, hint) else {
            crate::util::debug_log(&format!("jump: no tab matched \"{hint}\" among {names:?}"));
            return Ok(());
        };
        let pattern: IUIAutomationSelectionItemPattern = tabs
            .GetElement(picked as i32)?
            .GetCurrentPattern(UIA_SelectionItemPatternId)?
            .cast()?;
        pattern.Select()?;
        crate::util::debug_log(&format!("jump: tab \"{}\" selected", names[picked]));
        Ok(())
    })();
    if let Err(error) = result {
        crate::util::debug_log(&format!("jump: tab selection failed: {error}"));
    }
}

/// A UIA client, on a COM apartment that is whatever the thread already has.
///
/// S_FALSE ("already initialized") is success; RPC_E_CHANGED_MODE means the
/// thread is in the other apartment, which UIA tolerates — so the init
/// return value is deliberately ignored.
unsafe fn ui_automation() -> windows::core::Result<windows::Win32::UI::Accessibility::IUIAutomation>
{
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Accessibility::CUIAutomation;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
    }
}

/// An i32 VARIANT by hand: the crate ships no constructor, and VT_I4 holds
/// nothing that needs freeing.
fn variant_i4(value: i32) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

/// Focus the pane whose screen is showing this session, via UIA text.
///
/// Windows Terminal names every pane after whatever the shell last titled
/// it — with a prompt theme in charge, that is "PowerShell" four times over —
/// but it also exposes each pane's visible text through the text pattern it
/// implements for screen readers. The session's stored title is the head of
/// its last assistant message, which is exactly what the pane has on screen
/// while the session sits waiting for its human. A unique match gets
/// `SetFocus`, and XAML brings that pane's tab forward with it — which is why
/// this runs before, and usually instead of, tab-title matching.
///
/// False when no pane, or more than one, shows the hint: a running session
/// that has scrolled its message off screen simply fails to match here and
/// falls back to the tab pass.
fn focus_terminal_pane(window: isize, hint: &str) -> bool {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
        TextUnit_Character, TreeScope_Descendants, UIA_ControlTypePropertyId,
        UIA_TextControlTypeId, UIA_TextPatternId,
    };
    use windows::core::Interface;

    let focused: windows::core::Result<bool> = (|| {
        unsafe {
            let automation = ui_automation()?;
            let root = automation.ElementFromHandle(hwnd(window))?;
            let condition = automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &variant_i4(UIA_TextControlTypeId.0),
            )?;
            let texts = root.FindAll(TreeScope_Descendants, &condition)?;
            let count = texts.Length()?;
            let mut panes = Vec::new();
            let mut screens = Vec::new();
            for index in 0..count {
                let element = texts.GetElement(index)?;
                // Tab labels are Text elements too; the panes are the
                // TermControls.
                if element.CurrentClassName()? != "TermControl" {
                    continue;
                }
                let pattern: IUIAutomationTextPattern =
                    element.GetCurrentPattern(UIA_TextPatternId)?.cast()?;
                let document = pattern.DocumentRange()?;
                let tail = document.Clone()?;
                tail.MoveEndpointByRange(
                    TextPatternRangeEndpoint_Start,
                    &document,
                    TextPatternRangeEndpoint_End,
                )?;
                let _ = tail.MoveEndpointByUnit(
                    TextPatternRangeEndpoint_Start,
                    TextUnit_Character,
                    -12_000,
                )?;
                screens.push(normalize_pane_text(&tail.GetText(-1)?.to_string()));
                panes.push(element);
            }
            let Some(picked) = pick_pane(&screens, hint) else {
                crate::util::debug_log(&format!(
                    "jump: no single pane shows \"{hint}\" ({} panes)",
                    panes.len()
                ));
                return Ok(false);
            };
            panes[picked].SetFocus()?;
            crate::util::debug_log(&format!("jump: pane {picked} focused"));
            Ok(true)
        }
    })();
    match focused {
        Ok(done) => done,
        Err(error) => {
            crate::util::debug_log(&format!("jump: pane focus failed: {error}"));
            false
        }
    }
}

/// The index of the one pane screen that shows `hint`, if exactly one does.
///
/// `screens` are already normalized. Terminal rendering re-wraps lines and
/// re-draws markdown, so both sides are compared with whitespace and the
/// usual markup glyphs stripped; a hint too short after that would match
/// half the screen and is refused instead.
pub(crate) fn pick_pane(screens: &[String], hint: &str) -> Option<usize> {
    // The stored title ends in a truncation mark the screen never shows, and
    // asking for its whole two hundred characters to be visible in one piece
    // is asking too much: the head of the message is plenty to tell four
    // panes apart.
    let needle: String = normalize_pane_text(hint.trim_end_matches(['.', '…']))
        .chars()
        .take(32)
        .collect();
    if needle.chars().count() < 10 {
        return None;
    }
    let matches: Vec<usize> = screens
        .iter()
        .enumerate()
        .filter(|(_, screen)| screen.contains(&needle))
        .map(|(index, _)| index)
        .collect();
    (matches.len() == 1).then(|| matches[0])
}

/// Lowercased, with whitespace and markdown-ish glyphs dropped.
///
/// The transcript stores the message as written; the terminal shows it as
/// rendered — wrapped at arbitrary columns, bold markers eaten, bullets
/// redrawn. Deleting everything both sides disagree about leaves the
/// characters that actually carry the sentence.
pub(crate) fn normalize_pane_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(c, '*' | '_' | '`' | '#' | '>' | '|' | '-' | '…' | '·' | '•')
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// The index of the one tab that matches `hint`, if exactly one does.
///
/// Titles on both sides are messy — Claude Code prefixes its tab title with a
/// status glyph, the session summary may be a truncation — so matching is by
/// normalized containment either way. Two candidates and no exact tie-break
/// mean no answer: flipping to the wrong tab is worse than staying put.
pub(crate) fn pick_tab(names: &[String], hint: &str) -> Option<usize> {
    fn normalize(text: &str) -> String {
        text.trim_start_matches(|c: char| !c.is_alphanumeric())
            .trim()
            .to_lowercase()
    }
    let wanted = normalize(hint);
    if wanted.len() < 3 {
        return None;
    }
    let normalized: Vec<String> = names.iter().map(|name| normalize(name)).collect();
    let matches: Vec<usize> = normalized
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            name.len() >= 3 && (name.contains(&wanted) || wanted.contains(name.as_str()))
        })
        .map(|(index, _)| index)
        .collect();
    match matches.len() {
        1 => Some(matches[0]),
        0 => None,
        _ => {
            let exact: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|&index| normalized[index] == wanted)
                .collect();
            (exact.len() == 1).then(|| exact[0])
        }
    }
}

/// The entries of `ancestors` still worth asking for a window, nearest first.
///
/// An entry counts only while a process with its pid is running its exe —
/// "same pid, same name" is the guard against PID reuse handing the click to
/// an innocent bystander. `explorer.exe` never counts (it is everyone's
/// ancestor and owns the desktop), and neither do we.
pub(crate) fn live_candidates(
    alive: &HashMap<u32, String>,
    ancestors: &[atoll_core::protocol::ProcessRef],
    self_pid: u32,
) -> Vec<u32> {
    ancestors
        .iter()
        .filter(|entry| {
            entry.pid > 4
                && entry.pid != self_pid
                && entry.exe != "explorer.exe"
                && alive.get(&entry.pid) == Some(&entry.exe)
        })
        .map(|entry| entry.pid)
        .collect()
}

/// Every live process's executable name, lowercased, from one Toolhelp
/// snapshot — a consistent point-in-time view of the process tree.
fn process_exes() -> HashMap<u32, String> {
    let mut table = HashMap::new();
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return table;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            table.insert(
                entry.th32ProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase(),
            );
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }
    table
}

struct FindMainWindow {
    process: u32,
    found: HWND,
}

unsafe extern "system" fn match_main_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut FindMainWindow) };
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    if process != context.process {
        return BOOL(1);
    }
    // The window somebody would call "the app": visible, unowned, not a tool
    // window, and titled. Terminal hosts stand up invisible helper windows
    // too, and those fail one of these four.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    if unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok() {
        return BOOL(1);
    }
    let style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    if style & WS_EX_TOOLWINDOW.0 != 0 {
        return BOOL(1);
    }
    let mut buffer = [0u16; 8];
    if unsafe { GetWindowTextW(hwnd, &mut buffer) } <= 0 {
        return BOOL(1);
    }
    context.found = hwnd;
    BOOL(0)
}

/// The visible top-level window a process would call its main one, if any.
fn main_window_of(pid: u32) -> Option<isize> {
    let mut context = FindMainWindow {
        process: pid,
        found: HWND(std::ptr::null_mut()),
    };
    unsafe {
        let _ = EnumWindows(
            Some(match_main_window),
            LPARAM(&mut context as *mut _ as isize),
        );
    }
    (!context.found.0.is_null()).then_some(context.found.0 as isize)
}

/// Restore if minimized, then bring to the foreground.
///
/// This runs from a click on Atoll's own focused flyout, so this process
/// should hold the foreground and `SetForegroundWindow` should simply work.
/// When Windows refuses anyway — focus was somewhere unexpected — the
/// fallback attaches to the current foreground thread, which makes this
/// thread count as foreground for one more try. Folklore, but the documented
/// kind.
fn activate(handle: isize) -> bool {
    let window = hwnd(handle);
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        if SetForegroundWindow(window).as_bool() {
            return true;
        }

        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() {
            return false;
        }
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let our_thread = GetCurrentThreadId();
        if foreground_thread == 0 || foreground_thread == our_thread {
            return false;
        }
        let _ = AttachThreadInput(our_thread, foreground_thread, true);
        let _ = BringWindowToTop(window);
        let activated = SetForegroundWindow(window).as_bool();
        let _ = AttachThreadInput(our_thread, foreground_thread, false);
        activated
    }
}

/// The window's title, for the log.
fn title_of(handle: isize) -> String {
    let mut buffer = [0u16; 128];
    let length = unsafe { GetWindowTextW(hwnd(handle), &mut buffer) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

#[cfg(test)]
mod tests {
    use super::live_candidates;
    use atoll_core::protocol::ProcessRef;
    use std::collections::HashMap;

    fn chain(entries: &[(u32, &str)]) -> Vec<ProcessRef> {
        entries
            .iter()
            .map(|&(pid, exe)| ProcessRef {
                pid,
                exe: exe.to_string(),
            })
            .collect()
    }

    fn alive(entries: &[(u32, &str)]) -> HashMap<u32, String> {
        entries
            .iter()
            .map(|&(pid, exe)| (pid, exe.to_string()))
            .collect()
    }

    #[test]
    fn the_dead_spawn_shell_is_skipped_and_the_terminal_found() {
        // cmd died with the hook; the CLI, the shell and WT live on.
        let ancestors = chain(&[
            (95, "cmd.exe"),
            (90, "node.exe"),
            (85, "pwsh.exe"),
            (80, "windowsterminal.exe"),
        ]);
        let alive = alive(&[
            (90, "node.exe"),
            (85, "pwsh.exe"),
            (80, "windowsterminal.exe"),
        ]);
        assert_eq!(live_candidates(&alive, &ancestors, 9999), vec![90, 85, 80]);
    }

    #[test]
    fn a_reused_pid_running_something_else_does_not_count() {
        let ancestors = chain(&[(90, "pwsh.exe"), (80, "windowsterminal.exe")]);
        // 90 came back as notepad: same pid, different life.
        let alive = alive(&[(90, "notepad.exe"), (80, "windowsterminal.exe")]);
        assert_eq!(live_candidates(&alive, &ancestors, 9999), vec![80]);
    }

    #[test]
    fn explorer_ourselves_and_system_pids_never_count() {
        let ancestors = chain(&[(4, "system"), (50, "atoll.exe"), (70, "explorer.exe")]);
        let alive = alive(&[(4, "system"), (50, "atoll.exe"), (70, "explorer.exe")]);
        assert_eq!(live_candidates(&alive, &ancestors, 50), Vec::<u32>::new());
    }

    #[test]
    fn an_empty_or_fully_dead_chain_offers_nothing() {
        let ancestors = chain(&[(90, "pwsh.exe")]);
        assert_eq!(
            live_candidates(&HashMap::new(), &ancestors, 9999),
            Vec::<u32>::new()
        );
        assert_eq!(
            live_candidates(&HashMap::new(), &[], 9999),
            Vec::<u32>::new()
        );
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_glyph_prefix_and_case_do_not_hide_a_tab() {
        let tabs = names(&["✳ Say hi", "pwsh in atoll", "✶ Fix the panel"]);
        assert_eq!(super::pick_tab(&tabs, "say hi"), Some(0));
        assert_eq!(super::pick_tab(&tabs, "Fix the panel"), Some(2));
    }

    #[test]
    fn a_truncated_summary_still_finds_its_tab() {
        let tabs = names(&["✳ Rework the taskbar readout states", "✳ Say hi"]);
        assert_eq!(super::pick_tab(&tabs, "Rework the taskbar"), Some(0));
    }

    #[test]
    fn an_ambiguous_match_stays_put_unless_one_is_exact() {
        let tabs = names(&["✳ deploy", "✳ deploy again"]);
        // "deploy" matches both by containment, but exactly one exactly.
        assert_eq!(super::pick_tab(&tabs, "deploy"), Some(0));
        // "dep" is contained in both and exact in neither: no answer.
        assert_eq!(super::pick_tab(&tabs, "deplo"), None);
    }

    #[test]
    fn a_tiny_hint_is_no_hint() {
        let tabs = names(&["✳ ab", "cd"]);
        assert_eq!(super::pick_tab(&tabs, "ab"), None);
    }

    fn screens(list: &[&str]) -> Vec<String> {
        list.iter()
            .map(|text| super::normalize_pane_text(text))
            .collect()
    }

    #[test]
    fn a_wrapped_rerendered_message_still_identifies_its_pane() {
        // The terminal wrapped the line mid-word and ate the bold markers;
        // the transcript kept them. Both normalize to the same characters.
        let screens = screens(&[
            "⏵ bypass permissions on · esc to interrupt",
            "v4 进入总装阶段，流水线：滑\n梯两段预渲染（crop 烘死进素材）\n⏵ waiting",
            "总进度 99% ｜ 视觉分析 99%（报告已落盘）",
        ]);
        let hint = "v4 进入**总装阶段**，流水线：滑梯两段预渲染";
        assert_eq!(super::pick_pane(&screens, hint), Some(1));
    }

    #[test]
    fn two_panes_showing_the_same_text_is_no_answer() {
        let screens = screens(&["deploy the panel already", "deploy the panel already"]);
        assert_eq!(super::pick_pane(&screens, "deploy the panel"), None);
    }

    #[test]
    fn a_scrolled_away_message_matches_nothing() {
        let screens = screens(&["✶ running tools · 4m · ↓64k tokens"]);
        assert_eq!(
            super::pick_pane(&screens, "the message that scrolled off"),
            None
        );
    }

    #[test]
    fn a_hint_that_normalizes_too_short_is_refused() {
        let screens = screens(&["a b c d e f g h i j k"]);
        assert_eq!(super::pick_pane(&screens, "- a b *c*"), None);
    }

    #[test]
    fn a_truncated_title_matches_by_its_head() {
        // The stored title was capped mid-sentence with a truncation mark;
        // the screen shows the sentence whole. The head is what matters.
        let screens = screens(&[
            "总进度 99% ｜ 视觉分析 99%",
            "v4 进入总装阶段，流水线：滑梯两段预渲染（crop 烘死进素材，规避剪映 transform 坐标系风险）→ cutlist_v4 主视频轨",
        ]);
        let hint = "v4 进入总装阶段，流水线：滑梯两段预渲染（crop 烘死进素材，规避剪映 transform 坐标系风险）→ cutlist_v4（主视频轨 59→76 段）→ 草稿...";
        assert_eq!(super::pick_pane(&screens, hint), Some(1));
    }
}
