//! Keep a hover window non-activating through backend style/visibility updates.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

const SUBCLASS_ID: usize = 0x41545056;

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    match message {
        WM_STYLECHANGING if wparam.0 as i32 == GWL_EXSTYLE.0 && lparam.0 != 0 => {
            let changing = unsafe { &mut *(lparam.0 as *mut STYLESTRUCT) };
            changing.styleNew |= WS_EX_NOACTIVATE.0;
        }
        WM_WINDOWPOSCHANGING if lparam.0 != 0 => {
            let position = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
            position.flags |= SWP_NOACTIVATE;
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCDESTROY => {
            let _ = unsafe { RemoveWindowSubclass(window, Some(window_proc), id) };
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

/// Called on the window's owning thread, before Slint shows or hides it.
pub fn set_no_activate(handle: isize, enabled: bool) -> bool {
    let window = super::hwnd(handle);
    unsafe {
        if enabled {
            if !SetWindowSubclass(window, Some(window_proc), SUBCLASS_ID, 0).as_bool() {
                return false;
            }
        } else {
            let _ = RemoveWindowSubclass(window, Some(window_proc), SUBCLASS_ID);
        }
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        let style = if enabled {
            style | WS_EX_NOACTIVATE.0
        } else {
            style & !WS_EX_NOACTIVATE.0
        };
        SetWindowLongPtrW(window, GWL_EXSTYLE, style as isize);
    }
    true
}
