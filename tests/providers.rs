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
    server.start_call(7, "antigravity", json!({"prompt":"run","timeout_ms":2000}));
    let until = Instant::now() + common::WAIT;
    while !ready1.exists() {
        assert!(Instant::now() < until, "timed stub never wrote stdout");
        std::thread::sleep(Duration::from_millis(10));
    }
    let timed: Value = serde_json::from_str(common::text(&server.response(7))).unwrap();
    assert_eq!(timed["observed"]["failure"]["kind"], "timeout", "{timed}");
    assert_eq!(
        timed["observed"]["attempts"][0]["stdout_bytes"], 13,
        "{timed}"
    );
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
    assert_eq!(cancelled["observed"]["state"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["observed"]["failure"]["kind"], "cancellation");
    assert_eq!(
        cancelled["observed"]["attempts"][0]["stderr_bytes"], 13,
        "{cancelled}"
    );
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
        json!({"prompt":"run", "timeout_ms":150}),
    );
    assert_eq!(
        expired["observed"]["failure"]["kind"], "timeout",
        "{expired}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
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
        json!({"prompt": "run", "timeout_ms": 150}),
    );
    assert_eq!(
        expired["observed"]["failure"]["kind"], "timeout",
        "{expired}"
    );
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
    codex_fleet.script(
        "codex",
        json!([
            {"stdout": empty, "sleep_ms": 400},
            {"sleep_ms": 60000}
        ]),
    );
    let mut codex_server = codex_fleet.server("claude");
    codex_server.request(1, "tools/list", json!({}));
    let retried = call(
        &mut codex_server,
        "codex",
        json!({"prompt": "work", "timeout_ms": 1500}),
    );
    assert_eq!(
        retried["observed"]["failure"]["kind"], "timeout",
        "{retried}"
    );
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
        second_deadline < 1500,
        "retry did not carry the remaining absolute deadline: {retried}"
    );
    assert_eq!(
        retried["reported"]["thread_id"], "eeb3a731-d53a-4444-8a6a-4d9d89e76073",
        "{retried}"
    );
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
    let ro = call(
        &mut server,
        "codex",
        json!({"prompt": "second", "access": "read-only"}),
    );
    assert_eq!(ro["observed"]["state"], "finished", "{ro}");
    assert_eq!(ro["reported"]["thread_id"], thread);

    for reply_args in [
        json!({"thread_id": thread, "prompt": "would escalate"}),
        json!({
            "thread_id": thread, "prompt": "explicit settings",
            "access": "read-only", "mode": "advisory", "cwd": fleet.root
        }),
    ] {
        let refused = call(&mut server, "codex-reply", reply_args);
        assert_eq!(
            refused["observed"]["failure"]["kind"], "invalid_request",
            "{refused}"
        );
        assert!(
            refused["observed"]["failure"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("missing failure message: {refused}"))
                .contains("conflicting"),
            "{refused}"
        );
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
        json!([{"stdout": finished, "remove_output": true}]),
    );
    let mut server = fleet.server("claude");
    let failed = call(&mut server, "codex", json!({"prompt": "delete output"}));
    assert_eq!(failed["observed"]["state"], "failed", "{failed}");
    assert_eq!(
        failed["observed"]["failure"]["kind"], "capture",
        "post-spawn read error must not be reported as launch: {failed}"
    );
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
        server.start_call(
            9,
            "antigravity",
            json!({"prompt": "denied stop", "timeout_ms": 400}),
        );
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
        let failed: Value = serde_json::from_str(common::text(&server.response(9))).unwrap();
        assert_eq!(failed["observed"]["state"], "failed", "{failed}");
        assert_eq!(
            failed["observed"]["failure"]["kind"], "unknown",
            "post-spawn supervision failure must not be reported as launch: {failed}"
        );
        assert!(
            failed["observed"]["failure"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("missing failure message: {failed}"))
                .contains("os error 1"),
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
        assert!(!log.exists(), "reaped provider scratch was not removed");
        assert!(server.finish().0.success());
    }
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
