//! The MCP layer, through the compiled binary and hand-written frames (D4).
#![cfg(feature = "test-tools")]

mod common;
#[path = "mcp/supervisor.rs"]
mod supervisor;

use std::io::Write;
use std::time::{Duration, Instant};

use common::{NEW_VERSION, Server, meta, text};
use serde_json::json;

#[test]
fn every_client_name_is_served() {
    for client in [
        "claude",
        "codex",
        "antigravity",
        "kimi",
        "grok",
        "cursor",
        "copilot",
    ] {
        let mut server = Server::start(&["mcp", "--client", client]);
        server.wait_for_stderr(&format!("serving MCP for {client}"));
        let (status, _) = server.finish();
        assert!(status.success(), "{client}: {status}");
    }
}

#[test]
fn an_earlier_revision_is_served_after_initialize() {
    for version in ["2024-11-05", "2025-06-18", "2025-11-25"] {
        let (mut server, answer) = Server::initialized(version);
        assert_eq!(answer["result"]["protocolVersion"], version);
        assert_eq!(answer["result"]["serverInfo"]["name"], "proxenos");
        assert_eq!(
            answer["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(answer["result"]["capabilities"]["tools"].is_object());

        let listed = server.request(1, "tools/list", json!({}));
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(
            names,
            [
                "test_echo",
                "test_fail",
                "test_panic",
                "test_run",
                "test_wait",
                "await",
                "cancel",
                "usage"
            ]
        );
        // The schema goes out as it was written.
        let wait = &listed["result"]["tools"][4]["inputSchema"];
        assert_eq!(wait["additionalProperties"], false);
        assert_eq!(wait["required"], json!(["ms"]));

        let echoed = server.call(2, "test_echo", json!({ "a": 1, "b": "two" }));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text(&echoed)).expect("JSON text"),
            json!({ "a": 1, "b": "two" })
        );
        assert_ne!(echoed["result"]["isError"], true);
        let (status, _) = server.finish();
        assert!(status.success());
    }
}

#[test]
fn the_revision_that_requires_batches_is_not_offered() {
    // 2025-03-26 requires a server to accept JSON-RPC batches, which rmcp refuses.
    let (mut server, answer) = Server::initialized("2025-03-26");
    assert_eq!(answer["result"]["protocolVersion"], "2025-11-25");
    let discovered = server.request(1, "server/discover", json!({ "_meta": meta(NEW_VERSION) }));
    let supported = discovered["result"]["supportedVersions"].clone();
    assert_eq!(
        supported,
        json!(["2024-11-05", "2025-06-18", "2025-11-25", NEW_VERSION])
    );
}

#[test]
fn the_2026_revision_is_served_without_initialize() {
    let mut server = Server::mcp();
    let discovered = server.request(1, "server/discover", json!({ "_meta": meta(NEW_VERSION) }));
    let supported = discovered["result"]["supportedVersions"]
        .as_array()
        .expect("supported versions");
    assert!(supported.contains(&json!(NEW_VERSION)));
    assert!(supported.contains(&json!("2025-11-25")));

    let called = server.request(
        2,
        "tools/call",
        json!({ "_meta": meta(NEW_VERSION), "name": "test_echo", "arguments": { "x": true } }),
    );
    assert_eq!(text(&called), r#"{"x":true}"#);
    let (status, _) = server.finish();
    assert!(status.success());
}

#[test]
fn an_unsupported_revision_is_refused_as_the_specification_asks() {
    let mut server = Server::mcp();
    let refused = server.request(1, "tools/list", json!({ "_meta": meta("1900-01-01") }));
    assert_eq!(refused["error"]["code"], -32022);
    assert_eq!(refused["error"]["data"]["requested"], "1900-01-01");
}

#[test]
fn a_failing_tool_and_an_unknown_tool_are_told_apart() {
    let (mut server, _) = Server::initialized("2025-11-25");
    let failed = server.call(1, "test_fail", json!({}));
    assert_eq!(failed["result"]["isError"], true);
    assert_eq!(text(&failed), "failed on purpose");

    let unknown = server.call(2, "no_such_tool", json!({}));
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(
        unknown["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("no_such_tool")
    );

    // A call without arguments reaches the tool with none.
    let bare = server.request(3, "tools/call", json!({ "name": "test_echo" }));
    assert_eq!(text(&bare), "{}");
}

#[test]
fn a_panicking_tool_is_answered_and_the_server_carries_on() {
    let (mut server, _) = Server::initialized("2025-11-25");
    let panicked = server.call(1, "test_panic", json!({}));
    assert_eq!(panicked["error"]["code"], -32603);
    assert!(
        panicked["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("test_panic")
    );
    assert!(panicked["error"]["data"].is_null(), "{panicked}");
    let after = server.call(2, "test_echo", json!({ "still": "here" }));
    assert_eq!(text(&after), r#"{"still":"here"}"#);

    let mut cmd = Server::command(&["mcp", "--client", "claude"]);
    cmd.env("PROXENOS_TEST_PANIC_SESSION_LOAD", "1");
    let mut delegation_server = Server::start_command(cmd);
    delegation_server.initialize("2025-11-25");
    let delegation_panic = delegation_server.call(
        1,
        "codex-reply",
        json!({
            "thread_id": "00000000-0000-4000-8000-000000000001",
            "prompt": "trigger session loader panic",
            "cwd": ".",
            "access": "read-only",
            "mode": "advisory"
        }),
    );
    assert_eq!(
        delegation_panic["error"]["code"], -32603,
        "{delegation_panic}"
    );
    let message = delegation_panic["error"]["message"]
        .as_str()
        .expect("panic error message");
    assert!(message.contains("codex-reply"), "{delegation_panic}");
    assert!(
        !message.contains("injected session loader panic"),
        "{delegation_panic}"
    );
    assert_eq!(
        delegation_panic["error"]["data"],
        json!({
            "schema_version": 1,
            "failure": {
                "kind": "unknown",
                "message": message,
                "source": "observed"
            }
        }),
        "{delegation_panic}"
    );
    let still_alive = delegation_server.call(2, "test_echo", json!({ "still": "here" }));
    assert_eq!(text(&still_alive), r#"{"still":"here"}"#);
    let (status, _) = delegation_server.finish();
    assert!(status.success(), "{status}");

    // A panic whose text carries sign-in material is withheld in error.data. Synthetic text,
    // substituted through the test-tools hook because every real panic message is a fixed sentence.
    let mut cmd = Server::command(&["mcp", "--client", "claude"]);
    cmd.env("PROXENOS_TEST_PANIC_SESSION_LOAD", "1").env(
        "PROXENOS_TEST_PANIC_MESSAGE",
        "Sign in at https://login.example.invalid/device?code=ABCD-1234 to continue.",
    );
    let mut withheld_server = Server::start_command(cmd);
    withheld_server.initialize("2025-11-25");
    let withheld = withheld_server.call(
        1,
        "codex-reply",
        json!({
            "thread_id": "00000000-0000-4000-8000-000000000001",
            "prompt": "trigger session loader panic",
            "cwd": ".",
            "access": "read-only",
            "mode": "advisory"
        }),
    );
    assert_eq!(withheld["error"]["code"], -32603, "{withheld}");
    assert_eq!(
        withheld["error"]["data"]["failure"]["message"],
        "Failure details were withheld because they matched a sign-in or secret pattern; check the configuration and environment directly.",
        "{withheld}"
    );
    assert!(
        !withheld.to_string().contains("login.example.invalid"),
        "{withheld}"
    );
    let (status, _) = withheld_server.finish();
    assert!(status.success(), "{status}");
}

#[test]
fn a_cancelled_call_stops_its_handler_and_gets_no_answer() {
    let (mut server, _) = Server::initialized("2025-11-25");
    server.start_call(7, "test_wait", json!({ "ms": 60_000 }));
    server.wait_for_stderr("test_wait: started");
    let started = Instant::now();
    server.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": 7, "reason": "test" }
    }));
    server.wait_for_stderr("test_wait: cancelled");
    assert!(started.elapsed() < Duration::from_secs(30));

    // The frame after the cancellation answers the next request, not the cancelled one.
    server.start_call(9, "test_echo", json!({ "n": 9 }));
    let next = server.exact_next_frame();
    assert_eq!(next["id"], 9, "{next}");
    // Nothing ever answers the cancelled request.
    server.close_input();
    let rest = server.frames_until_end();
    assert!(rest.iter().all(|frame| frame["id"] != 7), "{rest:?}");
}

#[test]
fn a_finished_wait_is_answered() {
    let (mut server, _) = Server::initialized("2025-11-25");
    let waited = server.call(1, "test_wait", json!({ "ms": 1 }));
    assert_eq!(text(&waited), "waited");
}

#[test]
fn the_end_of_input_stops_a_running_call_at_once() {
    let (mut server, _) = Server::initialized("2025-11-25");
    server.start_call(5, "test_wait", json!({ "ms": 60_000 }));
    server.wait_for_stderr("test_wait: started");
    let closed = Instant::now();
    server.close_input();
    server.wait_for_stderr("test_wait: cancelled");
    // rmcp alone would wait five seconds before cancelling the call.
    let took = closed.elapsed();
    assert!(took < Duration::from_secs(4), "cancellation took {took:?}");
    let reply = server.exact_next_frame();
    assert_eq!(reply["id"], 5);
    assert_eq!(text(&reply), "cancelled");
    let (status, _) = server.finish();
    assert!(status.success(), "{status}");
}

#[test]
fn the_server_waits_for_every_running_call_before_it_exits() {
    let (mut server, _) = Server::initialized("2025-11-25");
    for id in 1..=3 {
        server.start_call(id, "test_wait", json!({ "ms": 60_000 }));
    }
    server.wait_for_stderr_count("test_wait: started", 3);
    let (status, _) = server.finish();
    assert!(status.success(), "{status}");
    assert_eq!(
        server.stderr().matches("test_wait: cancelled").count(),
        3,
        "{}",
        server.stderr()
    );
}

#[test]
fn a_call_that_takes_longer_than_rmcp_waits_to_clean_up_still_finishes() {
    let (mut server, _) = Server::initialized("2025-11-25");
    // rmcp stops waiting for running calls five seconds after its input ends.
    server.start_call(1, "test_wait", json!({ "ms": 60_000, "linger_ms": 6_000 }));
    server.wait_for_stderr("test_wait: started");
    let (status, took) = server.finish();
    assert!(status.success(), "{status}");
    assert!(took >= Duration::from_secs(6), "exit took {took:?}");
    assert!(server.frames_until_end().is_empty());
    assert!(
        server.stderr().contains("test_wait: done"),
        "{}",
        server.stderr()
    );
}

#[test]
fn a_call_that_outlasts_the_stop_limit_makes_serving_fail() {
    let (mut server, _) = Server::initialized("2025-11-25");
    server.start_call(1, "test_wait", json!({ "ms": 60_000, "linger_ms": 60_000 }));
    server.wait_for_stderr("test_wait: started");
    let deadline = Instant::now() + common::EXIT_WAIT;
    server.close_input();
    let watchdog_ms =
        server.wait_for_stderr_u64("proxenos test watchdog elapsed_ms=", 1, common::EXIT_WAIT);
    let (status, _) = server.wait_for_exit(deadline.saturating_duration_since(Instant::now()));
    let stderr = server.stderr();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        (9_900..12_000).contains(&watchdog_ms),
        "watchdog slept {watchdog_ms} ms, expected 9900–11999 ms"
    );
    assert!(stderr.contains("did not finish within 10 s"), "{stderr}");
    assert!(!stderr.contains("test_wait: done"), "{stderr}");
}

fn initialize_frame() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "proxenos-tests", "version": "0" }
        }
    })
}

#[test]
fn a_failed_write_stops_calls_and_makes_serving_fail() {
    let (mut server, stdout) = Server::mcp_with_unread_output();
    server.send(&initialize_frame());
    server.start_call(2, "test_wait", json!({ "ms": 60_000, "linger_ms": 1_000 }));
    server.wait_for_stderr("test_wait: started");

    drop(stdout);
    // A write succeeds while any process still holds the read end. A process that another test
    // is starting at this moment holds a copy until it executes the binary (measured on Linux), so
    // answers are asked for until one fails to be written. They are asked for on their own thread,
    // so that a server that stops reading cannot hold this one past its deadline.
    // Writes that block on a server that stops reading never hold up this thread's deadlines.
    let mut stdin = server.take_input();
    let (halted, told) = std::sync::mpsc::channel::<()>();
    let probes = std::thread::spawn(move || {
        for id in 3_u32.. {
            let echo = json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "test_echo", "arguments": {} }
            });
            if writeln!(stdin, "{echo}").is_err() {
                break;
            }
            if told.recv_timeout(Duration::from_millis(50)).is_ok() {
                // A call after the failure, which must never start.
                let late = json!({
                    "jsonrpc": "2.0", "id": "after-the-failure", "method": "tools/call",
                    "params": { "name": "test_wait", "arguments": { "ms": 0 } }
                });
                let _ = writeln!(stdin, "{late}");
                break;
            }
        }
        stdin // Input stays open until the child exits.
    });
    // Input is still open: cancellation shows that the failed write halted serving.
    server.wait_for_stderr("test_wait: cancelled");
    let _ = halted.send(());
    let (status, _) = server.wait_for_exit(Duration::from_secs(7));
    drop(probes.join().expect("the probes finish"));
    assert_eq!(status.code(), Some(1));
    let stderr = server.stderr();
    assert!(stderr.contains("writing the output failed"), "{stderr}");
    assert_eq!(stderr.matches("test_wait: started").count(), 1, "{stderr}");
}

#[test]
fn output_that_nobody_reads_is_bounded_by_the_stop_limit() {
    let (mut server, _unread) = Server::mcp_with_unread_output();
    server.send(&initialize_frame());
    // Answers far larger than any pipe holds, which this test does not read.
    let large = "x".repeat(1 << 20);
    for id in 2..6 {
        server.start_call(id, "test_echo", json!({ "large": large }));
    }
    server.close_input();
    let (status, _) = server.wait_for_exit(Duration::from_secs(15));
    assert_eq!(status.code(), Some(1));
    let stderr = server.stderr();
    assert!(stderr.contains("did not finish within 10 s"), "{stderr}");
}

#[test]
fn a_client_that_reads_late_still_gets_every_answer_whole() {
    let (mut server, mut stdout) = Server::mcp_with_unread_output();
    server.send(&initialize_frame());
    let large = "y".repeat(1 << 20);
    server.start_call(2, "test_echo", json!({ "large": large }));
    server.close_input();
    // Longer than rmcp waits for a pending answer after its input ends.
    std::thread::sleep(Duration::from_secs(6));
    let mut output = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut output).expect("the output is read");
    let (status, _) = server.finish();
    assert!(status.success(), "{status}");
    let frames: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).expect("every frame is whole JSON"))
        .collect();
    assert_eq!(frames.len(), 2, "{} bytes", output.len());
    let echoed: serde_json::Value =
        serde_json::from_str(common::text(&frames[1])).expect("JSON text");
    assert_eq!(echoed["large"].as_str().map(str::len), Some(1 << 20));
}

#[test]
fn output_that_piles_up_unread_past_its_limit_makes_serving_fail() {
    let (mut server, _unread) = Server::mcp_with_unread_output();
    server.send(&initialize_frame());
    server.start_call(2, "test_wait", json!({ "ms": 120_000 }));
    server.wait_for_stderr("test_wait: started");
    let mut stdin = server.take_input();
    let writing = std::thread::spawn(move || {
        let large = "z".repeat(1 << 20);
        for id in 3..80 {
            let call = json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "test_echo", "arguments": { "large": large } }
            });
            if writeln!(stdin, "{call}").is_err() {
                break;
            }
        }
        stdin // Keep input open until the child exits.
    });
    // Allow for slow input under instrumentation. Once overflow cancels the call, a blocked
    // stdout must not keep the process alive for its ten-second shutdown deadline.
    server.wait_for_stderr_within("test_wait: cancelled", Duration::from_secs(120));
    let (status, _) = server.wait_for_exit(Duration::from_secs(7));
    drop(writing.join().expect("the input writer finishes"));
    assert_eq!(status.code(), Some(1));
    let stderr = server.stderr();
    assert!(stderr.contains("more than 64 MiB of output"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn input_that_cannot_be_read_makes_serving_fail() {
    let directory = std::fs::File::open(env!("CARGO_MANIFEST_DIR")).expect("a directory opens");
    let spawning = common::spawning();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_proxenos"))
        .args(["mcp", "--client", "claude"])
        .stdin(directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary starts");
    drop(spawning);
    let deadline = Instant::now() + common::WAIT;
    while child
        .try_wait()
        .expect("the child can be waited on")
        .is_none()
    {
        if Instant::now() >= deadline {
            // Not left running after the test.
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("the binary runs");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("reading the input failed"), "{stderr}");
}

#[test]
fn input_that_ends_before_any_request_is_a_clean_exit() {
    let mut server = Server::mcp();
    let (status, _) = server.finish();
    assert!(status.success());
}

#[test]
fn a_connection_that_cannot_be_served_is_a_failure() {
    // A notification as the very first frame leaves no lifecycle to select.
    let mut server = Server::mcp();
    server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let (status, _) = server.finish();
    assert_eq!(status.code(), Some(1));
    let stderr = server.stderr();
    let reason = stderr
        .lines()
        .find(|line| line.starts_with("proxenos: ") && !line.contains("serving MCP"));
    assert!(reason.is_some(), "{stderr}");
}

#[test]
fn a_failed_handshake_is_reported_while_output_is_blocked() {
    let (mut server, _unread) = Server::mcp_with_unread_output();
    let stdin = server.take_input();
    // On its own thread, so that the wait below is bounded even if the server stops reading.
    let writing = std::thread::spawn(move || {
        // Answers to pings before initialize, more than a pipe holds and none of them read.
        let pings = (0..20_000).map(|id| json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }));
        let wrong = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let mut input = std::io::BufWriter::new(stdin);
        for frame in pings.chain([wrong]) {
            if writeln!(input, "{frame}").is_err() {
                break;
            }
        }
        let _ = input.flush();
        input // Input stays open until the child exits.
    });
    // The handshake failure alone ends serving, and its reason survives the drain that the
    // blocked output cuts short.
    let (status, _) = server.wait_for_exit(Duration::from_secs(20));
    drop(writing.join().expect("the input writer finishes"));
    assert_eq!(status.code(), Some(1));
    let stderr = server.stderr();
    let reason = stderr
        .lines()
        .find(|line| line.starts_with("proxenos: ") && !line.contains("serving MCP"));
    assert!(
        reason.is_some_and(|line| line.contains("initialized") && !line.contains("did not finish")),
        "{stderr}"
    );
}
