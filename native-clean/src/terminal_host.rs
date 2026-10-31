//! Headless host-side terminal service.
//!
//! The host owns process state; the WebRTC peer only transports framed bytes. No
//! desktop session, accessibility permission, clipboard permission, or shell
//! command is opened until a viewer sends an explicit OPEN frame.

use crate::wire;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, Mutex},
};

const MAX_SESSIONS: usize = 32;
const SCROLLBACK: usize = 256 * 1024;
const CHUNK: usize = 16 * 1024;

#[derive(Clone)]
pub struct TerminalHost {
    sessions: Arc<Mutex<HashMap<u32, Arc<Session>>>>,
    next_id: Arc<std::sync::atomic::AtomicU32>,
}

struct Session {
    id: u32,
    name: String,
    shell: String,
    state: AtomicBool,
    stdin: Mutex<Option<ChildStdin>>,
    stop: mpsc::Sender<()>,
    sink: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    scrollback: Mutex<VecDeque<u8>>,
}

impl TerminalHost {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(std::sync::atomic::AtomicU32::new(1)),
        }
    }

    /// Consume one complete BINARY_DATA_CHANNEL message and enqueue zero or more
    /// complete terminal frames on `out`.
    pub async fn handle(&self, bytes: &[u8], out: mpsc::UnboundedSender<Vec<u8>>) {
        let Ok(header) = wire::parse_header(bytes) else {
            return;
        };
        let Ok(payload) = wire::frame_payload(bytes, header) else {
            return;
        };
        match header.kind {
            wire::LIST_REQ => self.list(header.session, out).await,
            wire::OPEN => self.open(header.session, payload, out).await,
            wire::STDIN => self.write(header.session, payload).await,
            wire::RESIZE => {}
            wire::DETACH => self.detach(header.session).await,
            wire::CLOSE | wire::KILL_REQ => self.kill(header.session, out).await,
            _ => {}
        }
    }

    async fn list(&self, request_id: u32, out: mpsc::UnboundedSender<Vec<u8>>) {
        let sessions = self.sessions.lock().await;
        let values = sessions.values().map(|session| json!({
            "session_id": session.id,
            "name": session.name,
            "shell": session.shell,
            "state": if session.state.load(Ordering::Acquire) { "running" } else { "exited" },
        })).collect::<Vec<_>>();
        let payload = serde_json::to_vec(&values).unwrap_or_else(|_| b"[]".to_vec());
        let _ = out.send(wire::terminal_frame(wire::LIST_RESP, request_id, &payload));
    }

    async fn open(&self, requested: u32, payload: &[u8], out: mpsc::UnboundedSender<Vec<u8>>) {
        let options: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let (cols, rows) = dimensions(&options);
        let existing = if requested != 0 {
            self.sessions.lock().await.get(&requested).cloned()
        } else {
            None
        };
        let session = match existing {
            Some(session) => session,
            None => match self
                .spawn(
                    requested,
                    cols,
                    rows,
                    options.get("shell").and_then(Value::as_str),
                )
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    let body = json!({"code": 1, "message": error}).to_string();
                    let _ = out.send(wire::terminal_frame(
                        wire::OPEN_RESP,
                        requested,
                        body.as_bytes(),
                    ));
                    return;
                }
            },
        };
        *session.sink.lock().await = Some(out.clone());
        let _ = out.send(wire::terminal_frame(
            wire::OPEN_RESP,
            session.id,
            br#"{"code":0,"message":"ok"}"#,
        ));
        let replay = session
            .scrollback
            .lock()
            .await
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for chunk in replay.chunks(CHUNK) {
            let _ = out.send(wire::terminal_frame(wire::STDOUT, session.id, chunk));
        }
    }

    async fn spawn(
        &self,
        requested: u32,
        _cols: u16,
        _rows: u16,
        requested_shell: Option<&str>,
    ) -> Result<Arc<Session>, String> {
        {
            let sessions = self.sessions.lock().await;
            if sessions.len() >= MAX_SESSIONS {
                return Err("terminal session limit reached".into());
            }
        }
        let shell = shell_command(requested_shell);
        let mut command = Command::new(&shell);
        if cfg!(windows) {
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]);
        } else {
            command.arg("-l");
        }
        let mut child = command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "could not start host shell".to_owned())?;
        let stdin = child.stdin.take().ok_or("host shell has no stdin")?;
        let stdout = child.stdout.take().ok_or("host shell has no stdout")?;
        let stderr = child.stderr.take().ok_or("host shell has no stderr")?;
        let id = if requested != 0 {
            requested
        } else {
            self.allocate_id().await
        };
        if id == 0 {
            return Err("invalid terminal session id".into());
        }
        let (stop, stop_rx) = mpsc::channel(1);
        let session = Arc::new(Session {
            id,
            name: format!("session-{id}"),
            shell,
            state: AtomicBool::new(true),
            stdin: Mutex::new(Some(stdin)),
            stop,
            sink: Mutex::new(None),
            scrollback: Mutex::new(VecDeque::new()),
        });
        self.sessions.lock().await.insert(id, session.clone());
        self.spawn_reader(session.clone(), stdout);
        self.spawn_reader(session.clone(), stderr);
        self.spawn_waiter(session.clone(), child, stop_rx);
        Ok(session)
    }

    async fn allocate_id(&self) -> u32 {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed).max(1);
            if !self.sessions.lock().await.contains_key(&id) {
                return id;
            }
        }
    }

    fn spawn_reader<R>(&self, session: Arc<Session>, mut reader: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let size = match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(size) => size,
                };
                let mut scrollback = session.scrollback.lock().await;
                scrollback.extend(&buffer[..size]);
                while scrollback.len() > SCROLLBACK {
                    scrollback.pop_front();
                }
                drop(scrollback);
                if let Some(sink) = session.sink.lock().await.as_ref() {
                    let _ = sink.send(wire::terminal_frame(
                        wire::STDOUT,
                        session.id,
                        &buffer[..size],
                    ));
                }
            }
        });
    }

    fn spawn_waiter(
        &self,
        session: Arc<Session>,
        mut child: Child,
        mut stop_rx: mpsc::Receiver<()>,
    ) {
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                result = child.wait() => result.ok(),
                _ = stop_rx.recv() => { let _ = child.kill().await; child.wait().await.ok() },
            };
            let exit_code = status.and_then(|status| status.code()).unwrap_or(-1);
            session.state.store(false, Ordering::Release);
            let body = json!({"exit_code": exit_code}).to_string();
            if let Some(sink) = session.sink.lock().await.as_ref() {
                let _ = sink.send(wire::terminal_frame(
                    wire::EXIT,
                    session.id,
                    body.as_bytes(),
                ));
            }
            sessions.lock().await.remove(&session.id);
        });
    }

    async fn write(&self, id: u32, bytes: &[u8]) {
        let session = self.sessions.lock().await.get(&id).cloned();
        if let Some(session) = session {
            if let Some(stdin) = session.stdin.lock().await.as_mut() {
                let _ = stdin.write_all(bytes).await;
                let _ = stdin.flush().await;
            }
        }
    }

    async fn detach(&self, id: u32) {
        if let Some(session) = self.sessions.lock().await.get(&id).cloned() {
            *session.sink.lock().await = None;
        }
    }

    async fn kill(&self, id: u32, out: mpsc::UnboundedSender<Vec<u8>>) {
        if let Some(session) = self.sessions.lock().await.get(&id).cloned() {
            *session.sink.lock().await = None;
            let _ = session.stop.send(()).await;
            let _ = out.send(wire::terminal_frame(
                wire::KILL_RESP,
                id,
                br#"{"code":0,"message":"ok"}"#,
            ));
        }
    }
}

fn shell_command(requested: Option<&str>) -> String {
    if let Some(shell) = requested.filter(|shell| !shell.contains('/') && !shell.contains('\\')) {
        if !shell.is_empty() {
            return shell.to_owned();
        }
    }
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

fn dimensions(value: &Value) -> (u16, u16) {
    let number = |key: &str, default: u64| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default)
            .clamp(1, 1000) as u16
    };
    (number("cols", 120), number("rows", 39))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn opens_two_independent_sessions() {
        let host = TerminalHost::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        host.handle(
            &wire::terminal_frame(wire::OPEN, 0, br#"{"cols":80,"rows":24}"#),
            tx.clone(),
        )
        .await;
        host.handle(
            &wire::terminal_frame(wire::OPEN, 0, br#"{"cols":80,"rows":24}"#),
            tx.clone(),
        )
        .await;
        let mut opened = Vec::new();
        for _ in 0..2 {
            let bytes = timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let header = wire::parse_header(&bytes).unwrap();
            if header.kind == wire::OPEN_RESP {
                opened.push(header.session);
            }
        }
        assert_eq!(opened.len(), 2);
        assert_ne!(opened[0], opened[1]);
        for id in opened {
            host.handle(&wire::terminal_frame(wire::KILL_REQ, id, &[]), tx.clone())
                .await;
        }
    }
}
