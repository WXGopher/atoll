//! A local Codex TUI launch with a transparent app-server relay.
//! All RPCs still reach Codex. Only native user-input requests are mirrored to
//! Atoll; the first answer wins, whether it came from the TUI or the card.
mod job;

use atoll_core::protocol::{Command, Envelope, HookPayload, HookSource, Response, events};
use atoll_core::questions::{Answers, InputRequest};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, IsTerminal},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, windows::named_pipe::ClientOptions},
    process::Command as Process,
    sync::mpsc,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response as WsResponse},
    },
};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Native codex.exe path (optional when installed on PATH or through npm).
    #[arg(long)]
    codex_exe: Option<PathBuf>,
    /// Working directory for both the Codex backend and terminal UI.
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    /// Resume this local thread through the relay.
    #[arg(long)]
    resume: Option<String>,
    /// Initial prompt; omit to open the normal interactive terminal UI.
    prompt: Option<String>,
}

pub fn run(args: &Args) -> io::Result<()> {
    attach_console();
    let exe = executable(args.codex_exe.as_ref())?;
    let cwd = args
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    if !cwd.is_dir() {
        return Err(io::Error::other(
            "Codex working directory must be a directory",
        ));
    }
    crate::out::errln!(
        "Atoll: launching Codex with local question cards (experimental app-server transport)."
    );
    let mut terminal = atoll_core::protocol::TerminalMeta::default();
    for key in atoll_core::protocol::TERMINAL_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            terminal.env.insert((*key).into(), Value::String(value));
        }
    }
    if std::io::stdout().is_terminal() {
        let marker = format!(
            "Atoll {:?}",
            unsafe { windows::Win32::System::Com::CoCreateGuid() }.map_err(io::Error::other)?
        );
        crate::out::outln!("{marker}");
        for _ in 0..5 {
            if let Some(target) = crate::app::win::target::capture(&marker) {
                terminal = target.with_env(terminal.env);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?
        .block_on(launch(args, exe, cwd, terminal))
}

fn attach_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleCP, GetStdHandle, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe {
        if GetConsoleCP() != 0 {
            return;
        }
        let handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id).ok()));
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            for (id, handle) in handles {
                if let Some(handle) =
                    handle.filter(|handle| !handle.is_invalid() && !handle.0.is_null())
                {
                    let _ = SetStdHandle(id, handle);
                }
            }
        }
    }
}

fn executable(explicit: Option<&PathBuf>) -> io::Result<PathBuf> {
    if let Some(path) = explicit {
        return path.canonicalize();
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let exe = directory.join("codex.exe");
            if exe.is_file() {
                return Ok(exe);
            }
            let npm = directory.join("node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
            if npm.is_file() {
                return Ok(npm);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "native codex.exe not found; pass --codex-exe PATH",
    ))
}

async fn launch(
    args: &Args,
    exe: PathBuf,
    cwd: PathBuf,
    terminal: atoll_core::protocol::TerminalMeta,
) -> io::Result<()> {
    let job = job::Job::new()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let token = format!(
        "{:?}",
        unsafe { windows::Win32::System::Com::CoCreateGuid() }.map_err(io::Error::other)?
    );
    let mut tui = Process::new(&exe);
    tui.current_dir(&cwd)
        .arg("--remote")
        .arg(format!("ws://{address}"))
        .arg("--remote-auth-token-env")
        .arg("ATOLL_CODEX_RELAY_TOKEN")
        .env("ATOLL_CODEX_RELAY_TOKEN", &token)
        .kill_on_drop(true);
    if let Some(id) = &args.resume {
        tui.arg("resume").arg(id);
    }
    if let Some(prompt) = &args.prompt {
        tui.arg(prompt);
    }
    let mut tui = tui.spawn()?;
    job.assign(
        tui.id()
            .ok_or_else(|| io::Error::other("Codex terminal PID missing"))?,
    )?;
    let serve = async {
        // Reject browser origins and require an ephemeral bearer token. Never
        // accept a second client or expose the Codex backend on the network.
        let socket = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let (stream, _) = listener.accept().await?;
                let expected = format!("Bearer {token}");
                // tungstenite requires this concrete, unboxed HTTP response type.
                #[allow(clippy::result_large_err)]
                let check = |request: &Request,
                             response: WsResponse|
                 -> Result<WsResponse, ErrorResponse> {
                    if request.headers().contains_key("origin")
                        || request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            != Some(expected.as_str())
                    {
                        let mut denied = ErrorResponse::new(Some("Unauthorized".into()));
                        *denied.status_mut() =
                            tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
                        Err(denied)
                    } else {
                        Ok(response)
                    }
                };
                if let Ok(Ok(socket)) =
                    tokio::time::timeout(Duration::from_secs(2), accept_hdr_async(stream, check))
                        .await
                {
                    break Ok::<_, io::Error>(socket);
                }
            }
        })
        .await
        .map_err(io::Error::other)??;
        drop(listener);
        let mut backend = Process::new(&exe);
        if let Some(value) = terminal
            .env
            .get(crate::app::win::target::ENV)
            .and_then(Value::as_str)
        {
            backend.env(crate::app::win::target::ENV, value);
        }
        let mut backend = backend
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .creation_flags(0x08000000)
            .kill_on_drop(true)
            .spawn()?;
        job.assign(
            backend
                .id()
                .ok_or_else(|| io::Error::other("Codex backend PID missing"))?,
        )?;
        let input = backend
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Codex stdin missing"))?;
        let output = backend
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Codex stdout missing"))?;
        let result = relay(
            socket,
            input,
            output,
            cwd.to_string_lossy().into_owned(),
            token,
            terminal,
            atoll_core::pipe::pipe_path(&atoll_core::pipe::pipe_name()),
        )
        .await;
        let _ = backend.kill().await;
        result
    };
    tokio::select! {
        result = serve => {
            // Let the TUI restore its screen and console modes after closing
            // the socket. Killing it immediately can leave the shell raw.
            let status = tokio::time::timeout(Duration::from_secs(2), tui.wait()).await;
            if status.is_err() { let _ = tui.kill().await; }
            result?;
            match status {
                Ok(Ok(status)) if status.success() => Ok(()),
                Ok(Err(error)) => Err(error),
                _ => Err(io::Error::other("Codex terminal disconnected unsuccessfully")),
            }
        }
        status = tui.wait() => {
            if status?.success() { Ok(()) } else { Err(io::Error::other("Codex terminal exited unsuccessfully")) }
        }
    }
}

struct Pending {
    id: Value,
    key: String,
    request: InputRequest,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct Arbitration {
    answered: HashSet<String>,
    order: VecDeque<String>,
}
impl Arbitration {
    fn reopen(&mut self, id: &Value) {
        let key = id.to_string();
        self.answered.remove(&key);
        self.order.retain(|old| old != &key);
    }
    fn take(&mut self, id: &Value) -> bool {
        let key = id.to_string();
        if !self.answered.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > 4096
            && let Some(old) = self.order.pop_front()
        {
            self.answered.remove(&old);
        }
        true
    }
}

async fn relay<S, W, R>(
    socket: S,
    mut input: W,
    output: R,
    cwd: String,
    scope: String,
    terminal: atoll_core::protocol::TerminalMeta,
    pipe_path: String,
) -> io::Result<()>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let (mut client_tx, mut client_rx) = socket.split();
    let mut lines = BufReader::new(output).lines();
    let (answer_tx, mut answer_rx) = mpsc::unbounded_channel::<(Value, String, Answers)>();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut arbiter = Arbitration::default();
    let mut sequence = 0u64;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Err(io::Error::other("Codex app-server closed its output")); };
                let frame: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
                client_tx.send(Message::Text(line.into())).await.map_err(io::Error::other)?;
                let method = frame["method"].as_str().unwrap_or("");
                if !method.is_empty() && let Some(id) = frame.get("id") {
                    arbiter.reopen(id);
                    pending.remove(&id.to_string());
                }
                // Publish lifecycle metadata even when no hooks were installed.
                let (event, thread) = match method {
                    "thread/started" => (events::SESSION_START, frame["params"]["thread"]["id"].as_str()),
                    "turn/started" => (events::USER_PROMPT_SUBMIT, frame["params"]["threadId"].as_str()),
                    "turn/completed" => (if frame["params"]["turn"]["status"] == "completed" { events::STOP } else { events::INTERRUPT }, frame["params"]["threadId"].as_str()),
                    _ => ("", None),
                };
                if let Some(thread) = thread {
                    let mut payload = HookPayload { hook_event_name: Some(event.into()), session_id: Some(thread.into()), cwd: Some(cwd.clone()), ..Default::default() };
                    payload.set_terminal_meta(terminal.clone());
                    let _ = notify_atoll(payload, &pipe_path).await;
                }
                if method == "item/tool/requestUserInput" {
                    let id = &frame["id"];
                    if !(id.is_string() || id.is_i64() || id.is_u64()) { continue; }
                    let Ok(request) = serde_json::from_value::<InputRequest>(frame["params"].clone()) else { continue; };
                    if !request.valid() || pending.len() >= 64 { continue; }
                    let tx = answer_tx.clone();
                    let request_copy = request.clone();
                    let id_copy = id.clone();
                    sequence += 1;
                    let key = format!("{scope}:{sequence}");
                    let request_key = key.clone();
                    let cwd_copy = cwd.clone();
                    let terminal_copy = terminal.clone();
                    let pipe_copy = pipe_path.clone();
                    let task = tokio::spawn(async move {
                        if let Ok(Some(answers)) = ask_atoll(&request_copy, &key, &cwd_copy, terminal_copy, &pipe_copy).await {
                            let _ = tx.send((id_copy, key, answers));
                        }
                    });
                    pending.insert(id.to_string(), Pending { id: id.clone(), key: request_key, request, task });
                } else if method == "serverRequest/resolved" {
                    let id = &frame["params"]["requestId"];
                    if pending.remove(&id.to_string()).is_some() { arbiter.take(id); }
                } else if matches!(method, "turn/started" | "turn/completed" | "thread/closed") {
                    let thread = frame["params"]["threadId"].as_str().unwrap_or("");
                    pending.retain(|_, entry| {
                        if entry.request.thread_id == thread { arbiter.take(&entry.id); false } else { true }
                    });
                }
            }
            message = client_rx.next() => {
                let Some(message) = message else { return Ok(()); };
                match message.map_err(io::Error::other)? {
                    Message::Text(text) => {
                        let frame: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
                        if frame.get("method").is_none() && frame.get("id").is_some() {
                            let id = &frame["id"];
                            if arbiter.answered.contains(&id.to_string()) { continue; }
                            if pending.remove(&id.to_string()).is_some() { arbiter.take(id); }
                        }
                        input.write_all(text.as_bytes()).await?;
                        input.write_all(b"\n").await?;
                        input.flush().await?;
                    }
                    Message::Ping(bytes) => client_tx.send(Message::Pong(bytes)).await.map_err(io::Error::other)?,
                    Message::Close(_) => return Ok(()),
                    _ => {},
                }
            }
            Some((id, request_key, answers)) = answer_rx.recv() => {
                let key = id.to_string();
                if !pending.get(&key).is_some_and(|entry| entry.key == request_key && entry.request.accepts(&answers)) || !arbiter.take(&id) { continue; }
                pending.remove(&key);
                let response = json!({"id":id, "result":{"answers":answers}});
                input.write_all(response.to_string().as_bytes()).await?;
                input.write_all(b"\n").await?;
                input.flush().await?;
            }
        }
    }
}

async fn notify_atoll(payload: HookPayload, path: &str) -> io::Result<()> {
    let mut pipe = ClientOptions::new().open(path)?;
    let envelope = Envelope::Command {
        command: Command::ProcessClaudeHook {
            claude_hook: payload,
            source: HookSource::Codex,
        },
    };
    tokio::time::timeout(
        Duration::from_millis(250),
        pipe.write_all(
            atoll_core::protocol::encode_line(&envelope)
                .map_err(io::Error::other)?
                .as_bytes(),
        ),
    )
    .await
    .map_err(io::Error::other)?
}

async fn ask_atoll(
    request: &InputRequest,
    key: &str,
    cwd: &str,
    terminal: atoll_core::protocol::TerminalMeta,
    path: &str,
) -> io::Result<Option<Answers>> {
    let mut pipe = ClientOptions::new().open(path)?;
    let mut payload = HookPayload {
        hook_event_name: Some(events::CODEX_USER_INPUT.into()),
        session_id: Some(request.thread_id.clone()),
        cwd: Some(cwd.into()),
        tool_name: Some(atoll_core::state::ASK_USER_QUESTION.into()),
        tool_input: Some(serde_json::to_value(request).map_err(io::Error::other)?),
        extra: serde_json::Map::from_iter([("tool_use_id".into(), Value::String(key.into()))]),
        ..Default::default()
    };
    payload.set_terminal_meta(terminal);
    let envelope = Envelope::Command {
        command: Command::ProcessClaudeHook {
            claude_hook: payload,
            source: HookSource::Codex,
        },
    };
    pipe.write_all(
        atoll_core::protocol::encode_line(&envelope)
            .map_err(io::Error::other)?
            .as_bytes(),
    )
    .await?;
    let mut lines = BufReader::new(pipe).lines();
    let line = tokio::time::timeout(Duration::from_secs(86400), lines.next_line())
        .await
        .map_err(io::Error::other)??;
    match line.and_then(|line| serde_json::from_str::<Envelope>(&line).ok()) {
        Some(Envelope::Response {
            response: Response::CodexInput { answers },
        }) if request.accepts(&answers) => Ok(Some(answers)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_answer_wins_and_string_ids_do_not_collide_with_numbers() {
        let mut arbitration = Arbitration::default();
        assert!(arbitration.take(&json!(42)));
        assert!(!arbitration.take(&json!(42)));
        assert!(arbitration.take(&json!("42")));
        arbitration.reopen(&json!(42));
        assert!(arbitration.take(&json!(42)));
        assert!(!arbitration.take(&json!("42")));
    }

    #[tokio::test]
    async fn actual_pipe_replies_cross_relay_once_and_terminal_answer_cancels_card() {
        use atoll_core::server::PipeServer;
        use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
        let name = format!("atoll-relay-test-{}", std::process::id());
        let server = PipeServer::bind(&name).unwrap();
        let path = server.path().to_string();
        let (requests_tx, mut requests_rx) = mpsc::unbounded_channel();
        let server_task = tokio::spawn(server.serve(move |frame, handle| {
            let _ = requests_tx.send((frame, handle));
        }));
        let (web_client, web_server) = tokio::io::duplex(16384);
        let mut client = WebSocketStream::from_raw_socket(web_client, Role::Client, None).await;
        let socket = WebSocketStream::from_raw_socket(web_server, Role::Server, None).await;
        let (backend, proxy) = tokio::io::duplex(16384);
        let (proxy_read, proxy_write) = tokio::io::split(proxy);
        let (backend_read, mut backend_write) = tokio::io::split(backend);
        let mut backend_read = BufReader::new(backend_read).lines();
        let relay_task = tokio::spawn(relay(
            socket,
            proxy_write,
            proxy_read,
            "C:/project".into(),
            "scope".into(),
            Default::default(),
            path,
        ));
        let request = json!({"id":12,"method":"item/tool/requestUserInput","params":{
            "threadId":"thread","turnId":"turn","itemId":"item","isBlocking":true,
            "questions":[{"id":"q","header":"Header","question":"Test question?"}]
        }});
        backend_write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let forwarded = client.next().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(forwarded.to_text().unwrap()).unwrap(),
            request
        );
        let (frame, handle) = tokio::time::timeout(Duration::from_secs(3), requests_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let Envelope::Command {
            command:
                Command::ProcessClaudeHook {
                    claude_hook,
                    source,
                },
        } = frame
        else {
            panic!("missing input event");
        };
        assert_eq!(source, HookSource::Codex);
        assert_eq!(claude_hook.event_name(), events::CODEX_USER_INPUT);
        assert_eq!(
            claude_hook.tool_input.as_ref().unwrap()["questions"][0]["id"],
            "q"
        );
        let answers: Answers =
            serde_json::from_value(json!({"q":{"answers":["中文\nsecond line"]}})).unwrap();
        handle
            .send(&Envelope::Response {
                response: Response::CodexInput {
                    answers: answers.clone(),
                },
            })
            .unwrap();
        let answer: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(3), backend_read.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(answer, json!({"id":12,"result":{"answers":answers}}));
        client
            .send(Message::Text(
                json!({"id":12,"result":{"answers":{"q":{"answers":["late TUI answer"]}}}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), backend_read.next_line())
                .await
                .is_err()
        );
        let mut second = request.clone();
        second["id"] = json!(13);
        backend_write
            .write_all(format!("{second}\n").as_bytes())
            .await
            .unwrap();
        client.next().await.unwrap().unwrap();
        let (_, second_handle) = requests_rx.recv().await.unwrap();
        client
            .send(Message::Text(
                json!({"id":13,"result":{"answers":{"q":{"answers":["TUI wins"]}}}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let native: Value =
            serde_json::from_str(&backend_read.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(native["result"]["answers"]["q"]["answers"][0], "TUI wins");
        tokio::time::timeout(Duration::from_secs(3), async {
            while second_handle.is_open() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // JSON-RPC IDs may be reused after resolution. A new request must not
        // inherit the old response tombstone or the old Atoll correlation key.
        backend_write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        client.next().await.unwrap().unwrap();
        let (reused, reused_handle) = requests_rx.recv().await.unwrap();
        let Envelope::Command {
            command:
                Command::ProcessClaudeHook {
                    claude_hook: reused,
                    ..
                },
        } = reused
        else {
            panic!("missing reused request");
        };
        assert_ne!(
            reused.extra["tool_use_id"],
            claude_hook.extra["tool_use_id"]
        );
        reused_handle
            .send(&Envelope::Response {
                response: Response::CodexInput { answers },
            })
            .unwrap();
        let reused_answer: Value =
            serde_json::from_str(&backend_read.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reused_answer["id"], 12);
        client.close(None).await.unwrap();
        relay_task.await.unwrap().unwrap();
        server_task.abort();
    }
}
