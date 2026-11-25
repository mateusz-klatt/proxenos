use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::common::Server;
use crate::provider_support::{Fleet, call};

const FINISHED: &str = include_str!("../fixtures/antigravity/finished.json");
const PRINT_TIMEOUT: &str = include_str!("../fixtures/antigravity/timeout.json");
const ID: &str = "paper-crane";

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
    assert_eq!(
        unsupported["observed"]["failure"]["kind"],
        "invalid_request"
    );
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
    let reply = call(
        &mut server,
        "antigravity-reply",
        json!({
            "thread_id": ID, "mode": "implementation", "prompt": "Move the mark."
        }),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["observed"]["passed"]["model"], "fixture-model");
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
    let reply = json!({"prompt": "Continue.", "thread_id": ID});
    let refused = call(&mut server, "antigravity-reply", reply.clone());
    assert_eq!(refused["observed"]["state"], "failed");
    assert!(
        refused["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("model")
    );
    let mut explicit = reply;
    explicit["model"] = json!("user-choice");
    let ok = call(&mut server, "antigravity-reply", explicit);
    assert_eq!(ok["observed"]["state"], "finished");
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
    assert!(
        missing["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Cannot inspect"),
        "{missing}"
    );
    let directory = store(&fleet);
    fs::create_dir_all(directory.join("folder.db")).unwrap();
    fs::write(directory.join("Case-Sensitive.db"), []).unwrap();
    fs::write(directory.join("sidecar.db-wal"), []).unwrap();
    for id in [
        "case-sensitive",
        "folder",
        "sidecar",
        "unknown-id",
        "a/b",
        "../escape",
    ] {
        let failed = resume(&mut server, id, &fleet.root);
        assert_eq!(failed["observed"]["state"], "failed", "{id}: {failed}");
        assert!(
            !failed["observed"]["failure"]["message"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            directory.join("Case-Sensitive.db"),
            directory.join("alias.db"),
        )
        .unwrap();
        let failed = resume(&mut server, "alias", &fleet.root);
        assert_eq!(failed["observed"]["state"], "failed");
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
    for (own, found) in [
        ("  ".into(), true),
        (new_store.clone(), false),
        (legacy_store.clone(), true),
    ] {
        let mut command = fleet.command("claude");
        command
            .env("CLAUDE_DELEGATOR_AGY_CONVERSATIONS_DIR", &legacy_store)
            .env("PROXENOS_AGY_CONVERSATIONS_DIR", own);
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        let result = resume(&mut server, ID, &fleet.root);
        assert_eq!(result["observed"]["state"] == "finished", found, "{result}");
    }
    assert_eq!(delegated(&fleet).len(), 2);
}

#[test]
fn print_status_and_permission_diagnostics_classify_only_the_failed_turn() {
    let fleet = Fleet::new(&["agy"]);
    let mut server = fleet.server("claude");
    let failed = |error: &str| json!({"status":"ERROR","response":"","error":error}).to_string();
    let denial = "jetski: no output produced — a tool required the \"command\" permission";
    let quota = "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z.";
    fleet.script("agy", json!([
        {"stdout": PRINT_TIMEOUT, "code":1},
        {"stdout":failed(quota), "code":1},
        {"stdout":failed("Authentication required"), "code":1},
        {"stdout":failed("Invalid model selection"), "code":1},
        {"stdout":failed(""), "code":1},
        {"stdout":r#"{"status":"SUCCESS","response":""}"#, "stderr":denial},
        {"stdout":r#"{"status":"SUCCESS","response":"  "}"#},
        {"stdout":"terminal noise {with braces}\n{\"status\":\"SUCCESS\",\"response\":\"quota exceeded\",\"conversation_id\":\"paper-crane\"}\n{}", "stderr":"authentication required"},
        {"stdout":"{\"response\":\"An answer without a status.\"}"},
        {"stderr":"Unauthenticated", "code":1}
    ]));
    for (i, kind) in [
        "timeout",
        "quota",
        "authentication",
        "unknown",
        "unknown",
        "refusal",
        "unknown",
    ]
    .into_iter()
    .enumerate()
    {
        let result = call(
            &mut server,
            "antigravity",
            json!({"prompt":"Check classification."}),
        );
        assert_eq!(result["observed"]["state"], "failed", "{result}");
        assert_eq!(result["reported"]["failure"]["kind"], kind, "{result}");
        if i == 0 {
            assert_eq!(result["reported"]["thread_id"], ID);
        }
        if i == 1 {
            assert_eq!(
                result["reported"]["failure"]["reset_at"],
                "2030-04-02T15:00:00Z"
            );
        }
    }
    let success = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Report errors as text."}),
    );
    assert_eq!(success["observed"]["state"], "finished");
    assert_eq!(success["reported"]["content"], "quota exceeded");
    let no_status = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Missing status."}),
    );
    assert_eq!(no_status["observed"]["state"], "failed");
    let auth = call(&mut server, "antigravity", json!({"prompt":"Login."}));
    assert_eq!(auth["reported"]["failure"]["kind"], "authentication");
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
    for expect_id in [Some(ID), Some(ID), Some(ID), None] {
        let result = resume(&mut server, "old-thread", &fleet.root);
        assert_eq!(result["observed"]["state"], "failed", "{result}");
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
        assert!(
            result["reported"]["failure"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("missing reported failure message: {result}"))
                .contains("conversation ID"),
            "{result}"
        );
    }
    let quota = resume(&mut server, "old-thread", &fleet.root);
    assert_eq!(quota["observed"]["state"], "failed", "{quota}");
    assert!(quota["reported"]["thread_id"].is_null(), "{quota}");
    assert!(
        quota["reported"]["content"].is_null(),
        "error reply without id must not populate content: {quota}"
    );
    assert_eq!(quota["reported"]["failure"]["kind"], "quota", "{quota}");
    assert_eq!(
        quota["reported"]["failure"]["reset_at"], "2030-04-02T15:00:00Z",
        "{quota}"
    );
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
    assert_eq!(
        rejected["observed"]["failure"]["kind"], "invalid_request",
        "{rejected}"
    );
    assert!(fleet.records().is_empty());
    for _ in 0..2 {
        let start = call(&mut server, "antigravity", json!({"prompt": "start"}));
        assert_eq!(start["observed"]["state"], "finished", "{start}");
        assert!(
            start["reported"]["thread_id"].is_null(),
            "invalid/oversized conversation_id was not dropped: {start}"
        );
    }
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
    assert_eq!(
        result["observed"]["failure"]["kind"], "invalid_request",
        "{result}"
    );
    let records = fleet.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["args"], json!(["--version"]));
}
