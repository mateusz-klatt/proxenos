//! Provider behaviour over MCP with isolated CLI installations.
#![cfg(feature = "test-tools")]

#[path = "mcp/accounting.rs"]
mod accounting;
#[path = "mcp/antigravity.rs"]
mod antigravity;
#[path = "mcp/background.rs"]
mod background;
#[path = "mcp/claude.rs"]
mod claude;
#[path = "mcp/codex.rs"]
mod codex;
#[allow(dead_code)]
mod common;
#[path = "mcp/content.rs"]
mod content;
#[path = "mcp/copilot.rs"]
mod copilot;
#[path = "mcp/discovery.rs"]
mod discovery;
#[path = "mcp/kimi.rs"]
mod kimi;
mod provider_support;
#[path = "mcp/sessions.rs"]
mod sessions;

#[path = "mcp/git.rs"]
mod git;

use provider_support::{Fleet, assert_observed_failure, assert_reported_failure, call};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[test]
fn both_pairs_and_shared_tools_are_listed_with_their_own_capabilities() {
    let fleet = Fleet::new(&["codex", "agy"]);
    let mut server = fleet.server("claude");
    let response = server.request(5, "tools/list", json!({}));
    let tools: Vec<_> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| !s["name"].as_str().unwrap().starts_with("test_"))
        .collect();
    assert_eq!(tools.len(), 6);
    let mut provider_count = 0;
    let mut shared = Vec::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let fields = &tool["inputSchema"]["properties"];
        if matches!(name, "await" | "cancel") {
            shared.push(name);
            continue;
        }
        provider_count += 1;
        assert_eq!(fields["thread_id"].is_object(), name.ends_with("-reply"));
        assert_eq!(fields["effort"].is_object(), name.starts_with("codex"));
        assert!(fields["access"]["description"].as_str().unwrap().contains(
            if name.starts_with("codex") {
                "sandbox"
            } else {
                "shell"
            }
        ));
        assert_eq!(fields["wait"]["type"], "boolean");
        assert_eq!(fields["wait"]["default"], true);
        assert_eq!(fields["memory_limit_bytes"]["type"], "integer");
        assert_eq!(fields["memory_limit_bytes"]["minimum"], 1);
        assert!(fields["expert"].is_null());
    }
    assert_eq!(provider_count, 4);
    assert_eq!(shared, ["await", "cancel"]);
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
        assert_observed_failure(
            &failure,
            "depth_limit",
            "delegated CLI cannot start another",
        );
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
    for (args, detail) in [
        (json!({}), "Missing prompt"),
        (json!({"prompt":""}), "prompt"),
        (json!({"prompt":"go","effort":"high"}), "effort"),
        (json!({"prompt":"go","timeout_ms":0}), "timeout_ms"),
        (json!({"prompt":"go","mode":"manage"}), "mode"),
        (
            json!({"prompt":"go","coordination":{"other":true}}),
            "coordination",
        ),
    ] {
        let rejected = call(&mut server, "antigravity", args);
        assert_observed_failure(&rejected, "invalid_request", detail);
        assert!(rejected["observed"]["passed"].is_null());
    }
    let unknown = call(
        &mut server,
        "antigravity-reply",
        json!({"prompt":"resume","thread_id":"old"}),
    );
    assert_observed_failure(&unknown, "invalid_request", "no settings");
    assert!(fleet.records().is_empty());
    fleet.assert_steps_remaining("agy", 2);
    for coord in [false, true] {
        let mut args = json!({"prompt":"Respond briefly."});
        if coord {
            args["coordination"] = json!({"caller_agent_name":"requesting-agent"});
        }
        let response = call(&mut server, "antigravity", args);
        assert_eq!(response["observed"]["state"], "finished");
        fleet.assert_steps_remaining("agy", usize::from(!coord));
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
    let turns = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"] != json!(["--version"]))
        .count();
    assert_eq!(turns, 2);
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
    assert_observed_failure(&failed, "launch", "");
    // A launch failure from the version probe would not exercise the missing directory.
    assert!(
        !failed["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Version check"),
        "{failed}"
    );
    assert!(failed["observed"]["passed"].is_null());
    fleet.assert_steps_remaining("agy", 1);
    let exit = call(&mut server, "antigravity", json!({"prompt":"run"}));
    assert_observed_failure(&exit, "abnormal_exit", "exit code 37");
    let attempt = &exit["observed"]["attempts"][0];
    assert_eq!(attempt["exit_code"], 37);
    assert_eq!(attempt["stderr_bytes"], 11);
    assert_eq!(exit["reported"]["diagnostic"], "worker died");
    assert!(attempt["last_output_ms"].is_number());
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn timeout_and_disconnect_reap_the_turn_and_return_its_observations() {
    let fleet = Fleet::new(&["agy"]);
    let ready1 = fleet.root.join("timed.ready");
    let ready2 = fleet.root.join("cancel.ready");
    fleet.script(
        "agy",
        json!([
            {"stdout":"still working","ready":ready1,"sleep_ms":60000},
            {"stderr":"still working","ready":ready2,"sleep_ms":60000}
        ]),
    );
    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));
    server.start_call(7, "antigravity", json!({"prompt":"run","timeout_ms":3_000}));
    let until = Instant::now() + common::WAIT;
    while !ready1.exists() {
        assert!(Instant::now() < until, "timed stub never wrote stdout");
        std::thread::sleep(Duration::from_millis(10));
    }
    let timed: Value = serde_json::from_str(common::text(&server.response(7))).unwrap();
    assert_observed_failure(&timed, "timeout", "exceeded the call's deadline");
    assert_eq!(
        timed["observed"]["attempts"][0]["stdout_bytes"], 13,
        "{timed}"
    );
    fleet.assert_steps_remaining("agy", 1);
    server.start_call(32, "antigravity", json!({"prompt":"cancel me"}));
    let until = Instant::now() + common::WAIT;
    while !ready2.exists() {
        assert!(Instant::now() < until, "cancelled stub never wrote stderr");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The SDK suppresses explicitly cancelled requests. EOF signals the same provider
    // token and leaves the output pipe available for the cancellation envelope.
    server.close_input();
    let response = server.response(32);
    let cancelled: Value = serde_json::from_str(common::text(&response)).unwrap();
    assert_observed_failure(&cancelled, "cancellation", "was cancelled");
    assert_eq!(
        cancelled["observed"]["attempts"][0]["stderr_bytes"], 13,
        "{cancelled}"
    );
    fleet.assert_steps_remaining("agy", 0);
    let turns = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"] != json!(["--version"]))
        .count();
    assert_eq!(turns, 2);
}

#[test]
fn time_spent_in_discovery_can_exhaust_the_deadline_before_a_turn_launches() {
    let fleet = Fleet::new(&["agy"]);
    std::fs::write(
        fleet.root.join("agy.version"),
        json!({"stdout":"1.2.3", "sleep_ms":60000}).to_string(),
    )
    .unwrap();
    let mut server = fleet.server("claude");
    let started = Instant::now();
    let expired = call(
        &mut server,
        "antigravity",
        json!({"prompt":"run", "timeout_ms":500}),
    );
    assert_observed_failure(&expired, "timeout", "Version check exceeded its deadline");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "discovery ignored the call deadline: {:?}",
        started.elapsed()
    );
    assert!(expired["observed"]["passed"].is_null());
    assert!(expired["observed"]["access_mechanism"].is_null());
    assert!(
        fleet
            .records()
            .iter()
            .all(|record| record["args"] == json!(["--version"]))
    );
}

#[test]
fn call_deadline_applies_through_before_snapshot_and_retry_attempts() {
    let fleet = Fleet::new(&["git", "agy"]);
    fleet.script("git", json!([{"sleep_ms": 5000}]));
    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));
    let expired = call(
        &mut server,
        "antigravity",
        json!({"prompt": "run", "timeout_ms": 1_000}),
    );
    assert_observed_failure(&expired, "timeout", "exceeded the call's deadline");
    assert!(expired["observed"]["passed"].is_null(), "{expired}");
    assert!(
        expired["observed"]["access_mechanism"].is_null(),
        "{expired}"
    );
    assert_eq!(
        expired["observed"]["attempts"]
            .as_array()
            .unwrap_or_else(|| panic!("missing attempts: {expired}"))
            .len(),
        1
    );
    assert!(expired["observed"]["attempts"][0]["exit_code"].is_null());
    assert!(
        fleet
            .records()
            .iter()
            .filter(|record| record["cli"] == "agy")
            .all(|record| record["args"] == json!(["--version"]))
    );

    let codex_fleet = Fleet::new(&["codex"]);
    let empty = include_str!("fixtures/codex/empty.jsonl");
    let attempt2_ready = codex_fleet.root.join("attempt2.ready");
    codex_fleet.script(
        "codex",
        json!([
            {"stdout": empty, "sleep_ms": 400},
            {"ready": attempt2_ready, "sleep_ms": 60000}
        ]),
    );
    let mut codex_server = codex_fleet.server("claude");
    codex_server.request(1, "tools/list", json!({}));
    let retried = call(
        &mut codex_server,
        "codex",
        json!({"prompt": "work", "timeout_ms": 5_000}),
    );
    assert!(
        attempt2_ready.exists(),
        "retry attempt never started before deadline: {retried}"
    );
    assert_observed_failure(&retried, "timeout", "exceeded the call's deadline");
    assert_eq!(
        retried["observed"]["attempts"]
            .as_array()
            .unwrap_or_else(|| panic!("missing attempts: {retried}"))
            .len(),
        2
    );
    let second_deadline = retried["observed"]["attempts"][1]["deadline_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("missing second attempt deadline_ms: {retried}"));
    assert!(
        second_deadline < 5_000,
        "retry did not carry the remaining absolute deadline: {retried}"
    );
    assert_eq!(
        retried["reported"]["thread_id"], "eeb3a731-d53a-4444-8a6a-4d9d89e76073",
        "{retried}"
    );
    codex_fleet.assert_steps_remaining("codex", 0);
    let execs = codex_fleet
        .records()
        .into_iter()
        .filter(|record| record["args"][0] == "exec")
        .count();
    assert_eq!(execs, 2);
}

#[test]
fn conflicting_starts_for_the_same_thread_id_refuse_replies() {
    let fleet = Fleet::new(&["codex"]);
    let finished = include_str!("fixtures/codex/finished.jsonl");
    let thread = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";
    fleet.script("codex", json!([{"stdout": finished}, {"stdout": finished}]));
    let mut server = fleet.server("claude");
    let full = call(
        &mut server,
        "codex",
        json!({"prompt": "first", "access": "full"}),
    );
    assert_eq!(full["observed"]["state"], "finished", "{full}");
    assert_eq!(full["reported"]["thread_id"], thread);
    fleet.assert_steps_remaining("codex", 1);
    let ro = call(
        &mut server,
        "codex",
        json!({"prompt": "second", "access": "read-only"}),
    );
    assert_eq!(ro["observed"]["state"], "finished", "{ro}");
    assert_eq!(ro["reported"]["thread_id"], thread);
    fleet.assert_steps_remaining("codex", 0);

    for reply_args in [
        json!({"thread_id": thread, "prompt": "would escalate"}),
        json!({
            "thread_id": thread, "prompt": "explicit settings",
            "access": "read-only", "mode": "advisory", "cwd": fleet.root
        }),
    ] {
        let refused = call(&mut server, "codex-reply", reply_args);
        assert_observed_failure(&refused, "invalid_request", "conflicting");
    }
    let execs: Vec<_> = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"][0] == "exec")
        .collect();
    assert_eq!(execs.len(), 2, "ambiguous thread_id must never dispatch");
}

#[test]
fn post_spawn_output_capture_failure_keeps_dispatch_evidence() {
    let fleet = Fleet::new(&["codex"]);
    let finished = include_str!("fixtures/codex/finished.jsonl");
    fleet.script(
        "codex",
        json!([
            {"stdout": finished, "remove_output": true},
            {"stdout": finished, "replace_output_with_dir": true}
        ]),
    );
    let mut server = fleet.server("claude");
    for (index, (prompt, expected_detail)) in [
        ("delete output", "Could not read the provider output file"),
        ("dir output", "not a regular file"),
    ]
    .into_iter()
    .enumerate()
    {
        let failed = call(&mut server, "codex", json!({"prompt": prompt}));
        assert_observed_failure(&failed, "capture", expected_detail);
        assert!(failed["observed"]["passed"].is_object(), "{failed}");
        assert!(
            failed["observed"]["access_mechanism"].is_string(),
            "{failed}"
        );
        assert_eq!(
            failed["observed"]["attempts"]
                .as_array()
                .unwrap_or_else(|| panic!("missing attempts: {failed}"))
                .len(),
            1
        );
        assert_eq!(failed["observed"]["attempts"][0]["exit_code"], 0);
        assert!(
            failed["observed"]["attempts"][0]["stdout_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        fleet.assert_steps_remaining("codex", 1 - index);
    }
    let execs = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"][0] == "exec")
        .count();
    assert_eq!(execs, 2);
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
mod linux_supervision {
    #![allow(unsafe_code)]

    use super::*;
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    fn server_without_sigterm(fleet: &Fleet) -> common::Server {
        let command = fleet.command("claude");
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let instruction = |code: u32, jf, k| libc::sock_filter {
                code: code as u16,
                jt: 0,
                jf,
                k,
            };
            let number = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
            let signal = (std::mem::offset_of!(libc::seccomp_data, args) + size_of::<u64>()) as u32;
            let denied = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
            let mut filter = [
                instruction(BPF_LD | BPF_W | BPF_ABS, 0, number),
                instruction(BPF_JMP | BPF_JEQ | BPF_K, 3, libc::SYS_kill as u32),
                instruction(BPF_LD | BPF_W | BPF_ABS, 0, signal),
                instruction(BPF_JMP | BPF_JEQ | BPF_K, 1, libc::SIGTERM as u32),
                instruction(BPF_RET | BPF_K, 0, denied),
                instruction(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            let mode = libc::SECCOMP_MODE_FILTER as libc::c_ulong;
            let filter_ptr = &raw const program;
            // SAFETY: prctl installs the complete, initialized seccomp filter on this
            // fresh thread only; the filter stays live for the syscall.
            unsafe {
                assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_usize, 0, 0, 0), 0);
                assert_eq!(libc::prctl(libc::PR_SET_SECCOMP, mode, filter_ptr, 0, 0), 0);
            }
            let mut server = common::Server::start_command(command);
            server.initialize("2025-11-25");
            let _ = send.send(server);
        });
        receive
            .recv_timeout(common::WAIT)
            .expect("server with seccomp filter")
    }

    #[test]
    fn denied_stop_after_spawn_keeps_dispatch_evidence_and_cleans_up_after_reap() {
        let fleet = Fleet::new(&["agy"]);
        let ready = fleet.root.join("live.ready");
        fleet.script("agy", json!([{"ready": ready, "sleep_ms": 60000}]));
        let mut server = server_without_sigterm(&fleet);
        server.request(1, "tools/list", json!({}));
        server.start_call(9, "antigravity", json!({"prompt": "denied stop"}));
        let until = Instant::now() + common::WAIT;
        while !ready.exists() {
            assert!(Instant::now() < until, "stub did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = {
            let record = fleet.records().pop().unwrap();
            let args = record["args"].as_array().unwrap();
            let pos = args.iter().position(|arg| arg == "--log-file").unwrap();
            std::path::PathBuf::from(args[pos + 1].as_str().unwrap())
        };
        assert!(log.exists(), "provider scratch must exist while stub runs");
        server.close_input();
        let failed: Value = serde_json::from_str(common::text(&server.response(9))).unwrap();
        assert_observed_failure(&failed, "unknown", "os error 1");
        assert!(
            failed["observed"]["failure"]["message"]
                .as_str()
                .unwrap()
                .contains("Supervision failed"),
            "{failed}"
        );
        assert!(failed["observed"]["passed"].is_object(), "{failed}");
        assert!(
            failed["observed"]["access_mechanism"].is_string(),
            "{failed}"
        );
        assert_eq!(
            failed["observed"]["attempts"].as_array().unwrap().len(),
            1,
            "{failed}"
        );
        assert_eq!(failed["observed"]["attempts"][0]["signal"], libc::SIGKILL);
        fleet.assert_steps_remaining("agy", 0);
        assert!(!log.exists(), "reaped provider scratch was not removed");
        assert!(server.wait_for_exit(common::EXIT_WAIT).0.success());
    }
}

#[cfg(unix)]
#[test]
fn fifo_replacement_of_output_file_fails_without_blocking() {
    let fleet = Fleet::new(&["codex"]);
    let finished = include_str!("fixtures/codex/finished.jsonl");
    fleet.script(
        "codex",
        json!([{"stdout": finished, "replace_output_with_fifo": true}]),
    );
    let mut server = fleet.server("claude");
    let failed = call(&mut server, "codex", json!({"prompt": "fifo output"}));
    assert_observed_failure(&failed, "capture", "not a regular file");
    assert!(failed["observed"]["passed"].is_object(), "{failed}");
    assert_eq!(failed["observed"]["attempts"][0]["exit_code"], 0);
    fleet.assert_steps_remaining("codex", 0);
}

#[test]
fn preflight_respects_call_deadline_and_cancellation() {
    // Part A: expired call deadline stops post-discovery preflight immediately.
    let fleet = Fleet::new(&["git", "agy"]);
    let store = fleet.root.join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("paper-crane.db"), []).unwrap();
    fleet.script("git", json!([{"sleep_ms": 5000}]));
    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));
    let timed = call(
        &mut server,
        "antigravity-reply",
        json!({
            "thread_id": "paper-crane", "prompt": "timeout preflight",
            "model": "named", "access": "full", "mode": "advisory",
            "cwd": fleet.root, "timeout_ms": 1_000
        }),
    );
    assert_observed_failure(&timed, "timeout", "preflight");
    assert_eq!(timed["observed"]["attempts"], json!([]));

    // Part B: connection EOF cancels post-discovery preflight cleanly.
    let fleet2 = Fleet::new(&["git", "agy"]);
    let store2 = fleet2.root.join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&store2).unwrap();
    std::fs::write(store2.join("paper-crane.db"), []).unwrap();
    let ready = fleet2.root.join("git.ready");
    fleet2.script("git", json!([{"ready": ready, "sleep_ms": 60000}]));
    let mut server2 = fleet2.server("claude");
    server2.request(1, "tools/list", json!({}));
    server2.start_call(
        20,
        "antigravity-reply",
        json!({
            "thread_id": "paper-crane", "prompt": "cancel preflight",
            "model": "named", "access": "full", "mode": "advisory",
            "cwd": fleet2.root
        }),
    );
    let until = Instant::now() + common::WAIT;
    while !ready.exists() {
        assert!(Instant::now() < until, "git before-snapshot did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    server2.close_input();
    let cancelled: Value = serde_json::from_str(common::text(&server2.response(20))).unwrap();
    assert_observed_failure(&cancelled, "cancellation", "preflight");
    assert_eq!(cancelled["observed"]["attempts"], json!([]));
    fleet2.assert_steps_remaining("git", 0);
}

#[test]
fn start_store_ignores_failed_starts_and_evicts_oldest_entries() {
    let fleet = Fleet::new(&["agy"]);
    let store = fleet.root.join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&store).unwrap();
    for id in ["s1", "s2", "s3", "failed"] {
        std::fs::write(store.join(format!("{id}.db")), []).unwrap();
    }
    let ok = |id: &str| {
        json!({
            "stdout": json!({"status": "SUCCESS", "conversation_id": id, "response": id}).to_string()
        })
    };
    let err_step = json!({
        "stdout": json!({"status": "ERROR", "conversation_id": "failed", "response": "", "error": "timeout waiting for response"}).to_string(),
        "code": 1
    });
    fleet.script(
        "agy",
        json!([
            ok("s1"),
            err_step,
            ok("s2"),
            ok("s3"),
            ok("s2"),
            ok("s1"),
            ok("s1"),
            ok("s2"),
            ok("s3"),
            ok("s2"),
            ok("s1")
        ]),
    );

    // Part A: with durable session files enabled, failed starts are ignored everywhere, while
    // evicting s1 from StartStore drops its in-memory-only instructions and falls back to disk.
    let mut command = fleet.command("claude");
    command.env("PROXENOS_TEST_START_LIMIT", "2");
    let mut server = common::Server::start_command(command);
    server.initialize("2025-11-25");

    let s1 = call(
        &mut server,
        "antigravity",
        json!({"prompt": "1", "model": "m1", "instructions": "cache-only-note"}),
    );
    assert_eq!(s1["observed"]["state"], "finished", "{s1}");
    fleet.assert_steps_remaining("agy", 10);
    let failed = call(
        &mut server,
        "antigravity",
        json!({"prompt": "bad", "model": "m-bad"}),
    );
    assert_reported_failure(
        &failed,
        Some(("abnormal_exit", "exit code 1")),
        "timeout",
        "timeout waiting for response",
    );
    assert_eq!(failed["reported"]["thread_id"], "failed", "{failed}");
    fleet.assert_steps_remaining("agy", 9);
    let unremembered_failed = call(
        &mut server,
        "antigravity-reply",
        json!({"thread_id": "failed", "prompt": "retry"}),
    );
    assert_observed_failure(&unremembered_failed, "invalid_request", "no settings");
    fleet.assert_steps_remaining("agy", 9);

    let s2 = call(
        &mut server,
        "antigravity",
        json!({"prompt": "2", "model": "m2", "instructions": "still-cached-note"}),
    );
    assert_eq!(s2["observed"]["state"], "finished", "{s2}");
    fleet.assert_steps_remaining("agy", 8);
    let s3 = call(
        &mut server,
        "antigravity",
        json!({"prompt": "3", "model": "m3"}),
    );
    assert_eq!(s3["observed"]["state"], "finished", "{s3}");
    fleet.assert_steps_remaining("agy", 7);

    // Limit is 2, so s2 remained in StartStore (retaining its non-persisted instructions) while
    // s1 was evicted from StartStore and served from the durable session file (retaining model
    // "m1" without its non-persisted instructions).
    let kept = call(
        &mut server,
        "antigravity-reply",
        json!({"thread_id": "s2", "prompt": "still-cached"}),
    );
    assert_eq!(kept["observed"]["state"], "finished", "{kept}");
    assert_eq!(kept["observed"]["passed"]["model"], "m2", "{kept}");
    let from_file = call(
        &mut server,
        "antigravity-reply",
        json!({"thread_id": "s1", "prompt": "from-file"}),
    );
    assert_eq!(from_file["observed"]["state"], "finished", "{from_file}");
    assert_eq!(
        from_file["observed"]["passed"]["model"], "m1",
        "{from_file}"
    );
    let prints: Vec<String> = fleet
        .records()
        .into_iter()
        .filter_map(|record| {
            record["args"]
                .as_array()?
                .windows(2)
                .find(|pair| pair[0] == "--print")
                .and_then(|pair| pair[1].as_str().map(str::to_owned))
        })
        .collect();
    assert!(prints[4].contains("still-cached-note"), "{}", prints[4]);
    assert!(!prints[5].contains("cache-only-note"), "{}", prints[5]);
    assert!(server.finish().0.success());

    // Part B: without a usable durable state directory, eviction from StartStore drops s1.
    let mut mem_only = fleet.command("claude");
    mem_only
        .env("PROXENOS_TEST_START_LIMIT", "2")
        .env("PROXENOS_AGY_CONVERSATIONS_DIR", &store)
        .env("HOME", "relative-home")
        .env("LOCALAPPDATA", "relative-localappdata")
        .env("XDG_STATE_HOME", "relative-xdg");
    let mut server2 = common::Server::start_command(mem_only);
    server2.initialize("2025-11-25");

    for (prompt, model) in [("1", "m1"), ("2", "m2"), ("3", "m3")] {
        let done = call(
            &mut server2,
            "antigravity",
            json!({"prompt": prompt, "model": model}),
        );
        assert_eq!(done["observed"]["state"], "finished", "{done}");
    }
    let evicted = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "s1", "prompt": "evicted"}),
    );
    assert_observed_failure(&evicted, "invalid_request", "no settings");
    let kept2 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "s2", "prompt": "still remembered"}),
    );
    assert_eq!(kept2["observed"]["state"], "finished", "{kept2}");
    assert_eq!(kept2["observed"]["passed"]["model"], "m2", "{kept2}");
    let explicit = call(
        &mut server2,
        "antigravity-reply",
        json!({
            "thread_id": "s1", "prompt": "explicit",
            "cwd": fleet.root, "access": "full", "mode": "advisory", "model": "m1-explicit"
        }),
    );
    assert_eq!(explicit["observed"]["state"], "finished", "{explicit}");
    assert_eq!(
        explicit["observed"]["passed"]["model"], "m1-explicit",
        "{explicit}"
    );
    fleet.assert_steps_remaining("agy", 0);
    let turns = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"] != json!(["--version"]))
        .count();
    assert_eq!(turns, 11);
}

#[cfg(windows)]
#[test]
fn windows_provider_directory_cleanup_retries_after_sharing_violation() {
    use std::os::windows::fs::OpenOptionsExt;

    let fleet = Fleet::new(&["agy"]);
    let ready = fleet.root.join("win.ready");
    let release = fleet.root.join("win.release");
    let mut step = provider_support::turn("clean");
    step["ready"] = json!(ready);
    step["wait_for"] = json!(release);
    fleet.script("agy", json!([step]));
    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));
    server.start_call(11, "antigravity", json!({"prompt": "retry cleanup"}));
    let until = Instant::now() + common::WAIT;
    while !ready.exists() {
        assert!(Instant::now() < until, "stub did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = {
        let record = fleet.records().pop().unwrap();
        let args = record["args"].as_array().unwrap();
        let pos = args.iter().position(|arg| arg == "--log-file").unwrap();
        std::path::PathBuf::from(args[pos + 1].as_str().unwrap())
    };
    // FILE_SHARE_READ (1) lets read_output read the file after exit while denying delete sharing.
    let deny_delete = std::fs::File::options()
        .read(true)
        .share_mode(1)
        .open(&log)
        .unwrap();
    let releaser = std::thread::spawn(move || {
        std::fs::write(&release, []).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(deny_delete);
    });
    let done: Value = serde_json::from_str(common::text(&server.response(11))).unwrap();
    releaser.join().unwrap();
    assert_eq!(done["observed"]["state"], "finished", "{done}");
    fleet.assert_steps_remaining("agy", 0);
    assert!(
        !log.exists(),
        "provider scratch survived after sharing violation cleared"
    );
    assert!(server.finish().0.success());
    assert!(!server.stderr().contains("could not remove temporary file"));
}

#[test]
fn incomplete_capture_reports_why_the_terminal_record_was_unavailable() {
    let fleet = Fleet::new(&["agy"]);
    let long = provider_support::turn(&"x".repeat(4 * 1024 * 1024 + 100));
    fleet.script("agy", json!([long]));
    let mut server = fleet.server("claude");
    let envelope = call(&mut server, "antigravity", json!({"prompt":"report"}));
    assert_observed_failure(&envelope, "capture", "capture limit");
    assert!(envelope["reported"]["content"].is_null());
    let attempt = &envelope["observed"]["attempts"][0];
    assert!(
        attempt["stdout_bytes"].as_u64().unwrap() > attempt["stdout_captured"].as_u64().unwrap()
    );
    fleet.assert_steps_remaining("agy", 0);
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
