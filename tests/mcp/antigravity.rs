use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::common::Server;
use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, assert_observed_failure,
    assert_reported_failure, call,
};

const FINISHED: &str = include_str!("../fixtures/antigravity/finished.json");
const PRINT_TIMEOUT: &str = include_str!("../fixtures/antigravity/timeout.json");
const CAPTURED_AUTH_FAILED: &str =
    include_str!("../fixtures/antigravity/captured-auth-failed.stdout.json");
const ID: &str = "paper-crane";
const SIGN_IN_PROMPT: &str = "Authentication required. Please visit the URL to log in:";

fn failed_json(error: &str) -> String {
    json!({"status": "ERROR", "response": "", "error": error}).to_string()
}

#[track_caller]
fn assert_sign_in_failure(result: &Value) {
    assert_reported_failure(
        result,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert_eq!(
        result["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC,
        "{result}"
    );
    assert_eq!(result["observed"]["redacted"], true, "{result}");
}

fn store(fleet: &Fleet) -> PathBuf {
    fleet.root.join(".gemini/antigravity-cli/conversations")
}

fn argument<'a>(record: &'a Value, flag: &str) -> Option<&'a str> {
    record["args"]
        .as_array()
        .unwrap_or_else(|| panic!("missing args array in invocation record: {record}"))
        .windows(2)
        .find(|pair| pair[0] == flag)
        .and_then(|pair| pair[1].as_str())
}

fn delegated(fleet: &Fleet) -> Vec<Value> {
    fleet
        .records()
        .into_iter()
        .filter(|record| argument(record, "--print").is_some())
        .collect()
}

#[test]
fn start_and_reply_pass_the_model_workspace_permissions_and_new_guidance() {
    let fleet = Fleet::new(&["agy"]);
    let directory = store(&fleet);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(format!("{ID}.db")), []).unwrap();
    fleet.script(
        "agy",
        json!([
            {"stdout": FINISHED},
            {"stdout": FINISHED},
            {"stdout": FINISHED}
        ]),
    );
    let mut server = fleet.server("claude");
    let list = server.request(2, "tools/list", json!({}));
    let start_schema = &list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "antigravity")
        .unwrap()["inputSchema"];
    let access = start_schema["properties"]["access"]["description"]
        .as_str()
        .unwrap();
    assert!(access.contains("still write files"), "{access}");
    assert!(start_schema["properties"].get("effort").is_none());
    let unsupported = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Use a tier.", "effort":"high"}),
    );
    assert_observed_failure(&unsupported, "invalid_request", "effort");
    fleet.assert_steps_remaining("agy", 3);
    let start = call(
        &mut server,
        "antigravity",
        json!({
            "prompt": "/settings\nmeasure the fold", "mode": "advisory", "instructions": "Use the marked edge.",
            "cwd": fleet.root, "model": "fixture-model", "access": "read-only", "timeout_ms": 90_000
        }),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert_eq!(
        start["reported"]["content"],
        "The fold aligns with the mark."
    );
    assert_eq!(start["reported"]["thread_id"], ID);
    assert!(start["reported"]["model"].is_null());
    fleet.assert_steps_remaining("agy", 2);
    let reply = call(
        &mut server,
        "antigravity-reply",
        json!({
            "thread_id": ID, "mode": "implementation", "prompt": "Move the mark."
        }),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["observed"]["passed"]["model"], "fixture-model");
    fleet.assert_steps_remaining("agy", 1);
    let override_reply = call(
        &mut server,
        "antigravity-reply",
        json!({
            "thread_id": ID, "model": "another-model", "access": "full", "prompt": "Recheck."
        }),
    );
    assert_eq!(
        override_reply["observed"]["state"], "finished",
        "{override_reply}"
    );
    fleet.assert_steps_remaining("agy", 0);
    let records = delegated(&fleet);
    assert_eq!(records.len(), 3);
    assert!(
        !records[0]["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--conversation"))
    );
    assert_eq!(argument(&records[0], "--print-timeout"), Some("60000ms"));
    assert_eq!(argument(&records[1], "--print-timeout"), Some("3570000ms"));
    assert!(
        reply["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("missing deadline_ms in envelope: {reply}"))
            > 90_000,
        "reply inherited the start timeout: {reply}"
    );
    assert!(
        argument(&records[0], "--print")
            .unwrap()
            .ends_with("/settings\nmeasure the fold")
    );
    assert!(
        argument(&records[0], "--print")
            .unwrap()
            .contains("Use the marked edge.")
    );
    for record in &records[..2] {
        assert_eq!(argument(record, "--model"), Some("fixture-model"));
        assert!(
            !record["args"]
                .as_array()
                .unwrap()
                .contains(&json!("--dangerously-skip-permissions"))
        );
    }
    for record in &records {
        assert_eq!(
            fs::canonicalize(record["cwd"].as_str().unwrap()).unwrap(),
            fs::canonicalize(&fleet.root).unwrap()
        );
        assert!(
            record["args"]
                .as_array()
                .unwrap()
                .contains(&json!("--disable-slash-commands"))
        );
        assert_eq!(argument(record, "--output-format"), Some("json"));
        assert!(Path::new(argument(record, "--log-file").unwrap()).is_absolute());
        assert_eq!(record["env"]["CLAUDE_DELEGATOR_AGY_DEPTH"], "1");
        assert_eq!(record["env"]["PROXENOS_DEPTH"], "1");
        assert!(record["env"]["IRIS_EXECUTION"].is_null());
        assert!(
            !record["args"]
                .as_array()
                .unwrap()
                .contains(&json!("--add-dir"))
        );
    }
    assert_ne!(
        argument(&records[0], "--log-file"),
        argument(&records[1], "--log-file")
    );
    assert_eq!(argument(&records[1], "--conversation"), Some(ID));
    assert!(
        argument(&records[1], "--print")
            .unwrap()
            .contains("Use the marked edge.")
    );
    assert!(
        argument(&records[1], "--print")
            .unwrap()
            .ends_with("Move the mark.")
    );
    assert_eq!(argument(&records[2], "--model"), Some("another-model"));
    assert!(
        records[2]["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--dangerously-skip-permissions"))
    );
}

#[test]
fn a_model_free_start_requires_the_caller_to_name_a_reply_model() {
    let fleet = Fleet::new(&["agy"]);
    fs::create_dir_all(store(&fleet)).unwrap();
    fs::write(store(&fleet).join(format!("{ID}.pb")), []).unwrap();
    fleet.script("agy", json!([{"stdout": FINISHED}, {"stdout": FINISHED}]));
    let mut server = fleet.server("claude");
    let start = call(&mut server, "antigravity", json!({"prompt": "Start."}));
    assert_eq!(start["observed"]["state"], "finished");
    fleet.assert_steps_remaining("agy", 1);
    let reply = json!({"prompt": "Continue.", "thread_id": ID});
    let refused = call(&mut server, "antigravity-reply", reply.clone());
    assert_observed_failure(&refused, "invalid_request", "model");
    fleet.assert_steps_remaining("agy", 1);
    let mut explicit = reply;
    explicit["model"] = json!("user-choice");
    let ok = call(&mut server, "antigravity-reply", explicit);
    assert_eq!(ok["observed"]["state"], "finished");
    fleet.assert_steps_remaining("agy", 0);
    let records = delegated(&fleet);
    assert_eq!(records.len(), 2);
    assert!(argument(&records[0], "--model").is_none());
    assert_eq!(argument(&records[1], "--model"), Some("user-choice"));
}

fn resume(server: &mut Server, id: &str, cwd: &Path) -> Value {
    call(
        server,
        "antigravity-reply",
        json!({
            "thread_id": id, "prompt": "Resume.", "model": "named", "access": "full",
            "mode": "implementation", "cwd": cwd
        }),
    )
}

#[test]
fn unknown_conversations_cannot_reach_any_child_even_on_a_first_call() {
    let fleet = Fleet::new(&["agy"]);
    let mut server = fleet.server("claude");
    let missing = resume(&mut server, ID, &fleet.root);
    assert_observed_failure(&missing, "invalid_request", "Cannot inspect");
    let directory = store(&fleet);
    fs::create_dir_all(directory.join("folder.db")).unwrap();
    fs::write(directory.join("Case-Sensitive.db"), []).unwrap();
    fs::write(directory.join("sidecar.db-wal"), []).unwrap();
    for (id, detail) in [
        ("case-sensitive", "No exact regular conversation file"),
        ("folder", "No exact regular conversation file"),
        ("sidecar", "No exact regular conversation file"),
        ("unknown-id", "No exact regular conversation file"),
        ("a/b", "conversation identifier"),
        ("../escape", "conversation identifier"),
    ] {
        let failed = resume(&mut server, id, &fleet.root);
        assert_observed_failure(&failed, "invalid_request", detail);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            directory.join("Case-Sensitive.db"),
            directory.join("alias.db"),
        )
        .unwrap();
        let failed = resume(&mut server, "alias", &fleet.root);
        assert_observed_failure(
            &failed,
            "invalid_request",
            "No exact regular conversation file",
        );
    }
    assert!(
        fleet.records().is_empty(),
        "a rejected ID must not reach discovery or the provider"
    );
}

#[test]
fn both_directory_overrides_are_honoured_with_the_proxenos_one_first() {
    let fleet = Fleet::new(&["agy"]);
    let legacy_store = fleet.root.join("legacy store");
    let new_store = fleet.root.join("new store");
    fs::create_dir(&legacy_store).unwrap();
    fs::write(legacy_store.join(format!("{ID}.pb")), []).unwrap();
    fs::create_dir(&new_store).unwrap();
    fleet.script("agy", json!([{"stdout": FINISHED}, {"stdout": FINISHED}]));
    for (own, found, remaining) in [
        ("  ".into(), true, 1),
        (new_store.clone(), false, 1),
        (legacy_store.clone(), true, 0),
    ] {
        let mut command = fleet.command("claude");
        command
            .env("CLAUDE_DELEGATOR_AGY_CONVERSATIONS_DIR", &legacy_store)
            .env("PROXENOS_AGY_CONVERSATIONS_DIR", own);
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        let result = resume(&mut server, ID, &fleet.root);
        if found {
            assert_eq!(result["observed"]["state"], "finished", "{result}");
        } else {
            assert_observed_failure(
                &result,
                "invalid_request",
                "No exact regular conversation file",
            );
        }
        fleet.assert_steps_remaining("agy", remaining);
    }
    assert_eq!(delegated(&fleet).len(), 2);
}

#[test]
fn print_status_and_permission_diagnostics_classify_only_the_failed_turn() {
    let fleet = Fleet::new(&["agy"]);
    let mut server = fleet.server("claude");
    let denial = "jetski: no output produced — a tool required the \"command\" permission";
    let quota = "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z.";
    fleet.script("agy", json!([
        {"stdout": PRINT_TIMEOUT, "code":1},
        {"stdout":failed_json(quota), "code":1},
        {"stdout":failed_json("Authentication required"), "code":1},
        {"stdout":failed_json("Invalid model selection"), "code":1},
        {"stdout":failed_json(""), "code":1},
        {"stdout":r#"{"status":"SUCCESS","response":""}"#, "stderr":denial},
        {"stdout":r#"{"status":"SUCCESS","response":"  "}"#},
        {"stdout":"terminal noise {with braces}\n{\"status\":\"SUCCESS\",\"response\":\"quota exceeded\",\"conversation_id\":\"paper-crane\"}\n{}", "stderr":"authentication required"},
        {"stdout":"{\"response\":\"An answer without a status.\"}"},
        {"stderr":"Unauthenticated", "code":1}
    ]));
    for (i, (observed, kind, detail)) in [
        (
            Some(("abnormal_exit", "exit code 1")),
            "timeout",
            "timeout waiting for response",
        ),
        (
            Some(("abnormal_exit", "exit code 1")),
            "quota",
            "Quota exhausted",
        ),
        (
            Some(("abnormal_exit", "exit code 1")),
            "authentication",
            AUTHENTICATION_FAILURE_MESSAGE,
        ),
        (
            Some(("abnormal_exit", "exit code 1")),
            "unknown",
            "Invalid model selection",
        ),
        (
            Some(("abnormal_exit", "exit code 1")),
            "unknown",
            "unsuccessful status",
        ),
        (None, "refusal", "command\" permission"),
        (None, "unknown", "empty response"),
    ]
    .into_iter()
    .enumerate()
    {
        let result = call(
            &mut server,
            "antigravity",
            json!({"prompt":"Check classification."}),
        );
        assert_reported_failure(&result, observed, kind, detail);
        if i == 0 {
            assert_eq!(result["reported"]["thread_id"], ID);
        }
        if i == 1 {
            assert_eq!(
                result["reported"]["failure"]["reset_at"],
                "2030-04-02T15:00:00Z"
            );
        }
        fleet.assert_steps_remaining("agy", 9 - i);
    }
    let success = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Report errors as text."}),
    );
    assert_eq!(success["observed"]["state"], "finished");
    assert_eq!(success["reported"]["content"], "quota exceeded");
    assert!(success["reported"]["failure"].is_null(), "{success}");
    fleet.assert_steps_remaining("agy", 2);
    let no_status = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Missing status."}),
    );
    assert_observed_failure(
        &no_status,
        "unknown",
        "without evidence of a completed turn",
    );
    fleet.assert_steps_remaining("agy", 1);
    let unconfirmed = call(&mut server, "antigravity", json!({"prompt":"Login."}));
    assert_observed_failure(&unconfirmed, "abnormal_exit", "exit code 1");
    assert_eq!(unconfirmed["reported"]["diagnostic"], "Unauthenticated");
    fleet.assert_steps_remaining("agy", 0);
    assert_eq!(delegated(&fleet).len(), 10);
}

#[test]
fn a_reply_checks_the_confirmed_id_as_well_as_the_store() {
    let fleet = Fleet::new(&["agy"]);
    fs::create_dir_all(store(&fleet)).unwrap();
    fs::write(store(&fleet).join("old-thread.db"), []).unwrap();
    let long_other = json!({
        "status": "SUCCESS",
        "conversation_id": ID,
        "response": "s".repeat((16 << 10) + 64)
    })
    .to_string();
    let error_other = json!({
        "status": "ERROR",
        "conversation_id": ID,
        "response": "foreign conversation text",
        "error": "failed"
    })
    .to_string();
    let success_missing_id = json!({
        "status": "SUCCESS",
        "response": "unconfirmed conversation text"
    })
    .to_string();
    let quota_without_id = json!({
        "status": "ERROR",
        "response": "must not leak without id",
        "error": "Quota exhausted. Resets at 2030-04-02T15:00:00Z."
    })
    .to_string();
    fleet.script(
        "agy",
        json!([
            {"stdout": FINISHED},
            {"stdout": long_other},
            {"stdout": error_other},
            {"stdout": success_missing_id},
            {"stdout": quota_without_id, "code": 1}
        ]),
    );
    let mut server = fleet.server("claude");
    for (index, expect_id) in [Some(ID), Some(ID), Some(ID), None].into_iter().enumerate() {
        let result = resume(&mut server, "old-thread", &fleet.root);
        assert_reported_failure(&result, None, "unknown", "conversation ID");
        assert_eq!(
            result["reported"]["thread_id"].as_str(),
            expect_id,
            "{result}"
        );
        assert!(
            result["reported"]["content"].is_null(),
            "mismatched reply leaked foreign content: {result}"
        );
        assert!(
            result["observed"]["content_file"].is_null(),
            "mismatched reply archived foreign content: {result}"
        );
        assert!(
            result["observed"]["content_captured_bytes"].is_null(),
            "{result}"
        );
        fleet.assert_steps_remaining("agy", 4 - index);
    }
    let quota = resume(&mut server, "old-thread", &fleet.root);
    assert_reported_failure(
        &quota,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Quota exhausted",
    );
    assert!(quota["reported"]["thread_id"].is_null(), "{quota}");
    assert!(
        quota["reported"]["content"].is_null(),
        "error reply without id must not populate content: {quota}"
    );
    assert_eq!(
        quota["reported"]["failure"]["reset_at"], "2030-04-02T15:00:00Z",
        "{quota}"
    );
    fleet.assert_steps_remaining("agy", 0);
    assert_eq!(delegated(&fleet).len(), 5);
}

#[test]
fn invalid_or_oversized_antigravity_conversation_ids_are_dropped() {
    let fleet = Fleet::new(&["agy"]);
    let oversized = "a".repeat(200);
    let bad_start = json!({"status": "SUCCESS", "conversation_id": "../escape", "response": "ok"});
    let long_start = json!({"status": "SUCCESS", "conversation_id": oversized, "response": "ok"});
    fleet.script(
        "agy",
        json!([
            {"stdout": bad_start.to_string()},
            {"stdout": long_start.to_string()}
        ]),
    );
    let mut server = fleet.server("claude");
    let rejected = resume(&mut server, &oversized, &fleet.root);
    assert_observed_failure(&rejected, "invalid_request", "conversation identifier");
    assert!(fleet.records().is_empty());
    fleet.assert_steps_remaining("agy", 2);
    for index in 0..2 {
        let start = call(&mut server, "antigravity", json!({"prompt": "start"}));
        assert_eq!(start["observed"]["state"], "finished", "{start}");
        assert!(
            start["reported"]["thread_id"].is_null(),
            "invalid/oversized conversation_id was not dropped: {start}"
        );
        fleet.assert_steps_remaining("agy", 1 - index);
    }
    assert_eq!(delegated(&fleet).len(), 2);
}

#[test]
fn a_conversation_removed_during_discovery_never_receives_the_reply() {
    use std::time::{Duration, Instant};

    let fleet = Fleet::new(&["agy"]);
    let directory = store(&fleet);
    fs::create_dir_all(&directory).unwrap();
    let history = directory.join(format!("{ID}.db"));
    fs::write(&history, []).unwrap();
    let ready = fleet.root.join("probe.ready");
    let release = fleet.root.join("probe.release");
    fs::write(
        fleet.root.join("agy.version"),
        json!({"stdout": "1.2.3", "ready": ready, "wait_for": release}).to_string(),
    )
    .unwrap();
    fleet.script("agy", json!([{"stdout": FINISHED}]));
    let mut server = fleet.server("claude");
    server.start_call(
        41,
        "antigravity-reply",
        json!({
            "thread_id": ID, "prompt": "Resume.", "model": "named", "access": "full",
            "mode": "implementation", "cwd": fleet.root
        }),
    );
    let until = Instant::now() + crate::common::WAIT;
    while !ready.exists() {
        assert!(Instant::now() < until, "the version probe did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::remove_file(history).unwrap();
    fs::write(&release, []).unwrap();
    let answer = server.response(41);
    let result: Value = serde_json::from_str(crate::common::text(&answer)).unwrap();
    assert_observed_failure(
        &result,
        "invalid_request",
        "No exact regular conversation file",
    );
    fleet.assert_steps_remaining("agy", 1);
    let records = fleet.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["args"], json!(["--version"]));
}

#[test]
fn captured_auth_failed_fixture_classifies_as_authentication() {
    let fleet = Fleet::new(&["agy"]);
    let quoted_success = json!({
        "conversation_id": ID,
        "status": "SUCCESS",
        "response": "The log recorded authentication failed or timed out."
    })
    .to_string();
    let empty_success = r#"{"status":"SUCCESS","response":""}"#;
    fleet.script(
        "agy",
        json!([
            {"stdout": CAPTURED_AUTH_FAILED, "code": 1},
            {"stdout": quoted_success},
            {"stdout": empty_success, "stderr": r#"mcp "github": 401 Unauthorized"#},
            {"stdout": empty_success, "stderr": "git push: invalid credentials"}
        ]),
    );
    let mut server = fleet.server("claude");

    let failed = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
    assert_sign_in_failure(&failed);
    assert!(failed["reported"]["thread_id"].is_null(), "{failed}");
    assert!(failed["reported"]["content"].is_null(), "{failed}");
    fleet.assert_steps_remaining("agy", 3);

    let quoted = call(
        &mut server,
        "antigravity",
        json!({"prompt": "Quote the log line."}),
    );
    assert_eq!(quoted["observed"]["state"], "finished", "{quoted}");
    assert!(quoted["observed"]["failure"].is_null(), "{quoted}");
    assert!(quoted["reported"]["failure"].is_null(), "{quoted}");
    assert_eq!(quoted["reported"]["thread_id"], ID, "{quoted}");
    assert_eq!(
        quoted["reported"]["content"], "The log recorded authentication failed or timed out.",
        "{quoted}"
    );
    fleet.assert_steps_remaining("agy", 2);

    for (index, prompt) in ["Check MCP error.", "Check git error."]
        .into_iter()
        .enumerate()
    {
        let result = call(&mut server, "antigravity", json!({"prompt": prompt}));
        assert_reported_failure(&result, None, "unknown", "empty response");
        fleet.assert_steps_remaining("agy", 1 - index);
    }
    assert_eq!(delegated(&fleet).len(), 4);
}

#[test]
fn delayed_auth_failure_with_stderr_and_real_argv_classifies_as_authentication() {
    let fleet = Fleet::new(&["agy"]);
    let oauth_prompt = format!(
        "{SIGN_IN_PROMPT}\n  https://example.invalid/oauth?code=fake\n\nWaiting for authentication (timeout 60s)...\n"
    );
    let oauth_error =
        "Error: authentication timed out.\nerror: authentication failed or timed out\n";
    fleet.script(
        "agy",
        json!([{
            "stderr": oauth_prompt,
            "sleep_ms": 2000,
            "after_sleep_stderr": oauth_error,
            "after_sleep_stdout": CAPTURED_AUTH_FAILED,
            "code": 1
        }]),
    );
    let mut server = fleet.server("claude");
    let delayed = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));

    assert_sign_in_failure(&delayed);
    let attempt = &delayed["observed"]["attempts"][0];
    assert_eq!(attempt["cli_timeout_ms"], 3_570_000, "{delayed}");
    assert_eq!(
        attempt["stdout_bytes"],
        CAPTURED_AUTH_FAILED.len(),
        "{delayed}"
    );
    assert_eq!(
        attempt["stderr_bytes"],
        oauth_prompt.len() + oauth_error.len(),
        "{delayed}"
    );
    assert!(
        attempt["elapsed_ms"].as_u64().unwrap() >= 1500,
        "expected delayed auth failure to take ~2s: {delayed}"
    );
    fleet.assert_steps_remaining("agy", 0);

    let records = delegated(&fleet);
    assert_eq!(records.len(), 1);
    assert_eq!(argument(&records[0], "--output-format"), Some("json"));
    assert_eq!(argument(&records[0], "--print-timeout"), Some("3570000ms"));
    assert!(
        records[0]["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--disable-slash-commands"))
    );
    assert!(Path::new(argument(&records[0], "--log-file").unwrap()).is_absolute());
}

#[test]
fn third_party_stderr_does_not_turn_an_unknown_json_error_into_authentication() {
    let fleet = Fleet::new(&["agy"]);
    let third_party_errors = [
        r#"mcp "github": authentication failed"#.to_owned(),
        r#"mcp "github": rpc error: code = Unauthenticated"#.to_owned(),
        format!(r#"mcp "github": {SIGN_IN_PROMPT}"#),
        format!("  {SIGN_IN_PROMPT}"),
        SIGN_IN_PROMPT.to_ascii_lowercase(),
    ];
    let steps: Vec<_> = third_party_errors
        .iter()
        .map(
            |stderr| json!({"stdout": failed_json("request aborted"), "stderr": stderr, "code": 1}),
        )
        .collect();
    fleet.script("agy", json!(steps));
    let mut server = fleet.server("claude");

    for stderr in third_party_errors {
        let failure = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
        assert_reported_failure(
            &failure,
            Some(("abnormal_exit", "exit code 1")),
            "unknown",
            "request aborted",
        );
        assert_eq!(failure["reported"]["diagnostic"], stderr, "{failure}");
    }
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn authentication_omits_a_wrapped_sign_in_url_from_the_envelope() {
    let fleet = Fleet::new(&["agy"]);
    let stderr = format!(
        "{SIGN_IN_PROMPT}\n  https://example.invalid/oauth?client_id=fake\n&state=wrapped&code_challenge=xyz to continue.\n"
    );
    fleet.script(
        "agy",
        json!([{"stdout": CAPTURED_AUTH_FAILED, "stderr": stderr, "code": 1}]),
    );
    let mut server = fleet.server("claude");
    let failure = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));

    let envelope = failure.to_string();
    for sensitive in ["https://example.invalid", "state=wrapped", "code_challenge"] {
        assert!(
            !envelope.contains(sensitive),
            "sign-in URL leaked: {failure}"
        );
    }
    assert_sign_in_failure(&failure);
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn only_authentication_replaces_stderr_with_fixed_sign_in_diagnostic() {
    let fleet = Fleet::new(&["agy"]);
    let stderr = "Please see https://example.invalid/support\nECONNREFUSED\n";
    fleet.script(
        "agy",
        json!([
            {"stdout": CAPTURED_AUTH_FAILED, "stderr": stderr, "code": 1},
            {"stdout": failed_json("Invalid model selection"), "stderr": stderr, "code": 1}
        ]),
    );
    let mut server = fleet.server("claude");

    let authentication = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
    assert_sign_in_failure(&authentication);
    let other_failure = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
    assert_reported_failure(
        &other_failure,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "Invalid model selection",
    );
    assert_eq!(
        other_failure["reported"]["diagnostic"], stderr,
        "{other_failure}"
    );
    assert_eq!(
        other_failure["observed"]["redacted"], false,
        "{other_failure}"
    );
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn exact_sign_in_line_fills_in_an_unclassified_or_missing_json_error() {
    let fleet = Fleet::new(&["agy"]);
    let stderr = format!(
        "Starting agy\r\n{SIGN_IN_PROMPT}\r\n  https://example.invalid/oauth?code=fake\r\n"
    );
    let quota_stderr = "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z.\n";
    let mixed_stderr = format!("{stderr}{quota_stderr}");
    fleet.script(
        "agy",
        json!([
            {"stdout": failed_json("request aborted"), "stderr": stderr, "code": 1},
            {"stdout": r#"{"status":"ERROR","response":""}"#, "stderr": mixed_stderr, "code": 1},
            {"stdout": failed_json(" \t "), "stderr": mixed_stderr, "code": 1},
            {"stderr": stderr, "code": 1},
            {"stdout": r#"{"status":"SUCCESS","response":""}"#, "stderr": stderr, "code": 1},
            {"stdout": PRINT_TIMEOUT, "stderr": SIGN_IN_PROMPT, "code": 1},
            {"stderr": quota_stderr, "code": 1}
        ]),
    );
    let mut server = fleet.server("claude");

    for _ in 0..5 {
        let failure = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
        assert_sign_in_failure(&failure);
        assert!(!failure.to_string().contains("code=fake"), "{failure}");
    }

    let timeout = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
    assert_reported_failure(
        &timeout,
        Some(("abnormal_exit", "exit code 1")),
        "timeout",
        "timeout waiting for response",
    );
    assert_eq!(
        timeout["reported"]["diagnostic"], SIGN_IN_PROMPT,
        "{timeout}"
    );

    let quota = call(&mut server, "antigravity", json!({"prompt": "Reply OK"}));
    assert_reported_failure(
        &quota,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Quota exhausted",
    );
    assert_eq!(quota["reported"]["diagnostic"], quota_stderr, "{quota}");
    assert_eq!(
        quota["reported"]["failure"]["reset_at"], "2030-04-02T15:00:00Z",
        "{quota}"
    );
    fleet.assert_steps_remaining("agy", 0);
}
