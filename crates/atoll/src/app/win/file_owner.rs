//! Find a plain Codex CLI through the process holding its rollout open.
//! Restart Manager is used only for resource queries; no process is restarted.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use atoll_core::protocol::ProcessRef;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RM_UNIQUE_PROCESS, RmEndSession, RmGetList,
    RmRegisterResources, RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        let _ = unsafe { RmEndSession(self.0) };
    }
}

fn time(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn owners(path: &Path) -> Vec<RM_UNIQUE_PROCESS> {
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let mut handle = 0;
    if unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let session = Session(handle);
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let files = [PCWSTR(path.as_ptr())];
    if unsafe { RmRegisterResources(session.0, Some(&files), None, None) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut entries = vec![RM_PROCESS_INFO::default(); 16];
    for _ in 0..3 {
        let mut needed = 0;
        let mut count = entries.len() as u32;
        let mut reasons = 0;
        let result = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                Some(entries.as_mut_ptr()),
                &mut reasons,
            )
        };
        if result == ERROR_SUCCESS {
            return entries
                .iter()
                .take(count as usize)
                .map(|entry| entry.Process)
                .collect();
        }
        if result != ERROR_MORE_DATA || needed > 128 {
            break;
        }
        entries.resize(needed as usize, RM_PROCESS_INFO::default());
    }
    Vec::new()
}

pub struct Owner {
    pub ancestors: Vec<ProcessRef>,
    pid: u32,
    created: u64,
}

impl Owner {
    pub fn is_alive(&self) -> bool {
        super::target::stamp(self.pid) == Some(self.created)
    }
}

/// Ambiguous owners and reused PIDs cannot choose another session's terminal.
pub fn codex(path: &Path) -> Option<Owner> {
    let owners = owners(path);
    let processes = super::process_tree();
    let mut matches = owners.into_iter().filter(|owner| {
        processes
            .get(&owner.dwProcessId)
            .is_some_and(|p| p.exe == "codex.exe")
            && super::target::stamp(owner.dwProcessId) == Some(time(owner.ProcessStartTime))
    });
    let process = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let mut pid = process.dwProcessId;
    let mut created = time(process.ProcessStartTime);
    let mut ancestors = Vec::new();
    while ancestors.len() < 32 && pid > 4 {
        let Some(entry) = processes.get(&pid) else {
            break;
        };
        if entry.exe == "explorer.exe"
            || ancestors.iter().any(|entry: &ProcessRef| entry.pid == pid)
        {
            break;
        }
        let Some(stamp) = super::target::stamp(pid).filter(|stamp| *stamp <= created) else {
            break;
        };
        ancestors.push(ProcessRef {
            pid,
            exe: entry.exe.clone(),
        });
        created = stamp;
        pid = entry.parent;
    }
    Some(Owner {
        ancestors,
        pid: process.dwProcessId,
        created: time(process.ProcessStartTime),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_file_reports_its_owner_with_the_actual_process_start_time() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let pid = std::process::id();
        let found = owners(file.path());
        let current = found.iter().find(|entry| entry.dwProcessId == pid).unwrap();
        assert_eq!(
            Some(time(current.ProcessStartTime)),
            super::super::target::stamp(pid)
        );
        assert!(
            codex(file.path()).is_none(),
            "a test runner is not a Codex writer"
        );
    }
}
