use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{Fleet, assert_observed_failure, assert_reported_failure, call};

const CAPTURED_START: &str = include_str!("../fixtures/kimi/captured-start.stdout.jsonl");
const CAPTURED_REPLY: &str = include_str!("../fixtures/kimi/captured-reply.stdout.jsonl");
const CAPTURED_UNKNOWN_MODEL_OUT: &str =
    include_str!("../fixtures/kimi/captured-unknown-model.stdout.jsonl");
const CAPTURED_UNKNOWN_MODEL_ERR: &str =
    include_str!("../fixtures/kimi/captured-unknown-model.stderr.txt");
const CAPTURED_SESSION_NOT_FOUND_OUT: &str =
    include_str!("../fixtures/kimi/captured-session-not-found.stdout.jsonl");
const CAPTURED_SESSION_NOT_FOUND_ERR: &str =
    include_str!("../fixtures/kimi/captured-session-not-found.stderr.txt");
const CAPTURED_MISSING_LOGIN_OUT: &str =
    include_str!("../fixtures/kimi/captured-missing-login.stdout.jsonl");
const CAPTURED_MISSING_LOGIN_ERR: &str =
    include_str!("../fixtures/kimi/captured-missing-login.stderr.txt");
const CAPTURED_PLAN_REJECTED_ERR: &str =
    include_str!("../fixtures/kimi/captured-plan-rejected.stderr.txt");
const SYNTHETIC_MULTI_CHUNK: &str =
    include_str!("../fixtures/kimi/synthetic-multi-chunk.stdout.jsonl");
const SYNTHETIC_NO_ASSISTANT: &str =
    include_str!("../fixtures/kimi/synthetic-no-assistant.stdout.jsonl");
const SYNTHETIC_NO_COMPLETION: &str =
    include_str!("../fixtures/kimi/synthetic-no-completion.stdout.jsonl");
const SYNTHETIC_WRONG_DIRECTORY_ERR: &str =
    include_str!("../fixtures/kimi/synthetic-wrong-directory.stderr.txt");

const CAPTURED_ID: &str = "session_4d58d263-d61a-48ac-88fe-62125c2b86b8";
const SYNTHETIC_ID: &str = "session_11111111-2222-4333-8444-555555555555";

fn argument<'a>(record: &'a Value, flag: &str) -> Option<&'a str> {
    record["args"]
        .as_array()
        .unwrap_or_else(|| panic!("missing args array in invocation record: {record}"))
        .windows(2)
        .find(|pair| pair[0] == flag)
        .and_then(|pair| pair[1].as_str())
}

fn has_flag(record: &Value, flag: &str) -> bool {
    record["args"]
        .as_array()
        .unwrap_or_else(|| panic!("missing args array in invocation record: {record}"))
        .iter()
        .any(|arg| arg == flag)
}

fn delegated(fleet: &Fleet) -> Vec<Value> {
    fleet
        .records()
        .into_iter()
        .filter(|record| record["cli"] == "kimi" && argument(record, "-p").is_some())
        .collect()
}

fn sessions_root(base: &Path) -> PathBuf {
    #[cfg(all(unix, not(target_os = "macos")))]
    return base.join(".local/state/proxenos/sessions");
    #[cfg(target_os = "macos")]
    return base.join("Library/Application Support/proxenos/sessions");
    #[cfg(windows)]
    return base.join("proxenos/sessions");
}

fn kimi_turn(id: &str, content: &str) -> Value {
    let stdout = format!(
        "{}\n{}\n{}\n",
        json!({"role": "meta", "type": "system.version", "version": "2.8.0"}),
        json!({"role": "assistant", "content": content}),
        json!({"role": "meta", "type": "session.resume_hint", "session_id": id}),
    );
    json!({"stdout": stdout})
}

fn listed_provider_tools(server: &mut Server) -> Vec<Value> {
    server
        .request(12, "tools/list", json!({}))
        .get("result")
        .and_then(|result| result["tools"].as_array())
        .unwrap()
        .iter()
        .filter(|tool| {
            let name = tool["name"].as_str().unwrap();
            !name.starts_with("test_") && !matches!(name, "await" | "cancel")
        })
        .cloned()
        .collect()
}

#[test]
fn kimi_schema_self_exclusion_and_discovery() {
    let fleet = Fleet::new(&["kimi"]);
    let mut claude = fleet.server("claude");
    let tools = listed_provider_tools(&mut claude);
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["kimi", "kimi-reply"]);

    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let props = &tool["inputSchema"]["properties"];
        assert_eq!(props["thread_id"].is_object(), name == "kimi-reply");
        assert!(props.get("effort").is_none());
        assert_eq!(props["access"]["enum"], json!(["full"]));
        let desc = props["access"]["description"].as_str().unwrap();
        assert!(desc.contains("no permission tier"), "{desc}");
        assert!(desc.contains("read-only is unsupported"), "{desc}");
        assert!(desc.contains("AGENTS.md"), "{desc}");
        assert_eq!(props["wait"]["type"], "boolean");
        assert_eq!(props["memory_limit_bytes"]["type"], "integer");
    }

    // Kimi as a client never lists or invokes its own provider tools.
    let mut self_client = fleet.server("kimi");
    assert!(listed_provider_tools(&mut self_client).is_empty());
    let denied = self_client.call(13, "kimi", json!({"prompt": "self"}));
    assert_eq!(denied["error"]["code"], -32602);

    // Absent CLI reports not_found.
    let empty_fleet = Fleet::new(&[]);
    let mut absent_server = empty_fleet.server("claude");
    assert!(listed_provider_tools(&mut absent_server).is_empty());
    let missing = call(&mut absent_server, "kimi", json!({"prompt": "missing"}));
    assert_observed_failure(&missing, "not_found", "Could not find kimi");

    // Fallback install directory (~/.kimi-code/bin on Unix, %USERPROFILE%\.kimi-code\bin on Windows)
    // is discovered when kimi is absent from PATH.
    let fallback_fleet = Fleet::new(&[]);
    let fallback = fallback_fleet
        .root
        .join(".kimi-code/bin")
        .join(format!("kimi{}", std::env::consts::EXE_SUFFIX));
    fs::create_dir_all(fallback.parent().unwrap()).unwrap();
    fs::hard_link(crate::provider_support::stub(), &fallback).unwrap();
    let mut fallback_server = fallback_fleet.server("claude");
    let discovered: Vec<_> = listed_provider_tools(&mut fallback_server)
        .into_iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(discovered, ["kimi", "kimi-reply"]);
}

#[test]
fn kimi_start_and_reply_replay_captured_fixtures_and_enforce_argv_rules() {
    let fleet = Fleet::new(&["kimi"]);
    let cwd = fleet.root.join("chosen workspace");
    fs::create_dir(&cwd).unwrap();
    fleet.script(
        "kimi",
        json!([
            {"stdout": CAPTURED_START},
            {"stdout": CAPTURED_REPLY},
            {"stdout": CAPTURED_REPLY}
        ]),
    );
    let mut cmd = fleet.command("claude");
    cmd.env("IRIS_EXECUTION", "caller-iris")
        .env("CLAUDECODE", "1")
        .env("PROXENOS_STUB_MARKER", "keep");
    let mut server = Server::start_command(cmd);
    server.initialize("2025-11-25");

    let start = call(
        &mut server,
        "kimi",
        json!({
            "prompt": "Reply with the single word OK.",
            "mode": "advisory",
            "instructions": "Keep it brief.",
            "model": "kimi-code/k3",
            "cwd": cwd,
            "access": "full",
            "timeout_ms": 5_000,
            "coordination": {"caller_agent_name": "coord-caller"}
        }),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert_eq!(start["observed"]["provider"], "kimi");
    assert_eq!(start["observed"]["passed"]["model"], "kimi-code/k3");
    assert!(
        start["observed"]["access_mechanism"]
            .as_str()
            .unwrap()
            .contains("AGENTS.md"),
        "{start}"
    );
    assert_eq!(start["reported"]["thread_id"], CAPTURED_ID);
    assert_eq!(start["reported"]["content"], "OK");
    assert!(start["reported"]["model"].is_null());
    assert!(start["reported"]["usage"].is_null());
    assert!(
        start["not_claimed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().is_some_and(|s| s.contains("AGENTS.md"))),
        "missing AGENTS.md caveat in not_claimed: {start}"
    );
    fleet.assert_steps_remaining("kimi", 2);

    // Reply inherits model and cwd (not timeout_ms) and allows changing mode.
    let reply = call(
        &mut server,
        "kimi-reply",
        json!({
            "thread_id": CAPTURED_ID,
            "mode": "implementation",
            "prompt": "Apply the fix."
        }),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["observed"]["passed"]["model"], "kimi-code/k3");
    assert_eq!(reply["observed"]["passed"]["mode"], "implementation");
    assert_eq!(reply["reported"]["thread_id"], CAPTURED_ID);
    assert_eq!(reply["reported"]["content"], "OK");
    assert!(
        reply["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap()
            > 5_000,
        "reply inherited the start timeout: {reply}"
    );
    fleet.assert_steps_remaining("kimi", 1);

    // Reply can explicitly override model.
    let override_reply = call(
        &mut server,
        "kimi-reply",
        json!({
            "thread_id": CAPTURED_ID,
            "model": "kimi-code/k3.1-pro",
            "prompt": "Recheck."
        }),
    );
    assert_eq!(
        override_reply["observed"]["state"], "finished",
        "{override_reply}"
    );
    assert_eq!(
        override_reply["observed"]["passed"]["model"],
        "kimi-code/k3.1-pro"
    );
    fleet.assert_steps_remaining("kimi", 0);

    let records = delegated(&fleet);
    assert_eq!(records.len(), 3);
    assert!(argument(&records[0], "-S").is_none());
    assert_eq!(argument(&records[1], "-S"), Some(CAPTURED_ID));
    assert_eq!(argument(&records[2], "-S"), Some(CAPTURED_ID));
    assert_eq!(argument(&records[0], "-m"), Some("kimi-code/k3"));
    assert_eq!(argument(&records[1], "-m"), Some("kimi-code/k3"));
    assert_eq!(argument(&records[2], "-m"), Some("kimi-code/k3.1-pro"));

    let start_prompt = argument(&records[0], "-p").unwrap();
    assert!(start_prompt.contains("Advise the caller"), "{start_prompt}");
    assert!(start_prompt.contains("Keep it brief."), "{start_prompt}");
    assert!(
        start_prompt.contains("Reply with the single word OK."),
        "{start_prompt}"
    );
    assert!(start_prompt.contains("coord-caller"), "{start_prompt}");

    let reply_prompt = argument(&records[1], "-p").unwrap();
    assert!(
        reply_prompt.contains("Carry out the requested changes"),
        "{reply_prompt}"
    );
    assert!(reply_prompt.contains("Keep it brief."), "{reply_prompt}");
    assert!(reply_prompt.ends_with("Apply the fix."), "{reply_prompt}");

    for record in &records {
        assert_eq!(argument(record, "--output-format"), Some("stream-json"));
        for forbidden in ["--continue", "-c", "--plan", "--yolo", "-y", "--auto"] {
            assert!(
                !has_flag(record, forbidden),
                "forbidden flag {forbidden} passed to kimi: {record}"
            );
        }
        assert_eq!(
            fs::canonicalize(record["cwd"].as_str().unwrap()).unwrap(),
            fs::canonicalize(&cwd).unwrap()
        );
        assert_eq!(record["env"]["PROXENOS_DEPTH"], "1");
        assert_eq!(record["env"]["CLAUDE_DELEGATOR_KIMI_DEPTH"], "1");
        assert_eq!(record["env"]["PROXENOS_STUB_MARKER"], "keep");
        assert!(record["env"]["IRIS_EXECUTION"].is_null());
        assert!(record["env"]["CLAUDECODE"].is_null());
    }
}

#[test]
fn kimi_refuses_read_only_bad_models_invalid_ids_and_conflicting_replies() {
    let fleet = Fleet::new(&["kimi"]);
    let long_model = "m".repeat(257);
    let oversized_id = "s".repeat(200);
    fleet.script(
        "kimi",
        json!([
            {"stdout": CAPTURED_START},
            {"stdout": CAPTURED_START}
        ]),
    );
    let mut server = fleet.server("claude");

    // Refuses access: read-only on both start and reply before discovery or launch.
    let ro_start = call(
        &mut server,
        "kimi",
        json!({"prompt": "Review this.", "access": "read-only"}),
    );
    assert_observed_failure(&ro_start, "invalid_request", "read-only");
    assert!(ro_start["observed"]["passed"].is_null());
    assert!(
        ro_start["not_claimed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().is_some_and(|s| s.contains("AGENTS.md"))),
        "{ro_start}"
    );

    let ro_reply = call(
        &mut server,
        "kimi-reply",
        json!({
            "thread_id": CAPTURED_ID,
            "prompt": "Continue.",
            "cwd": fleet.root,
            "mode": "advisory",
            "access": "read-only"
        }),
    );
    assert_observed_failure(&ro_reply, "invalid_request", "read-only");

    // Other invalid parameters are refused before discovery.
    for (tool, args, detail) in [
        (
            "kimi",
            json!({"prompt": "go", "access": "sandbox"}),
            "Unsupported parameter or value for access",
        ),
        ("kimi", json!({"prompt": "go", "effort": "high"}), "effort"),
        ("kimi", json!({"prompt": "go", "model": ""}), "model"),
        (
            "kimi",
            json!({"prompt": "go", "model": "  --flag"}),
            "model",
        ),
        ("kimi", json!({"prompt": "go", "model": "-x"}), "model"),
        (
            "kimi",
            json!({"prompt": "go", "model": long_model}),
            "model",
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": "--continue",
                "prompt": "go",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "Kimi session identifier",
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": "../escape",
                "prompt": "go",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "Kimi session identifier",
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": "a/b",
                "prompt": "go",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "Kimi session identifier",
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": oversized_id,
                "prompt": "go",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "Kimi session identifier",
        ),
        (
            "kimi-reply",
            json!({"thread_id": CAPTURED_ID, "prompt": "unremembered"}),
            "no settings",
        ),
    ] {
        let rejected = call(&mut server, tool, args);
        assert_observed_failure(&rejected, "invalid_request", detail);
    }
    assert!(
        fleet.records().is_empty(),
        "rejected requests must never reach discovery or dispatch"
    );
    fleet.assert_steps_remaining("kimi", 2);

    // Conflicting starts for the same session ID mark it ambiguous in memory and on disk.
    let s1 = call(
        &mut server,
        "kimi",
        json!({"prompt": "first", "model": "kimi-code/k3"}),
    );
    assert_eq!(s1["observed"]["state"], "finished", "{s1}");
    let s2 = call(
        &mut server,
        "kimi",
        json!({"prompt": "second", "model": "kimi-code/k3.1-pro"}),
    );
    assert_eq!(s2["observed"]["state"], "finished", "{s2}");
    fleet.assert_steps_remaining("kimi", 0);

    for reply_args in [
        json!({"thread_id": CAPTURED_ID, "prompt": "ambiguous bare"}),
        json!({
            "thread_id": CAPTURED_ID,
            "prompt": "ambiguous explicit",
            "cwd": fleet.root,
            "access": "full",
            "mode": "advisory",
            "model": "kimi-code/k3"
        }),
    ] {
        let refused = call(&mut server, "kimi-reply", reply_args);
        assert_observed_failure(&refused, "invalid_request", "conflicting");
    }
    assert!(server.finish().0.success());

    // Ambiguity survives server restart via the persisted session file.
    let mut restarted = fleet.server("claude");
    let refused_after_restart = call(
        &mut restarted,
        "kimi-reply",
        json!({"thread_id": CAPTURED_ID, "prompt": "after restart"}),
    );
    assert_observed_failure(&refused_after_restart, "invalid_request", "conflicting");
    assert_eq!(delegated(&fleet).len(), 2);
}

#[test]
fn kimi_reply_after_server_restart_uses_session_file_and_supports_model_less_replies() {
    let fleet = Fleet::new(&["kimi"]);
    let cwd_with_model = fleet.root.join("workspace-model");
    let cwd_without_model = fleet.root.join("workspace-default");
    fs::create_dir(&cwd_with_model).unwrap();
    fs::create_dir(&cwd_without_model).unwrap();

    fleet.script(
        "kimi",
        json!([
            {"stdout": CAPTURED_START},
            kimi_turn(SYNTHETIC_ID, "default model start"),
            {"stdout": CAPTURED_REPLY},
            kimi_turn(SYNTHETIC_ID, "default model reply")
        ]),
    );

    // Server 1: start one session with an explicit model and one without any model.
    let mut cmd = fleet.command("claude");
    cmd.env("PROXENOS_STUB_MARKER", "SECRET_ENV_VALUE");
    let mut server1 = Server::start_command(cmd);
    server1.initialize("2025-11-25");

    let s1 = call(
        &mut server1,
        "kimi",
        json!({
            "prompt": "SECRET_PROMPT_TEXT",
            "instructions": "SECRET_INSTRUCTIONS_TEXT",
            "model": "kimi-code/k3",
            "cwd": cwd_with_model,
            "mode": "advisory",
            "timeout_ms": 5_000
        }),
    );
    assert_eq!(s1["observed"]["state"], "finished", "{s1}");
    assert_eq!(s1["reported"]["thread_id"], CAPTURED_ID);

    let s2 = call(
        &mut server1,
        "kimi",
        json!({
            "prompt": "start with kimi default model",
            "cwd": cwd_without_model
        }),
    );
    assert_eq!(s2["observed"]["state"], "finished", "{s2}");
    assert_eq!(s2["reported"]["thread_id"], SYNTHETIC_ID);
    assert!(s2["observed"]["passed"]["model"].is_null());
    assert!(server1.finish().0.success());

    // Confirm persisted session files exist and store no secrets or prompt text.
    let provider_dir = sessions_root(&fleet.root).join("kimi");
    let session_files: Vec<PathBuf> = fs::read_dir(&provider_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(session_files.len(), 2, "{session_files:?}");
    for file in &session_files {
        let raw = fs::read_to_string(file).unwrap();
        for forbidden in [
            "SECRET_PROMPT_TEXT",
            "SECRET_INSTRUCTIONS_TEXT",
            "SECRET_ENV_VALUE",
            "timeout_ms",
        ] {
            assert!(!raw.contains(forbidden), "leaked {forbidden}: {raw}");
        }
    }

    // Server 2 (after restart): both replies load settings from disk.
    let mut server2 = fleet.server("claude");
    let r1 = call(
        &mut server2,
        "kimi-reply",
        json!({"thread_id": CAPTURED_ID, "prompt": "follow up with model"}),
    );
    assert_eq!(r1["observed"]["state"], "finished", "{r1}");
    assert_eq!(r1["observed"]["passed"]["model"], "kimi-code/k3");
    assert_eq!(r1["reported"]["content"], "OK");
    assert!(
        r1["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap()
            > 5_000,
        "reply inherited timeout_ms from persisted file: {r1}"
    );

    // D19 model-less reply: start named no model and Kimi does not require -m on reply,
    // so reply omits -m while repeating the start's directory from the session file.
    let r2 = call(
        &mut server2,
        "kimi-reply",
        json!({"thread_id": SYNTHETIC_ID, "prompt": "follow up without model"}),
    );
    assert_eq!(r2["observed"]["state"], "finished", "{r2}");
    assert!(r2["observed"]["passed"]["model"].is_null());
    assert_eq!(r2["reported"]["content"], "default model reply");
    fleet.assert_steps_remaining("kimi", 0);

    let records = delegated(&fleet);
    assert_eq!(records.len(), 4);
    assert_eq!(argument(&records[2], "-S"), Some(CAPTURED_ID));
    assert_eq!(argument(&records[2], "-m"), Some("kimi-code/k3"));
    assert_eq!(
        fs::canonicalize(records[2]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&cwd_with_model).unwrap()
    );
    assert_eq!(argument(&records[3], "-S"), Some(SYNTHETIC_ID));
    assert!(argument(&records[3], "-m").is_none());
    assert_eq!(
        fs::canonicalize(records[3]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&cwd_without_model).unwrap()
    );
}

#[test]
fn kimi_streams_chunks_requires_completion_and_confirms_session_id() {
    let fleet = Fleet::new(&["kimi"]);
    let foreign_long = kimi_turn("session_other", &"f".repeat((16 << 10) + 64));
    let missing_id_hint = json!({
        "stdout": format!(
            "{}\n{}\n{}\n",
            json!({"role": "meta", "type": "system.version", "version": "2.8.0"}),
            json!({"role": "assistant", "content": "unconfirmed reply text"}),
            json!({"role": "meta", "type": "session.resume_hint"})
        )
    });
    let invalid_id_start = kimi_turn("../escape", "valid answer with bad id");
    let oversized_id_start = kimi_turn(&"s".repeat(200), "valid answer with long id");

    fleet.script(
        "kimi",
        json!([
            {"stdout": SYNTHETIC_MULTI_CHUNK},
            {"stdout": SYNTHETIC_NO_COMPLETION},
            {"stdout": SYNTHETIC_NO_ASSISTANT},
            {"stdout": SYNTHETIC_NO_ASSISTANT, "stderr": "401 Unauthorized"},
            foreign_long,
            missing_id_hint,
            invalid_id_start,
            oversized_id_start
        ]),
    );
    let mut server = fleet.server("claude");

    // Multiple assistant chunks past non-JSON noise concatenate cleanly.
    let multi = call(&mut server, "kimi", json!({"prompt": "multi"}));
    assert_eq!(multi["observed"]["state"], "finished", "{multi}");
    assert_eq!(multi["reported"]["thread_id"], SYNTHETIC_ID);
    assert_eq!(multi["reported"]["content"], "First part and second part.");
    fleet.assert_steps_remaining("kimi", 7);

    // Zero exit without session.resume_hint fails as missing completion evidence.
    let incomplete = call(&mut server, "kimi", json!({"prompt": "truncated"}));
    assert_observed_failure(
        &incomplete,
        "unknown",
        "without evidence of a completed turn",
    );
    assert!(incomplete["reported"]["content"].is_null());
    fleet.assert_steps_remaining("kimi", 6);

    // session.resume_hint with no assistant content fails cleanly (unclassified, then classified).
    let no_answer = call(&mut server, "kimi", json!({"prompt": "silent"}));
    assert_reported_failure(&no_answer, None, "unknown", "without an assistant answer");
    fleet.assert_steps_remaining("kimi", 5);

    let no_answer_auth = call(&mut server, "kimi", json!({"prompt": "silent auth"}));
    assert_reported_failure(&no_answer_auth, None, "authentication", "401 Unauthorized");
    fleet.assert_steps_remaining("kimi", 4);

    // Reply receiving a foreign session ID or missing session ID fails and never leaks or archives content.
    for (index, expected_reported_id) in [Some("session_other"), None].into_iter().enumerate() {
        let mismatch = call(
            &mut server,
            "kimi-reply",
            json!({"thread_id": SYNTHETIC_ID, "prompt": "check id"}),
        );
        assert_reported_failure(
            &mismatch,
            None,
            "unknown",
            "did not confirm the requested session ID",
        );
        assert_eq!(
            mismatch["reported"]["thread_id"].as_str(),
            expected_reported_id,
            "{mismatch}"
        );
        assert!(
            mismatch["reported"]["content"].is_null(),
            "foreign content leaked: {mismatch}"
        );
        assert!(
            mismatch["observed"]["content_file"].is_null(),
            "foreign content archived: {mismatch}"
        );
        fleet.assert_steps_remaining("kimi", 3 - index);
    }

    // Invalid or oversized session IDs on start are dropped rather than returned or persisted.
    for index in 0..2 {
        let done = call(&mut server, "kimi", json!({"prompt": "bad id start"}));
        assert_eq!(done["observed"]["state"], "finished", "{done}");
        assert!(done["reported"]["thread_id"].is_null(), "{done}");
        fleet.assert_steps_remaining("kimi", 1 - index);
    }
    assert_eq!(delegated(&fleet).len(), 8);
}

#[test]
fn kimi_classifies_captured_and_synthetic_errors_without_misclassifying_answers() {
    let fleet = Fleet::new(&["kimi"]);
    let answer_with_error_text = kimi_turn(
        CAPTURED_ID,
        "Model \"x\" is not configured in config.toml. Quota exhausted.",
    );
    fleet.script(
        "kimi",
        json!([
            {
                "stdout": CAPTURED_UNKNOWN_MODEL_OUT,
                "stderr": CAPTURED_UNKNOWN_MODEL_ERR,
                "code": 1
            },
            {
                "stdout": CAPTURED_SESSION_NOT_FOUND_OUT,
                "stderr": CAPTURED_SESSION_NOT_FOUND_ERR,
                "code": 1
            },
            {
                "stdout": CAPTURED_MISSING_LOGIN_OUT,
                "stderr": CAPTURED_MISSING_LOGIN_ERR,
                "code": 1
            },
            {
                "stderr": CAPTURED_PLAN_REJECTED_ERR,
                "code": 1
            },
            {
                "stdout": CAPTURED_SESSION_NOT_FOUND_OUT,
                "stderr": SYNTHETIC_WRONG_DIRECTORY_ERR,
                "code": 1
            },
            {
                "stderr": "401 Unauthorized",
                "code": 1
            },
            {
                "stderr": "error: failed to run prompt: Quota exhausted. Resets at 2030-05-01T12:00:00Z.\n",
                "code": 1
            },
            {
                "stderr": "error: insufficient_quota\n",
                "code": 1
            },
            {
                "stderr": "error: failed to run prompt: content_policy_violation\n",
                "code": 1
            },
            {
                "stderr": "worker died",
                "code": 37
            },
            {
                "code": 2
            },
            answer_with_error_text
        ]),
    );
    let mut server = fleet.server("claude");

    let classified_cases: &[(&str, Value, &str, &str, Option<&str>)] = &[
        (
            "kimi",
            json!({"prompt": "unknown model", "model": "nonexistent-model-probe"}),
            "invalid_request",
            "is not configured in config.toml",
            None,
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": "bogus-session-id-0000",
                "prompt": "missing session",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "invalid_request",
            "not found",
            None,
        ),
        (
            "kimi",
            json!({"prompt": "missing login"}),
            "authentication",
            "/login to sign in",
            None,
        ),
        (
            "kimi",
            json!({"prompt": "plan rejected"}),
            "invalid_request",
            "Cannot combine --prompt with --plan",
            None,
        ),
        (
            "kimi-reply",
            json!({
                "thread_id": SYNTHETIC_ID,
                "prompt": "wrong dir",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
            "invalid_request",
            "different directory",
            None,
        ),
        (
            "kimi",
            json!({"prompt": "raw auth"}),
            "authentication",
            "401 Unauthorized",
            None,
        ),
        (
            "kimi",
            json!({"prompt": "quota with reset"}),
            "quota",
            "Quota exhausted",
            Some("2030-05-01T12:00:00Z"),
        ),
        (
            "kimi",
            json!({"prompt": "quota no reset"}),
            "quota",
            "insufficient_quota",
            None,
        ),
        (
            "kimi",
            json!({"prompt": "refusal"}),
            "refusal",
            "content_policy_violation",
            None,
        ),
    ];

    for (index, (tool, args, kind, detail, reset_at)) in classified_cases.iter().enumerate() {
        let result = call(&mut server, tool, args.clone());
        assert_reported_failure(
            &result,
            Some(("abnormal_exit", "exit code 1")),
            kind,
            detail,
        );
        assert_eq!(
            result["reported"]["failure"]["reset_at"].as_str(),
            *reset_at,
            "{result}"
        );
        fleet.assert_steps_remaining("kimi", 11 - index);
    }

    // Unrecognized stderr stays in reported.diagnostic without a classified reported.failure.
    let unknown_exit = call(&mut server, "kimi", json!({"prompt": "worker died"}));
    assert_observed_failure(&unknown_exit, "abnormal_exit", "exit code 37");
    assert_eq!(unknown_exit["reported"]["diagnostic"], "worker died");
    fleet.assert_steps_remaining("kimi", 2);

    // Non-zero exit with empty stderr reports abnormal_exit cleanly.
    let silent_exit = call(&mut server, "kimi", json!({"prompt": "silent fail"}));
    assert_observed_failure(&silent_exit, "abnormal_exit", "exit code 2");
    assert!(silent_exit["reported"]["diagnostic"].is_null());
    fleet.assert_steps_remaining("kimi", 1);

    // Error-like strings in assistant output of a completed turn never become failures.
    let ok = call(&mut server, "kimi", json!({"prompt": "quote error"}));
    assert_eq!(ok["observed"]["state"], "finished", "{ok}");
    assert_eq!(
        ok["reported"]["content"],
        "Model \"x\" is not configured in config.toml. Quota exhausted."
    );
    assert!(ok["reported"]["failure"].is_null());
    fleet.assert_steps_remaining("kimi", 0);
}

#[test]
fn kimi_reports_timeout_cancellation_and_capture_failures() {
    let fleet = Fleet::new(&["kimi"]);
    let ready_timeout = fleet.root.join("kimi-timeout.ready");
    let ready_cancel = fleet.root.join("kimi-cancel.ready");
    let flooded = format!(
        "{}\n{}\n",
        json!({"role": "meta", "type": "system.version", "version": "2.8.0"}),
        "x".repeat((4 << 20) + 128)
    );
    let long_answer = "k".repeat((16 << 10) + 64);

    fleet.script(
        "kimi",
        json!([
            {"stdout": flooded},
            kimi_turn(CAPTURED_ID, &long_answer),
            {"stdout": "{\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"2.8.0\"}\n", "ready": ready_timeout, "sleep_ms": 60_000},
            {"stderr": "working", "ready": ready_cancel, "sleep_ms": 60_000}
        ]),
    );

    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));

    // Exceeding the 4 MiB stdout capture limit before session.resume_hint yields capture failure.
    let capture = call(&mut server, "kimi", json!({"prompt": "flood"}));
    assert_observed_failure(&capture, "capture", "beyond the capture limit");
    fleet.assert_steps_remaining("kimi", 3);

    // Long valid answer (> 16 KiB) is excerpted and archived cleanly.
    let archived = call(&mut server, "kimi", json!({"prompt": "long answer"}));
    assert_eq!(archived["observed"]["state"], "finished", "{archived}");
    assert_eq!(
        archived["reported"]["content"].as_str().unwrap().len(),
        16 << 10
    );
    let content_file = PathBuf::from(archived["observed"]["content_file"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&content_file).unwrap(), long_answer);
    fleet.assert_steps_remaining("kimi", 2);

    // Timeout stops the hung kimi child and reports observed.failure.kind == "timeout".
    server.start_call(7, "kimi", json!({"prompt": "hang", "timeout_ms": 1500}));
    let until = Instant::now() + WAIT;
    while !ready_timeout.exists() {
        assert!(Instant::now() < until, "timed kimi stub did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let timed: Value = serde_json::from_str(text(&server.response(7))).unwrap();
    assert_observed_failure(&timed, "timeout", "exceeded the call's deadline");
    fleet.assert_steps_remaining("kimi", 1);

    // Closing MCP input cancels the running kimi child cleanly.
    server.start_call(8, "kimi", json!({"prompt": "cancel me"}));
    let until = Instant::now() + WAIT;
    while !ready_cancel.exists() {
        assert!(Instant::now() < until, "cancelled kimi stub did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    server.close_input();
    let cancelled: Value = serde_json::from_str(text(&server.response(8))).unwrap();
    assert_observed_failure(&cancelled, "cancellation", "was cancelled");
    fleet.assert_steps_remaining("kimi", 0);
}
