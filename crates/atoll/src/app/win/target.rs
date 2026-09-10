//! Stable Windows Terminal tab/pane identities captured at an Atoll launch.
//! Capture requires seeing our unique marker in the pane, not merely focus.
use serde::{Deserialize, Serialize};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Ole::{SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound},
            Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
        UI::{Accessibility::*, WindowsAndMessaging::GetWindowThreadProcessId},
    },
    core::Interface,
};

pub const ENV: &str = "ATOLL_TERMINAL_TARGET";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    window: isize,
    pid: u32,
    created: u64,
    tab: Vec<i32>,
    pane: Vec<i32>,
}

impl Target {
    pub fn with_env(
        &self,
        mut env: serde_json::Map<String, serde_json::Value>,
    ) -> atoll_core::protocol::TerminalMeta {
        if let Ok(value) = serde_json::to_string(self) {
            env.insert(ENV.into(), value.into());
        }
        atoll_core::protocol::TerminalMeta {
            env,
            hook_pid: std::process::id(),
            ancestors: vec![atoll_core::protocol::ProcessRef {
                pid: self.pid,
                exe: "windowsterminal.exe".into(),
            }],
        }
    }
}

struct Apartment(bool);
impl Apartment {
    fn new() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub(super) fn stamp(pid: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = FILETIME::default();
        let (mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let result = GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        result.ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

fn runtime_id(element: &IUIAutomationElement) -> windows::core::Result<Vec<i32>> {
    unsafe {
        let array = element.GetRuntimeId()?;
        let result = (|| {
            let first = SafeArrayGetLBound(array, 1)?;
            let last = SafeArrayGetUBound(array, 1)?;
            let mut ids = Vec::new();
            if last - first > 32 {
                return Ok(ids);
            }
            for i in first..=last {
                let mut value = 0i32;
                SafeArrayGetElement(array, &i, (&mut value as *mut i32).cast())?;
                ids.push(value);
            }
            Ok(ids)
        })();
        let _ = SafeArrayDestroy(array);
        result
    }
}

fn elements(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    control: UIA_CONTROLTYPE_ID,
) -> windows::core::Result<Vec<IUIAutomationElement>> {
    unsafe {
        let condition = automation
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &super::variant_i4(control.0))?;
        let elements = root.FindAll(TreeScope_Descendants, &condition)?;
        (0..elements.Length()?.min(128))
            .map(|i| elements.GetElement(i))
            .collect()
    }
}

fn selected(tab: &IUIAutomationElement) -> bool {
    unsafe {
        tab.GetCurrentPattern(UIA_SelectionItemPatternId)
            .and_then(|p| p.cast::<IUIAutomationSelectionItemPattern>())
            .and_then(|p| p.CurrentIsSelected())
            .is_ok_and(|v| v.as_bool())
    }
}

fn select(tab: &IUIAutomationElement) -> windows::core::Result<()> {
    unsafe {
        tab.GetCurrentPattern(UIA_SelectionItemPatternId)?
            .cast::<IUIAutomationSelectionItemPattern>()?
            .Select()
    }
}

fn screen(element: &IUIAutomationElement) -> windows::core::Result<String> {
    unsafe {
        let document = element
            .GetCurrentPattern(UIA_TextPatternId)?
            .cast::<IUIAutomationTextPattern>()?
            .DocumentRange()?;
        let tail = document.Clone()?;
        tail.MoveEndpointByRange(
            TextPatternRangeEndpoint_Start,
            &document,
            TextPatternRangeEndpoint_End,
        )?;
        tail.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -8000)?;
        Ok(tail.GetText(-1)?.to_string())
    }
}

/// Called after printing a random marker, before starting the TUI. A background
/// launch or an ambiguous marker never binds to whichever pane happens to focus.
pub fn capture(marker: &str) -> Option<Target> {
    let _apartment = Apartment::new();
    let window = super::foreground_window()?;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(super::hwnd(window), Some(&mut pid));
    }
    if super::process_exes().get(&pid).map(String::as_str) != Some("windowsterminal.exe") {
        return None;
    }
    let created = stamp(pid)?;
    let result: windows::core::Result<Option<Target>> = (|| unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = automation.ElementFromHandle(super::hwnd(window))?;
        let panes: Vec<_> = elements(&automation, &root, UIA_TextControlTypeId)?
            .into_iter()
            .filter(|e| {
                e.CurrentClassName().is_ok_and(|name| name == "TermControl")
                    && screen(e).is_ok_and(|text| text.contains(marker))
            })
            .collect();
        if panes.len() != 1 {
            return Ok(None);
        }
        let tabs = elements(&automation, &root, UIA_TabItemControlTypeId)?;
        let Some(tab) = tabs.iter().find(|tab| selected(tab)) else {
            return Ok(None);
        };
        Ok(Some(Target {
            window,
            pid,
            created,
            tab: runtime_id(tab)?,
            pane: runtime_id(&panes[0])?,
        }))
    })();
    result.ok().flatten()
}

/// Select a recorded hidden tab, then the recorded pane. Never pick by a
/// project name, document contents, or the currently focused pane.
pub fn activate(target: &Target) -> bool {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(super::hwnd(target.window), Some(&mut pid));
    }
    if pid != target.pid
        || stamp(pid) != Some(target.created)
        || target.tab.is_empty()
        || target.pane.is_empty()
    {
        return false;
    }
    let _apartment = Apartment::new();
    let result: windows::core::Result<bool> = (|| unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = automation.ElementFromHandle(super::hwnd(target.window))?;
        let tabs = elements(&automation, &root, UIA_TabItemControlTypeId)?;
        let Some(tab) = tabs
            .iter()
            .find(|tab| runtime_id(tab).is_ok_and(|id| id == target.tab))
        else {
            return Ok(false);
        };
        if !super::activate(target.window) {
            return Ok(false);
        }
        let previous = tabs.iter().find(|tab| selected(tab));
        select(tab)?;
        let focus = (|| {
            for pane in elements(&automation, &root, UIA_TextControlTypeId)? {
                if runtime_id(&pane)? == target.pane {
                    pane.SetFocus()?;
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        if !matches!(focus, Ok(true))
            && let Some(previous) = previous
        {
            let _ = select(previous);
        }
        focus
    })();
    result.unwrap_or(false)
}

/// A missing pane may fall back to its own window; never to another window
/// sharing the same Windows Terminal process.
pub fn activate_window(target: &Target) -> bool {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(super::hwnd(target.window), Some(&mut pid));
    }
    pid == target.pid && stamp(pid) == Some(target.created) && super::activate(target.window)
}

pub fn is_foreground(target: &Target) -> bool {
    super::foreground_window() == Some(target.window) && stamp(target.pid) == Some(target.created)
}

pub fn from_meta(meta: &atoll_core::protocol::TerminalMeta) -> Option<Target> {
    serde_json::from_str(meta.env.get(ENV)?.as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_process_or_missing_identity_never_focuses() {
        assert!(!activate(&Target {
            window: 0,
            pid: 0,
            created: 0,
            tab: vec![],
            pane: vec![]
        }));
    }

    #[test]
    #[ignore = "run inside a dedicated named Windows Terminal test window"]
    fn native_hidden_tab_and_running_split_return_to_captured_pane() {
        use std::{io::Write, time::Duration};
        let name =
            std::env::var("ATOLL_NATIVE_TEST_WINDOW").expect("dedicated Terminal window name");
        let result_file = std::env::var("ATOLL_NATIVE_TEST_RESULT").expect("test result path");
        let marker = format!("atoll-pane-test-{}", std::process::id());
        println!("{marker}");
        std::io::stdout().flush().unwrap();
        let target = (0..30)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(100));
                capture(&marker)
            })
            .expect("marker must identify exactly one visible pane");
        let _apartment = Apartment::new();
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap() };
        let check = || {
            assert!(activate(&target));
            std::thread::sleep(Duration::from_millis(100));
            let focused = unsafe { automation.GetFocusedElement().unwrap() };
            assert_eq!(runtime_id(&focused).unwrap(), target.pane);
        };
        let run_wt = |action: &str| {
            assert!(
                std::process::Command::new("wt.exe")
                    .args([
                        "-w",
                        &name,
                        action,
                        "cmd.exe",
                        "/k",
                        "title Atoll test peer"
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
            std::thread::sleep(Duration::from_millis(700));
        };
        run_wt("split-pane");
        // The pane's content changes while the recorded identity stays fixed.
        for i in 0..40 {
            println!("running test output {i}");
        }
        std::io::stdout().flush().unwrap();
        check();
        run_wt("new-tab");
        check();
        std::fs::write(
            result_file,
            serde_json::json!({"passed":true,"window":target.window,"pid":target.pid}).to_string(),
        )
        .unwrap();
    }
}
