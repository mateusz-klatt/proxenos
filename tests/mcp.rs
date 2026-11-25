//! The MCP layer, through the compiled binary and hand-written frames (D4).
#![cfg(feature = "test-tools")]

mod common;

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
        assert_eq!(names, ["test_echo", "test_fail", "test_panic", "test_wait"]);
        // The schema goes out as it was written.
        let wait = &listed["result"]["tools"][3]["inputSchema"];
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
    let after = server.call(2, "test_echo", json!({ "still": "here" }));
    assert_eq!(text(&after), r#"{"still":"here"}"#);
}

#[test]
fn a_cancelled_call_stops_its_handler() {
    let (mut server, _) = Server::initialized("2025-11-25");
    server.send(&json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "test_wait", "arguments": { "ms": 60_000 } }
    }));
    // Make sure the call is running before it is cancelled.
    let started = Instant::now();
    let pong = server.call(8, "test_echo", json!({}));
    assert_eq!(text(&pong), "{}");
    server.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": 7, "reason": "test" }
    }));
    server.wait_for_stderr("test_wait: cancelled");
    assert!(started.elapsed() < Duration::from_secs(30));

    // The cancelled request gets no answer; the next one does.
    let next = server.call(9, "test_echo", json!({ "n": 9 }));
    assert_eq!(next["id"], 9);
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
    server.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "test_wait", "arguments": { "ms": 60_000 } }
    }));
    let pong = server.call(6, "test_echo", json!({}));
    assert_eq!(text(&pong), "{}");
    let (status, took) = server.finish();
    assert!(status.success(), "{status}");
    // rmcp alone would wait five seconds before cancelling the call.
    assert!(took < Duration::from_secs(3), "exit took {took:?}");
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
    server.wait_for_stderr("proxenos: ");
    assert!(server.stderr().lines().count() >= 2, "{}", server.stderr());
}
