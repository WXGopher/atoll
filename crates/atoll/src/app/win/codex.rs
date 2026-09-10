//! Official Codex local-thread deep links; never execute a transcript as a command.
use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

pub fn thread_uri(id: &str) -> Option<String> {
    // Local Codex thread IDs are UUIDs. Restrict the path component so a log
    // cannot supply a different URI, query, path, or shell argument.
    (id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }))
    .then(|| format!("codex://threads/{id}"))
}

pub fn available() -> bool {
    let mut size = 0;
    unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            w!("codex"),
            w!("URL Protocol"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .is_ok()
    }
}

pub fn open_thread(id: &str) -> bool {
    let Some(uri) = thread_uri(id).filter(|_| available()) else {
        return false;
    };
    let uri: Vec<u16> = uri.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(uri.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_thread_path_is_not_an_arbitrary_uri() {
        let id = "019d4531-4232-70fc-bcab-0123456789ab";
        assert_eq!(thread_uri(id).unwrap(), format!("codex://threads/{id}"));
        for invalid in [
            "",
            "../settings",
            "new?prompt=test",
            "https://example.com",
            "019d4531-4232-70fc-bcab-0123456789ab?x",
            "019d4531-4232-70fc-bcab-0123456789az",
        ] {
            assert!(thread_uri(invalid).is_none());
        }
    }

    #[test]
    #[ignore = "opens an existing local thread in the installed Codex desktop app"]
    fn native_registered_handler_accepts_local_thread() {
        let id = std::env::var("ATOLL_CODEX_TEST_THREAD").expect("existing local thread id");
        assert!(available());
        assert!(open_thread(&id));
    }
}
