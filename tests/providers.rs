//! Provider behaviour over MCP with isolated CLI installations.
#![cfg(feature = "test-tools")]

#[path = "mcp/antigravity.rs"]
mod antigravity;
#[path = "mcp/codex.rs"]
mod codex;
#[allow(dead_code)]
mod common;
#[path = "mcp/content.rs"]
mod content;
#[path = "mcp/discovery.rs"]
mod discovery;
mod provider_support;

#[path = "mcp/git.rs"]
mod git;

use provider_support::{Fleet, call};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[test]
fn both_pairs_are_listed_with_their_own_capabilities() {
    let fleet = Fleet::new(&["codex", "agy"]);
    let mut server = fleet.server("claude");
    let response = server.request(5, "tools/list", json!({}));
    let tools: Vec<_> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| !s["name"].as_str().unwrap().starts_with("test_"))
        .collect();
    assert_eq!(tools.len(), 4);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let fields = &tool["inputSchema"]["properties"];
        assert_eq!(fields["thread_id"].is_object(), name.ends_with("-reply"));
        assert_eq!(fields["effort"].is_object(), name.starts_with("codex"));
        assert!(fields["access"]["description"].as_str().unwrap().contains(
            if name.starts_with("codex") {
                "sandbox"
            } else {
                "shell"
            }
        ));
        assert!(fields["wait"].is_null() && fields["expert"].is_null());
    }
}

#[test]
fn a_chain_that_has_used_either_bridge_stops_before_discovery() {
    for depth in [
        "PROXENOS_DEPTH",
        "CLAUDE_DELEGATOR_AGY_DEPTH",
        "CLAUDE_DELEGATOR_GROK_DEPTH",
    ] {
        let fleet = Fleet::new(&["codex"]);
        let mut cmd = fleet.command("claude");
        cmd.env(depth, "1");
        let mut server = common::Server::start_command(cmd);
        server.initialize("2025-11-25");
        let failure = call(&mut server, "codex", json!({"prompt":"nested"}));
        assert_eq!(failure["observed"]["failure"]["kind"], "depth_limit");
        assert!(fleet.records().is_empty());
    }
}

#[test]
fn only_supported_settings_reach_the_cli_and_coordination_is_opt_in() {
    let fleet = Fleet::new(&["agy"]);
    fleet.script(
        "agy",
        json!([
            provider_support::turn("first"),
            provider_support::turn("second")
        ]),
    );
    let mut cmd = fleet.command("claude");
    cmd.env("IRIS_EXECUTION", "caller-test")
        .env("CLAUDECODE", "1")
        .env("PROXENOS_STUB_MARKER", "keep");
    let mut server = common::Server::start_command(cmd);
    server.initialize("2025-11-25");
    for args in [
        json!({}),
        json!({"prompt":""}),
        json!({"prompt":"go","effort":"high"}),
        json!({"prompt":"go","timeout_ms":0}),
        json!({"prompt":"go","mode":"manage"}),
        json!({"prompt":"go","coordination":{"other":true}}),
    ] {
        let rejected = call(&mut server, "antigravity", args);
        assert_eq!(rejected["observed"]["failure"]["kind"], "invalid_request");
        assert!(rejected["observed"]["passed"].is_null());
    }
    let unknown = call(
        &mut server,
        "antigravity-reply",
        json!({"prompt":"resume","thread_id":"old"}),
    );
    assert!(
        unknown["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("no settings")
    );
    assert!(fleet.records().is_empty());
    for coord in [false, true] {
        let mut args = json!({"prompt":"Respond briefly."});
        if coord {
            args["coordination"] = json!({"caller_agent_name":"requesting-agent"});
        }
        let response = call(&mut server, "antigravity", args);
        assert_eq!(response["observed"]["state"], "finished");
        assert!(response["not_claimed"].as_array().unwrap().len() >= 3);
        assert!(
            !response["reported"]
                .as_object()
                .unwrap()
                .contains_key("state")
        );
        let records = fleet.records();
        let record = records.last().unwrap();
        let prompt = record["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(prompt.contains("requesting-agent"), coord);
        for depth in [
            "PROXENOS_DEPTH",
            "CLAUDE_DELEGATOR_CODEX_DEPTH",
            "CLAUDE_DELEGATOR_KIMI_DEPTH",
        ] {
            assert_eq!(record["env"][depth], "1");
        }
        assert_eq!(record["env"]["PROXENOS_STUB_MARKER"], "keep");
        assert!(record["env"]["IRIS_EXECUTION"].is_null() && record["env"]["CLAUDECODE"].is_null());
    }
}

#[test]
fn launch_failure_and_abnormal_exit_report_different_evidence() {
    let fleet = Fleet::new(&["agy"]);
    fleet.script("agy", json!([{"code":37,"stderr":"worker died"}]));
    let mut server = fleet.server("claude");
    let failed = call(
        &mut server,
        "antigravity",
        json!({"prompt":"run","cwd":fleet.root.join("gone")}),
    );
    assert_eq!(failed["observed"]["failure"]["kind"], "launch");
    assert!(failed["observed"]["passed"].is_null());
    let exit = call(&mut server, "antigravity", json!({"prompt":"run"}));
    assert_eq!(exit["observed"]["failure"]["kind"], "abnormal_exit");
    let attempt = &exit["observed"]["attempts"][0];
    assert_eq!(attempt["exit_code"], 37);
    assert_eq!(attempt["stderr_bytes"], 11);
    assert_eq!(exit["reported"]["diagnostic"], "worker died");
    assert!(attempt["last_output_ms"].is_number());
    assert!(exit["reported"]["failure"].is_null());
}

#[test]
fn timeout_and_disconnect_reap_the_turn_and_return_its_observations() {
    let fleet = Fleet::new(&["agy"]);
    fleet.script("agy",json!([{"stdout":"still working","sleep_ms":60000},{"stderr":"still working","sleep_ms":60000}]));
    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));
    let timed = call(
        &mut server,
        "antigravity",
        json!({"prompt":"run","timeout_ms":600}),
    );
    assert_eq!(timed["observed"]["failure"]["kind"], "timeout");
    assert_eq!(timed["observed"]["attempts"][0]["stdout_bytes"], 13);
    server.start_call(32, "antigravity", json!({"prompt":"cancel me"}));
    let until = Instant::now() + common::WAIT;
    while fleet.records().len() < 3 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    // The SDK suppresses explicitly cancelled requests. EOF signals the same provider
    // token and leaves the output pipe available for the cancellation envelope.
    server.close_input();
    let response = server.response(32);
    let cancelled: Value = serde_json::from_str(common::text(&response)).unwrap();
    assert_eq!(cancelled["observed"]["state"], "cancelled");
    assert_eq!(cancelled["observed"]["failure"]["kind"], "cancellation");
}

#[test]
fn time_spent_in_discovery_can_exhaust_the_deadline_before_a_turn_launches() {
    let fleet = Fleet::new(&["agy"]);
    std::fs::write(
        fleet.root.join("agy.version"),
        json!({"stdout":"1.2.3", "sleep_ms":100}).to_string(),
    )
    .unwrap();
    let mut server = fleet.server("claude");
    let expired = call(
        &mut server,
        "antigravity",
        json!({"prompt":"run", "timeout_ms":1}),
    );
    assert_eq!(expired["observed"]["failure"]["kind"], "timeout");
    assert!(expired["observed"]["passed"].is_null());
    assert!(expired["observed"]["access_mechanism"].is_null());
    assert!(expired["observed"]["attempts"][0]["exit_code"].is_null());
    assert_eq!(fleet.records().len(), 1);
}

#[test]
fn incomplete_capture_reports_why_the_terminal_record_was_unavailable() {
    let fleet = Fleet::new(&["agy"]);
    let long = provider_support::turn(&"x".repeat(4 * 1024 * 1024 + 100));
    fleet.script("agy", json!([long]));
    let mut server = fleet.server("claude");
    let envelope = call(&mut server, "antigravity", json!({"prompt":"report"}));
    assert_eq!(envelope["observed"]["failure"]["kind"], "capture");
    assert!(envelope["reported"]["content"].is_null());
    let attempt = &envelope["observed"]["attempts"][0];
    assert!(
        attempt["stdout_bytes"].as_u64().unwrap() > attempt["stdout_captured"].as_u64().unwrap()
    );
}

#[test]
fn explicitly_cancelling_an_mcp_request_releases_its_provider_files() {
    let fleet = Fleet::new(&["agy"]);
    fleet.script("agy", json!([{"sleep_ms":60000}]));
    let mut server = fleet.server("codex");
    server.start_call(4, "antigravity", json!({"prompt":"stop"}));
    let until = Instant::now() + common::WAIT;
    let log = loop {
        if let Some(record) = fleet.records().get(1) {
            let args = record["args"].as_array().unwrap();
            let flag = args.iter().position(|arg| arg == "--log-file").unwrap();
            break std::path::PathBuf::from(args[flag + 1].as_str().unwrap());
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(log.exists());
    server.send(
        &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}}),
    );
    let deadline = Instant::now() + common::WAIT;
    while log.exists() {
        assert!(
            Instant::now() < deadline,
            "cancelled call still owns its log"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    server.start_call(5, "test_echo", json!({}));
    assert_eq!(server.exact_next_frame()["id"], 5);
}
