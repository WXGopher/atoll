//! Exercise the GUI lifecycle through its pipe, with isolated data and no
//! taskbar readout. No real agent, credential, hook setting or terminal is used.
#![cfg(windows)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Gui(Child);

impl Gui {
    fn start(root: &Path, pipe: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_atoll"))
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .env("ATOLL_CONFIG_DIR", root)
            .env("ATOLL_PIPE_NAME", pipe)
            .env("USERPROFILE", root)
            .env("HOME", root)
            .env("APPDATA", root)
            .env("LOCALAPPDATA", root)
            .env("CODEX_HOME", root.join(".codex"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let gui = Self(child);
        // A harmless hello also waits until the isolated pipe is listening.
        send(
            pipe,
            &[json!({"type":"hello", "hello":{"client":"display-test"}})],
        );
        gui
    }

    fn wait_for_exit(&mut self) {
        eventually(|| {
            self.0.try_wait().unwrap().map(|status| {
                assert!(status.success());
            })
        });
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn eventually<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for Atoll");
        thread::sleep(Duration::from_millis(50));
    }
}

fn send(pipe: &str, frames: &[Value]) {
    let mut connection = eventually(|| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(atoll_core::pipe::pipe_path(pipe))
            .ok()
    });
    for frame in frames {
        writeln!(connection, "{frame}").unwrap();
    }
    connection.flush().unwrap();
}

fn hook(source: &str, event: &str) -> Value {
    json!({"type":"command", "command": {
        "type":"processClaudeHook", "source":source,
        "claudeHook":{"hook_event_name":event, "session_id":source, "cwd":"display-test"}
    }})
}

fn snapshot(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
#[ignore = "runs isolated Slint GUI processes; requires a Windows desktop"]
fn hooks_control_visibility_and_restarts_preserve_the_last_display() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("display.json");
    let now = atoll_core::now_unix_secs();
    let pipe = format!("atoll-display-test-{}-{now}", std::process::id());
    fs::write(root.join("config.json"), r#"{"taskbar":{"enabled":false}}"#).unwrap();
    let previous = json!({
        "claude":{"visible":true,"lastSeen":now-3600},
        "usage":{"claude":{"limits":[{
            "kind":"weekly_all","label":"Week","percent":77.0,"resets_at":now+86400
        }],"fetched_at":now}},
        "sessions":[{"id":"saved","title":"previous project","detail":"Done",
            "phase":"completed","source":"claude"}],
        "updatedAt":now-3600
    });
    let saved = serde_json::to_vec(&previous).unwrap();
    fs::write(&path, &saved).unwrap();
    let mut gui = Gui::start(root, &pipe);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        fs::read(&path).unwrap(),
        saved,
        "startup leaves the saved display alone"
    );
    let log = fs::read_to_string(root.join("debug.log")).unwrap_or_default();
    assert!(
        !log.contains("usage fetch"),
        "no startup usage fetch: {log}"
    );

    send(&pipe, &[hook("codex", "UserPromptSubmit")]);
    eventually(|| {
        let state = snapshot(&path);
        (state["codex"]["visible"] == true && state["claude"]["visible"] == false).then_some(())
    });
    assert_eq!(
        snapshot(&path)["usage"]["claude"]["limits"][0]["percent"],
        77.0
    );
    assert!(
        snapshot(&path)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["source"] == "codex")
    );

    send(&pipe, &[hook("claude", "UserPromptSubmit")]);
    eventually(|| {
        let state = snapshot(&path);
        (state["claude"]["visible"] == true && state["codex"]["visible"] == true).then_some(())
    });

    // Send these on the same connection to preserve event order. Exit must
    // flush immediately, without waiting for the five-second save interval.
    let shutdown = json!({"type":"event","event":{"type":"shutdown"}});
    send(&pipe, &[hook("claude", "SessionEnd"), shutdown.clone()]);
    gui.wait_for_exit();
    let mut state = snapshot(&path);
    assert_eq!(state["claude"]["visible"], false);
    assert_eq!(state["codex"]["visible"], true);
    assert_eq!(state["sessions"].as_array().unwrap().len(), 1);

    // Simulate a later launch: even an expired visible agent must stay as it
    // was until a new hook arrives, including through an idle clean exit.
    state["codex"]["lastSeen"] = json!(now - 3600);
    let saved = serde_json::to_vec(&state).unwrap();
    fs::write(&path, &saved).unwrap();
    let mut restarted = Gui::start(root, &pipe);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(fs::read(&path).unwrap(), saved);
    send(&pipe, &[shutdown]);
    restarted.wait_for_exit();
    assert_eq!(fs::read(&path).unwrap(), saved);
}
