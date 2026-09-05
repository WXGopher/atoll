//! Keep the readout's native frame consistent with its role as a taskbar control.
//!
//! Winit's undecorated top-level windows still carry WS_CAPTION: it hides their
//! frame in WM_NCCALCSIZE instead. Reparenting that window into Explorer leaves
//! the caption bits behind, and winit can rewrite both styles when its flags
//! change. A repaint cannot fix that. Filter the proposed styles synchronously,
//! before Windows can paint a caption or recalculate a smaller client area.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

const SUBCLASS_ID: usize = 0x41544f4c;

fn style(value: u32, embedded: bool) -> u32 {
    let value =
        value & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0;
    if embedded {
        (value | WS_CHILD.0) & !WS_POPUP.0
    } else {
        value & !WS_CHILD.0
    }
}

fn extended_style(value: u32) -> u32 {
    (value | WS_EX_TOOLWINDOW.0)
        & !(WS_EX_APPWINDOW
            | WS_EX_WINDOWEDGE
            | WS_EX_CLIENTEDGE
            | WS_EX_STATICEDGE
            | WS_EX_DLGMODALFRAME)
            .0
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    embedded: usize,
) -> LRESULT {
    match message {
        WM_STYLECHANGING if lparam.0 != 0 => {
            // Windows owns STYLESTRUCT for the duration of this synchronous
            // callback and explicitly permits changing its proposed style.
            let changing = unsafe { &mut *(lparam.0 as *mut STYLESTRUCT) };
            match wparam.0 as i32 {
                index if index == GWL_STYLE.0 => {
                    changing.styleNew = style(changing.styleNew, embedded != 0);
                }
                index if index == GWL_EXSTYLE.0 => {
                    changing.styleNew = extended_style(changing.styleNew);
                }
                _ => {}
            }
            return LRESULT(0);
        }
        WM_NCCALCSIZE => {
            // The entire rectangle is client area, for both forms of this
            // message (including the wParam=0 path winit delegates to Windows).
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            let _ = unsafe { RemoveWindowSubclass(window, Some(window_proc), id) };
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

/// Install/update the readout-only frame guard on its owning UI thread.
/// The callback owns no allocation and removes itself when the window dies.
pub fn prepare(handle: isize, embedded: bool) -> bool {
    let window = super::hwnd(handle);
    unsafe {
        if !SetWindowSubclass(
            window,
            Some(window_proc),
            SUBCLASS_ID,
            usize::from(embedded),
        )
        .as_bool()
        {
            return false;
        }
        let old_style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
        let old_extended = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        let new_style = style(old_style, embedded);
        let new_extended = extended_style(old_extended);
        if old_style != new_style || old_extended != new_extended {
            SetWindowLongPtrW(window, GWL_STYLE, new_style as isize);
            SetWindowLongPtrW(window, GWL_EXSTYLE, new_extended as isize);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
    true
}
