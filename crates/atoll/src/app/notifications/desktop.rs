//! Native desktop toasts, with Atoll's own Start-menu identity.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{SyncSender, sync_channel};

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PVCHF_DEFAULT, PropVariantChangeType,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface, PCWSTR};

use super::Completion;

const APP_ID: &str = "Atoll.Desktop";
const APP_ID_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

pub struct Notifier {
    tx: SyncSender<Completion>,
}

impl Notifier {
    pub fn new() -> Self {
        let (tx, rx) = sync_channel::<Completion>(16);
        let spawned = std::thread::Builder::new()
            .name("atoll-notifications".into())
            .spawn(move || {
                if let Err(error) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                    crate::util::debug_log(&format!("notifications unavailable: {error}"));
                    return;
                }
                let mut ready = false;
                let mut active = VecDeque::new();
                for completion in rx {
                    let result = (|| {
                        if !ready {
                            let exe = atoll_core::install::stable_bin_dir()
                                .ok()
                                .map(|dir| dir.join("atoll.exe"))
                                .filter(|path| path.is_file())
                                .or_else(|| std::env::current_exe().ok())
                                .ok_or_else(windows::core::Error::from_win32)?;
                            register_shortcut(&exe)?;
                            ready = true;
                        }
                        let toast = show(&completion)?;
                        active.push_back(toast);
                        while active.len() > 16 {
                            active.pop_front();
                        }
                        Ok::<_, windows::core::Error>(())
                    })();
                    if let Err(error) = result {
                        crate::util::debug_log(&format!("notification failed: {error}"));
                    }
                }
                drop(active);
                unsafe { RoUninitialize() };
            });
        if let Err(error) = spawned {
            crate::util::debug_log(&format!("notification worker: {error}"));
        }
        Self { tx }
    }

    pub fn send(&self, completion: Completion) {
        if let Err(error) = self.tx.try_send(completion) {
            crate::util::debug_log(&format!("notification not queued: {error}"));
        }
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

fn register_shortcut(exe: &Path) -> windows::core::Result<()> {
    unsafe {
        let programs = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)?;
        let path = programs.to_string();
        CoTaskMemFree(Some(programs.0.cast()));
        let path = std::path::PathBuf::from(path?).join("Atoll.lnk");
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(wide(exe.as_os_str()).as_ptr()))?;
        if let Some(parent) = exe.parent() {
            link.SetWorkingDirectory(PCWSTR(wide(parent.as_os_str()).as_ptr()))?;
        }
        link.SetDescription(windows::core::w!("Atoll agent sessions and usage"))?;
        let properties: IPropertyStore = link.cast()?;
        let mut app_id = PROPVARIANT::default();
        PropVariantChangeType(
            &mut app_id,
            &PROPVARIANT::from(APP_ID),
            PVCHF_DEFAULT,
            VT_LPWSTR,
        )?;
        properties.SetValue(&APP_ID_KEY, &app_id)?;
        properties.Commit()?;
        let file: IPersistFile = link.cast()?;
        file.Save(PCWSTR(wide(path.as_os_str()).as_ptr()), true)?;
    }
    Ok(())
}

fn show(completion: &Completion) -> windows::core::Result<ToastNotification> {
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml(&completion.title, &completion.body)))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    // Keep only the latest completion for a session in Action Center.
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    completion.session_id.hash(&mut hash);
    toast.SetTag(&HSTRING::from(format!("{:016x}", hash.finish())))?;
    toast.SetGroup(&HSTRING::from("completion"))?;
    let session = completion.session_id.clone();
    toast.Activated(&TypedEventHandler::new(move |_, _| {
        let session = session.clone();
        let _ = slint::invoke_from_event_loop(move || super::super::notification_clicked(&session));
        Ok(())
    }))?;
    toast.Failed(&TypedEventHandler::new(
        |_, args: windows::core::Ref<windows::UI::Notifications::ToastFailedEventArgs>| {
            if let Some(args) = args.as_ref() {
                crate::util::debug_log(&format!(
                    "Windows rejected notification: {:?}",
                    args.ErrorCode()
                ));
            }
            Ok(())
        },
    ))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(toast)
}

fn xml(title: &str, body: &str) -> String {
    format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual><audio silent=\"true\"/></toast>",
        escape(title),
        escape(body)
    )
}

fn escape(text: &str) -> String {
    text.chars()
        .filter(|c| *c >= ' ' || matches!(c, '\t' | '\n' | '\r'))
        .fold(String::new(), |mut out, c| {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(c),
            }
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "sends a real Windows toast; requires installed Atoll and enabled notifications"]
    fn native_completion_reaches_windows_notification_history() {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.unwrap();
        let exe = atoll_core::install::stable_bin_dir()
            .unwrap()
            .join("atoll.exe");
        assert!(exe.is_file(), "install Atoll first");
        register_shortcut(&exe).unwrap();
        let app_id = HSTRING::from(APP_ID);
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&app_id).unwrap();
        let toast = show(&Completion {
            session_id: format!("atoll-native-test-{}", std::process::id()),
            title: "Atoll notification verification".into(),
            body: "Background completion delivery test.".into(),
        })
        .unwrap();
        // Before the first Show, Setting can return ERROR_NOT_FOUND because
        // Windows has not created this app's notification preferences yet.
        assert_eq!(
            notifier.Setting().unwrap(),
            windows::UI::Notifications::NotificationSetting::Enabled
        );
        let history = ToastNotificationManager::History().unwrap();
        let tag = toast.Tag().unwrap();
        let group = toast.Group().unwrap();
        let mut arrived = false;
        for _ in 0..20 {
            arrived = history
                .GetHistoryWithId(&app_id)
                .unwrap()
                .into_iter()
                .any(|item| item.Tag().ok().as_ref() == Some(&tag));
            if arrived {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        history
            .RemoveGroupedTagWithId(&tag, &group, &app_id)
            .unwrap();
        drop((history, toast, notifier));
        unsafe { RoUninitialize() };
        assert!(
            arrived,
            "Windows did not retain the completion notification"
        );
    }

    #[test]
    fn project_names_cannot_inject_toast_markup() {
        let document = xml("<project>&\u{0}", "done \"now\"");
        assert!(document.contains("&lt;project&gt;&amp;"));
        assert!(!document.contains('\u{0}'));
        assert!(document.contains("done &quot;now&quot;"));
    }
}
