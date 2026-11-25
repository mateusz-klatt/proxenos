//! A raw-frame MCP client for driving the compiled binary.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const WAIT: Duration = Duration::from_secs(10);

/// Longer than the server's own ten-second limit after the end of its input.
pub const EXIT_WAIT: Duration = Duration::from_secs(30);

pub const NEW_VERSION: &str = "2026-07-28";

/// Held while a test starts a process. Where the standard library creates a pipe and marks it
/// close-on-exec in two steps, as on macOS, a process that another thread starts in between
/// inherits that end without the mark, and keeps it open for as long as it runs. A test that
/// closes a child's output then sees no failed write, and one that closes a child's input sees the
/// child never exit. Measured on macOS: without the lock, 5 of 20 runs of the MCP tests failed.
pub fn spawning() -> MutexGuard<'static, ()> {
    static SPAWNING: Mutex<()> = Mutex::new(());
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The per-request `_meta` of the 2026-07-28 revision.
pub fn meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": { "name": "proxenos-tests", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

pub struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(args: &[&str]) -> Self {
        let (server, stdout, sender) = Self::spawn(args);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        server
    }

    /// Gives the test the read end of stdout so it can withhold reads or close the pipe.
    pub fn mcp_with_unread_output() -> (Self, ChildStdout) {
        let (server, stdout, _) = Self::spawn(&["mcp", "--client", "claude"]);
        (server, stdout)
    }

    fn spawn(args: &[&str]) -> (Self, ChildStdout, Sender<String>) {
        let spawning = spawning();
        let mut child = Command::new(env!("CARGO_BIN_EXE_proxenos"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary starts");
        drop(spawning);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let captured = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&captured);
        let stderr_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut text = sink.lock().expect("stderr lock");
                text.push_str(&line);
                text.push('\n');
            }
        });
        let (sender, lines) = channel();
        let server = Self {
            child,
            stdin,
            lines,
            stderr: captured,
            stderr_reader: Some(stderr_reader),
        };
        (server, stdout, sender)
    }

    pub fn mcp() -> Self {
        Self::start(&["mcp", "--client", "claude"])
    }

    /// Starts a server and completes the `initialize` handshake of an earlier revision.
    pub fn initialized(version: &str) -> (Self, Value) {
        let mut server = Self::mcp();
        let answer = server.request(
            0,
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "proxenos-tests", "version": "0" }
            }),
        );
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        (server, answer)
    }

    pub fn send(&mut self, frame: &Value) {
        self.send_raw(&frame.to_string());
    }

    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{line}").expect("the server reads its input");
        stdin.flush().expect("the server reads its input");
    }

    /// Sends a request and returns the response with the same id.
    pub fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.response(id)
    }

    /// The next frame, which must arrive in time.
    pub fn exact_next_frame(&mut self) -> Value {
        self.next_frame(Instant::now() + WAIT)
            .expect("a frame arrives in time")
    }

    /// Every frame left on standard output, once the server has closed it.
    pub fn frames_until_end(&mut self) -> Vec<Value> {
        let deadline = Instant::now() + EXIT_WAIT;
        let mut frames = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => frames.push(serde_json::from_str(&line).expect("JSON")),
                Err(RecvTimeoutError::Disconnected) => return frames,
                Err(RecvTimeoutError::Timeout) => panic!("standard output did not end"),
            }
        }
    }

    /// Sends a call without waiting for its answer.
    pub fn start_call(&mut self, id: i64, name: &str, arguments: Value) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
    }

    /// The next response with `id`; other frames before it are skipped.
    pub fn response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let frame = self
                .next_frame(deadline)
                .expect("a response arrives in time");
            if frame.get("id") == Some(&json!(id)) {
                return frame;
            }
        }
    }

    /// The next frame on standard output, or `None` when none arrives before `deadline`.
    pub fn next_frame(&mut self, deadline: Instant) -> Option<Value> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(line) => Some(serde_json::from_str(&line).expect("every output line is JSON")),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    pub fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// Waits until standard error holds `needle`.
    pub fn wait_for_stderr(&self, needle: &str) {
        self.wait_for_stderr_within(needle, WAIT);
    }

    pub fn wait_for_stderr_within(&self, needle: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.stderr().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "standard error never showed {needle:?}: {}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    pub fn take_input(&mut self) -> ChildStdin {
        self.stdin.take().expect("stdin is open")
    }

    /// Closes standard input and waits for the process to exit.
    pub fn finish(&mut self) -> (ExitStatus, Duration) {
        self.close_input();
        self.wait_for_exit(EXIT_WAIT)
    }

    pub fn wait_for_exit(&mut self, limit: Duration) -> (ExitStatus, Duration) {
        let started = Instant::now();
        let deadline = started + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("the child can be waited on") {
                let took = started.elapsed();
                // The process has exited, so its standard error is closed: read all of it.
                if let Some(reader) = self.stderr_reader.take() {
                    reader.join().expect("the stderr reader ends");
                }
                return (status, took);
            }
            assert!(Instant::now() < deadline, "the server did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    /// Ends the server the way a client does, by closing its input, so that it exits by
    /// itself and writes its coverage data. It is killed only if it does not exit in time.
    fn drop(&mut self) {
        self.close_input();
        let deadline = Instant::now() + EXIT_WAIT;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The text of the first content block of a tool result.
pub fn text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("a text result")
}
