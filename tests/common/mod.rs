//! A raw-frame MCP client for driving the compiled binary.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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

struct StderrBuffer {
    text: String,
    eof: bool,
}

pub struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<(Mutex<StderrBuffer>, Condvar)>,
}

impl Server {
    pub fn start(args: &[&str]) -> Self {
        Self::start_command(Self::command(args))
    }

    pub fn command(args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_proxenos"));
        // Even a version probe would execute a real CLI on a developer's machine.
        let absent = std::env::temp_dir().join(format!("proxenos-absent-{}", uuid::Uuid::new_v4()));
        for key in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_STATE_HOME",
        ] {
            command.env(key, &absent);
        }
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("PROXENOS_")
                || name.starts_with("CLAUDE_DELEGATOR_")
                || name == "CODEX_DELEGATOR_CODEX_BIN"
            {
                command.env_remove(key);
            }
        }
        // Cargo runs many independent servers concurrently. Two workers per server still
        // exercise concurrent calls without creating a machine-sized pool for every test.
        command
            .env("TOKIO_WORKER_THREADS", "2")
            .args(args)
            .stderr(Stdio::piped());
        command
    }

    pub fn start_command(command: Command) -> Self {
        let (server, stdout, sender) = Self::spawn(command);
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
        let (server, stdout, _) = Self::spawn(Self::command(&["mcp", "--client", "claude"]));
        (server, stdout)
    }

    fn spawn(mut command: Command) -> (Self, ChildStdout, Sender<String>) {
        let spawning = spawning();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary starts");
        drop(spawning);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take();
        let captured = Arc::new((
            Mutex::new(StderrBuffer {
                text: String::new(),
                eof: false,
            }),
            Condvar::new(),
        ));
        let sink = Arc::clone(&captured);
        thread::spawn(move || {
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let (lock, cvar) = &*sink;
                    let mut state = lock.lock().expect("stderr lock");
                    state.text.push_str(&line);
                    state.text.push('\n');
                    cvar.notify_all();
                }
            }
            let (lock, cvar) = &*sink;
            let mut state = lock.lock().expect("stderr lock");
            state.eof = true;
            cvar.notify_all();
        });
        let (sender, lines) = channel();
        let server = Self {
            child,
            stdin,
            lines,
            stderr: captured,
        };
        (server, stdout, sender)
    }

    pub fn mcp() -> Self {
        Self::start(&["mcp", "--client", "claude"])
    }

    /// Starts a server and completes the `initialize` handshake of an earlier revision.
    pub fn initialized(version: &str) -> (Self, Value) {
        let mut server = Self::mcp();
        let answer = server.initialize(version);
        (server, answer)
    }

    pub fn initialize(&mut self, version: &str) -> Value {
        let answer = self.request(
            0,
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "proxenos-tests", "version": "0" }
            }),
        );
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        answer
    }

    pub fn send(&mut self, frame: &Value) {
        self.send_raw(&frame.to_string());
    }

    pub fn send_raw(&mut self, line: &str) {
        let mut stdin = self.take_input();
        let line = line.to_owned();
        let (sent, done) = channel();
        thread::spawn(move || {
            let result = writeln!(stdin, "{line}").and_then(|()| stdin.flush());
            let _ = sent.send((stdin, result));
        });
        let (stdin, result) = done.recv_timeout(WAIT).expect("the input write finishes");
        self.stdin = Some(stdin);
        result.expect("the server reads its input");
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
        self.wait_for_stderr_count_within(needle, 1, limit);
    }

    /// Waits until standard error holds at least `count` occurrences of `needle`.
    pub fn wait_for_stderr_count(&self, needle: &str, count: usize) {
        self.wait_for_stderr_count_within(needle, count, WAIT);
    }

    fn wait_for_stderr_count_within(&self, needle: &str, count: usize, limit: Duration) {
        let deadline = Instant::now() + limit;
        let (lock, cvar) = &*self.stderr;
        let mut state = lock.lock().expect("stderr lock");
        while state.text.matches(needle).count() < count {
            assert!(
                !state.eof,
                "standard error ended before showing {needle:?} {count} time(s): {}",
                state.text
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "standard error never showed {needle:?} {count} time(s): {}",
                state.text
            );
            let (next, _) = cvar.wait_timeout(state, remaining).expect("stderr condvar");
            state = next;
        }
    }

    /// Returns captured standard error after the server has exited and stderr reached EOF.
    /// While the server is running, use [`Self::wait_for_stderr`] instead of sampling stderr.
    pub fn stderr(&self) -> String {
        let (lock, _) = &*self.stderr;
        let state = lock.lock().expect("stderr lock");
        assert!(
            state.eof,
            "Server::stderr() sampled before stderr reached EOF; wait for a marker with wait_for_stderr() or drain via finish()/wait_for_exit() first"
        );
        state.text.clone()
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
                // An inherited pipe can outlive the server. Bound the wait for its diagnostics too.
                let drain_deadline =
                    Instant::now() + deadline.saturating_duration_since(Instant::now()).max(WAIT);
                let (lock, cvar) = &*self.stderr;
                let mut state = lock.lock().expect("stderr lock");
                while !state.eof {
                    let remaining = drain_deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "the stderr reader did not end");
                    let (next, _) = cvar.wait_timeout(state, remaining).expect("stderr condvar");
                    state = next;
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
