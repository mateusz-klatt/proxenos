//! Raw newline-delimited JSON-RPC 2.0 over stdio client for driving MCP servers.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serialize spawns so pipe creation and close-on-exec never race across threads.
fn spawning() -> MutexGuard<'static, ()> {
    static SPAWNING: Mutex<()> = Mutex::new(());
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
pub struct TimedFrame {
    pub frame: Value,
    pub raw_bytes: usize,
    pub received_at: Instant,
}

pub enum StartOutcome {
    Ready(McpServer),
    ExitedAtStartup {
        exit_code: Option<i32>,
        stderr: String,
        elapsed_ms: f64,
    },
}

pub struct McpServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<TimedFrame, String>>,
    stderr: Arc<Mutex<String>>,
    stderr_done: Option<Receiver<()>>,
    exited: Option<ExitStatus>,
}

impl McpServer {
    pub fn spawn(mut command: Command) -> Result<Self, String> {
        let guard = spawning();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn server: {err}"))?;
        drop(guard);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("server stdout was not piped")?;
        let stderr = child.stderr.take();

        let captured = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&captured);
        let (stderr_tx, stderr_done) = channel();
        thread::spawn(move || {
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut text = sink.lock().unwrap_or_else(PoisonError::into_inner);
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            let _ = stderr_tx.send(());
        });

        let (tx, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let received_at = Instant::now();
                let raw_bytes = line.len();
                let parsed = serde_json::from_str::<Value>(&line)
                    .map(|frame| TimedFrame {
                        frame,
                        raw_bytes,
                        received_at,
                    })
                    .map_err(|err| format!("invalid JSON frame ({err}): {line}"));
                if tx.send(parsed).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            lines,
            stderr: captured,
            stderr_done: Some(stderr_done),
            exited: None,
        })
    }

    /// Spawn a server and perform the MCP `initialize` + `notifications/initialized` handshake.
    /// If the server exits immediately during startup (for example claude-delegator when a
    /// provider CLI is absent), returns `StartOutcome::ExitedAtStartup` cleanly.
    pub fn start_and_initialize(
        command: Command,
        timeout: Duration,
    ) -> Result<StartOutcome, String> {
        let started = Instant::now();
        let mut server = Self::spawn(command)?;
        let init_frame = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "proxenos-comparison", "version": "0" }
            }
        });
        if server.try_send(&init_frame).is_err() {
            let status = server.wait_exit(Duration::from_secs(5))?;
            return Ok(StartOutcome::ExitedAtStartup {
                exit_code: status.code(),
                stderr: server.stderr(),
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            });
        }
        let deadline = Instant::now() + timeout;
        match server.next_frame_until(deadline) {
            Ok(Some(timed)) if timed.frame.get("id") == Some(&json!(0)) => {
                let _ = server.try_send(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                }));
                Ok(StartOutcome::Ready(server))
            }
            Ok(Some(other)) => Err(format!(
                "unexpected frame during initialize: {}",
                other.frame
            )),
            Ok(None) => {
                if let Some(status) = server.try_wait_exit()? {
                    server.drain_stderr(Duration::from_secs(2));
                    Ok(StartOutcome::ExitedAtStartup {
                        exit_code: status.code(),
                        stderr: server.stderr(),
                        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    })
                } else {
                    Err(format!(
                        "timed out waiting for initialize response; stderr: {}",
                        server.stderr()
                    ))
                }
            }
            Err(err) => Err(err),
        }
    }

    pub fn try_send(&mut self, frame: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("server stdin is closed")?;
        let line = frame.to_string();
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("failed to write to server stdin: {err}"))
    }

    pub fn send(&mut self, frame: &Value) -> Result<(), String> {
        self.try_send(frame)
    }

    pub fn request(
        &mut self,
        id: i64,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<TimedFrame, String> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        self.wait_response(id, Instant::now() + timeout)
    }

    pub fn start_call(&mut self, id: i64, name: &str, arguments: Value) -> Result<(), String> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
    }

    pub fn send_cancelled(&mut self, request_id: i64, reason: &str) -> Result<(), String> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": request_id, "reason": reason }
        }))
    }

    pub fn wait_response(&mut self, id: i64, deadline: Instant) -> Result<TimedFrame, String> {
        loop {
            match self.next_frame_until(deadline)? {
                Some(timed) if timed.frame.get("id") == Some(&json!(id)) => return Ok(timed),
                Some(_) => continue,
                None => {
                    return Err(format!(
                        "no response for request {id} before deadline; stderr: {}",
                        self.stderr()
                    ));
                }
            }
        }
    }

    pub fn next_frame_until(&mut self, deadline: Instant) -> Result<Option<TimedFrame>, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    pub fn drain_frames(&mut self) -> Vec<TimedFrame> {
        let mut drained = Vec::new();
        while let Ok(Ok(frame)) = self.lines.try_recv() {
            drained.push(frame);
        }
        drained
    }

    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Sends SIGKILL on Unix (TerminateProcess on Windows) and reaps the server process.
    pub fn sigkill(&mut self) -> Result<ExitStatus, String> {
        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .map_err(|err| format!("failed to wait after kill: {err}"))?;
        self.exited = Some(status);
        self.drain_stderr(Duration::from_secs(2));
        Ok(status)
    }

    pub fn try_wait_exit(&mut self) -> Result<Option<ExitStatus>, String> {
        if let Some(status) = self.exited {
            return Ok(Some(status));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited = Some(status);
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(format!("try_wait failed: {err}")),
        }
    }

    pub fn wait_exit(&mut self, limit: Duration) -> Result<ExitStatus, String> {
        if let Some(status) = self.exited {
            self.drain_stderr(Duration::from_secs(2));
            return Ok(status);
        }
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.try_wait_exit()? {
                self.drain_stderr(deadline.saturating_duration_since(Instant::now()));
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "server did not exit within {:?}; stderr: {}",
                    limit,
                    self.stderr()
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn finish(&mut self, limit: Duration) -> Result<ExitStatus, String> {
        self.close_input();
        self.wait_exit(limit)
    }

    pub fn stderr(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn drain_stderr(&mut self, limit: Duration) {
        if let Some(done) = self.stderr_done.take() {
            let _ = done.recv_timeout(limit);
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        if self.exited.is_some() {
            return;
        }
        self.close_input();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(status);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the text of the first content block of a tool result, if present.
pub fn result_text(frame: &Value) -> Option<&str> {
    frame["result"]["content"][0]["text"].as_str()
}
