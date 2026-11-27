//! Persisted session settings files, D19 no-model branches, and corrupt-file safety (D19).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::Server;
use crate::provider_support::{Fleet, call};

const CODEX_FINISHED: &str = include_str!("../fixtures/codex/finished.jsonl");
const CODEX_THREAD: &str = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";
const AGY_FINISHED: &str = include_str!("../fixtures/antigravity/finished.json");
const AGY_ID: &str = "paper-crane";
const SAVE_LOCK_ACQUIRED: &str = "proxenos test session save: acquired key lock";
const SAVE_LOCK_RELEASING: &str = "proxenos test session save: releasing key lock";
const REPLY_LOCK_WAITING: &str = "proxenos test session reply: waiting for key lock";
const REPLY_LOCK_ACQUIRED: &str = "proxenos test session reply: acquired key lock";

fn server_with_save_lock_markers(fleet: &Fleet) -> Server {
    let mut command = fleet.command("claude");
    command.env("PROXENOS_TEST_SAVE_LOCK_MARKERS", "1");
    let mut server = Server::start_command(command);
    server.initialize("2025-11-25");
    server
}

fn marker_positions(stderr: &str, marker: &str) -> Vec<usize> {
    stderr.match_indices(marker).map(|(idx, _)| idx).collect()
}

fn sessions_root(base: &Path) -> PathBuf {
    #[cfg(all(unix, not(target_os = "macos")))]
    return base.join(".local/state/proxenos/sessions");
    #[cfg(target_os = "macos")]
    return base.join("Library/Application Support/proxenos/sessions");
    #[cfg(windows)]
    return base.join("proxenos/sessions");
}

fn single_session_file(dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected 1 session file in {}",
        dir.display()
    );
    files.pop().unwrap()
}

fn agy_store(fleet: &Fleet) -> PathBuf {
    fleet.root.join(".gemini/antigravity-cli/conversations")
}

fn argument<'a>(record: &'a Value, flag: &str) -> Option<&'a str> {
    record["args"]
        .as_array()
        .unwrap()
        .windows(2)
        .find(|pair| pair[0] == flag)
        .and_then(|pair| pair[1].as_str())
}

fn wait_for_codex_output_removed(fleet: &Fleet, before_records: usize) {
    let until = Instant::now() + crate::common::WAIT;
    while fleet.records().len() <= before_records {
        assert!(
            Instant::now() < until,
            "codex stub did not record invocation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let records = fleet.records();
    let output_path = PathBuf::from(argument(records.last().unwrap(), "-o").unwrap());
    let until = Instant::now() + crate::common::WAIT;
    while output_path.exists() {
        assert!(
            Instant::now() < until,
            "codex turn did not finish reading and cleaning up output"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reply_after_server_restart_uses_persisted_file_and_stores_no_secrets() {
    let fleet = Fleet::new(&["codex"]);
    let cwd = fleet.root.join("workspace");
    fs::create_dir(&cwd).unwrap();
    fleet.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED, "final_message": "SECRET_ANSWER_MARKER"},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED}
        ]),
    );

    let mut command = fleet.command("claude");
    command.env("PROXENOS_STUB_MARKER", "SECRET_ENV_MARKER");
    let mut server1 = Server::start_command(command);
    server1.initialize("2025-11-25");

    let start = call(
        &mut server1,
        "codex",
        json!({
            "prompt": "SECRET_PROMPT_MARKER",
            "instructions": "SECRET_INSTRUCTIONS_MARKER",
            "model": "saved-model",
            "effort": "high",
            "access": "read-only",
            "mode": "advisory",
            "cwd": cwd,
            "timeout_ms": 5_000
        }),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert!(server1.finish().0.success());

    let provider_dir = sessions_root(&fleet.root).join("codex");
    let session_file = single_session_file(&provider_dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&provider_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&session_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let raw = fs::read_to_string(&session_file).unwrap();
    for forbidden in [
        "SECRET_PROMPT_MARKER",
        "SECRET_INSTRUCTIONS_MARKER",
        "SECRET_ENV_MARKER",
        "SECRET_ANSWER_MARKER",
        "timeout_ms",
    ] {
        assert!(
            !raw.contains(forbidden),
            "session file leaked {forbidden}: {raw}"
        );
    }
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["provider"], "codex");
    assert_eq!(parsed["thread_id"], CODEX_THREAD);
    assert_eq!(parsed["ambiguous"], false);
    assert!(parsed["saved_at_epoch_s"].as_u64().unwrap() > 0);
    assert_eq!(parsed["settings"]["model"], "saved-model");
    assert_eq!(parsed["settings"]["effort"], "high");
    assert_eq!(parsed["settings"]["access"], "read-only");
    assert_eq!(parsed["settings"]["mode"], "advisory");
    assert!(parsed["reported_model"].is_null());

    // Restart server: reply inherits settings from the file on disk without inheriting timeout_ms.
    let mut server2 = fleet.server("claude");
    let reply = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "follow up"}),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["observed"]["passed"]["model"], "saved-model");
    assert_eq!(reply["observed"]["passed"]["effort"], "high");
    assert_eq!(reply["observed"]["passed"]["access"], "read-only");
    assert!(
        reply["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap()
            > 5_000,
        "reply inherited timeout_ms from file: {reply}"
    );

    // Codex fixed-instructions constraints still hold after restart.
    let bad_mode = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "act", "mode": "implementation"}),
    );
    assert_eq!(bad_mode["observed"]["failure"]["kind"], "invalid_request");
    let bad_instructions = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "act", "instructions": "change"}),
    );
    assert_eq!(
        bad_instructions["observed"]["failure"]["kind"],
        "invalid_request"
    );
    assert!(
        bad_instructions["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Omit instructions on a reply"),
        "{bad_instructions}"
    );
    let repeated_instructions = call(
        &mut server2,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "act",
            "instructions": "SECRET_INSTRUCTIONS_MARKER"
        }),
    );
    assert_eq!(
        repeated_instructions["observed"]["failure"]["kind"],
        "invalid_request"
    );
    assert!(
        repeated_instructions["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Omit instructions on a reply"),
        "{repeated_instructions}"
    );
    assert!(server2.finish().0.success());

    let execs: Vec<Value> = fleet
        .records()
        .into_iter()
        .filter(|r| r["args"][0] == "exec")
        .collect();
    assert_eq!(execs.len(), 2);
    assert_eq!(argument(&execs[1], "-m"), Some("saved-model"));
    assert_eq!(
        fs::canonicalize(execs[1]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&cwd).unwrap()
    );

    // An expired session file is treated as unremembered and NOT deleted during load (P2 #8).
    let mut expired = parsed;
    expired["saved_at_epoch_s"] = json!(1);
    fs::write(&session_file, expired.to_string()).unwrap();
    let mut server3 = fleet.server("claude");
    let unremembered = call(
        &mut server3,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "too late"}),
    );
    assert_eq!(
        unremembered["observed"]["failure"]["kind"],
        "invalid_request"
    );
    assert!(
        unremembered["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("no settings")
    );
    assert!(
        session_file.exists(),
        "load must not delete expired session files"
    );

    // A fresh confirmed start cleanly replaces the expired file under the save lock.
    let refreshed = call(
        &mut server3,
        "codex",
        json!({"prompt": "refresh expired", "model": "refreshed-model"}),
    );
    assert_eq!(refreshed["observed"]["state"], "finished", "{refreshed}");
    let refreshed_json: Value =
        serde_json::from_str(&fs::read_to_string(&session_file).unwrap()).unwrap();
    assert_eq!(refreshed_json["ambiguous"], false);
    assert_eq!(refreshed_json["settings"]["model"], "refreshed-model");
    assert!(refreshed_json["saved_at_epoch_s"].as_u64().unwrap() > 1);
}

#[test]
fn conflicting_starts_persist_ambiguity_and_detect_cross_restart_conflicts() {
    let fleet = Fleet::new(&["codex", "agy"]);
    fs::create_dir_all(agy_store(&fleet)).unwrap();
    fs::write(agy_store(&fleet).join("cross-restart.db"), []).unwrap();
    let agy_step = json!({
        "stdout": json!({
            "status": "SUCCESS",
            "conversation_id": "cross-restart",
            "response": "ok"
        })
        .to_string()
    });
    fleet.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED}
        ]),
    );
    fleet.script("agy", json!([agy_step.clone(), agy_step]));

    // Case A: conflicting confirmed starts in the same server persist ambiguity to disk.
    let mut server1 = fleet.server("claude");
    assert_eq!(
        call(
            &mut server1,
            "codex",
            json!({"prompt": "s1", "access": "full"})
        )["observed"]["state"],
        "finished"
    );
    assert_eq!(
        call(
            &mut server1,
            "codex",
            json!({"prompt": "s2", "access": "read-only"})
        )["observed"]["state"],
        "finished"
    );
    // Also seed cross-restart Antigravity session with model m1.
    assert_eq!(
        call(
            &mut server1,
            "antigravity",
            json!({"prompt": "a1", "model": "m1"})
        )["observed"]["state"],
        "finished"
    );
    assert!(server1.finish().0.success());

    let codex_file = single_session_file(&sessions_root(&fleet.root).join("codex"));
    let codex_json: Value =
        serde_json::from_str(&fs::read_to_string(&codex_file).unwrap()).unwrap();
    assert_eq!(codex_json["ambiguous"], true);
    assert!(codex_json["settings"].is_null());
    assert!(codex_json["reported_model"].is_null());

    // After restart, the ambiguous Codex ID stays refused (both bare and with explicit settings).
    let mut server2 = fleet.server("claude");
    let before_reply = fleet.records().len();
    for args in [
        json!({"thread_id": CODEX_THREAD, "prompt": "after restart"}),
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "explicit after restart",
            "cwd": fleet.root,
            "access": "read-only",
            "mode": "advisory"
        }),
    ] {
        let refused = call(&mut server2, "codex-reply", args);
        assert_eq!(refused["observed"]["failure"]["kind"], "invalid_request");
        assert!(
            refused["observed"]["failure"]["message"]
                .as_str()
                .unwrap()
                .contains("conflicting"),
            "{refused}"
        );
    }
    assert_eq!(fleet.records().len(), before_reply);

    // Another confirmed start for an already-ambiguous ID keeps it ambiguous.
    assert_eq!(
        call(
            &mut server2,
            "codex",
            json!({"prompt": "s3", "access": "full"})
        )["observed"]["state"],
        "finished"
    );
    let still_ambiguous = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "still ambiguous"}),
    );
    assert!(
        still_ambiguous["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting")
    );

    // Case B: a conflicting start after a server restart detects the conflict against disk.
    assert_eq!(
        call(
            &mut server2,
            "antigravity",
            json!({"prompt": "a2", "model": "m2"})
        )["observed"]["state"],
        "finished"
    );
    let refused_in_mem = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "cross-restart", "prompt": "mem check"}),
    );
    assert!(
        refused_in_mem["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{refused_in_mem}"
    );
    assert!(server2.finish().0.success());

    // And stays ambiguous on a subsequent restart.
    let mut server3 = fleet.server("claude");
    let refused_after_restart = call(
        &mut server3,
        "antigravity-reply",
        json!({"thread_id": "cross-restart", "prompt": "disk check"}),
    );
    assert!(
        refused_after_restart["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{refused_after_restart}"
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn xdg_state_home_overrides_default_linux_session_location() {
    let fleet = Fleet::new(&["codex"]);
    let custom_state = fleet.root.join("custom-xdg-state");
    fleet.script(
        "codex",
        json!([{"stdout": CODEX_FINISHED}, {"stdout": CODEX_FINISHED}]),
    );

    let start_with_xdg = || {
        let mut cmd = fleet.command("claude");
        cmd.env("XDG_STATE_HOME", &custom_state);
        let mut server = Server::start_command(cmd);
        server.initialize("2025-11-25");
        server
    };

    let mut server1 = start_with_xdg();
    let start = call(
        &mut server1,
        "codex",
        json!({"prompt": "first", "model": "xdg-model"}),
    );
    assert_eq!(start["observed"]["state"], "finished");
    assert!(server1.finish().0.success());

    let xdg_dir = custom_state.join("proxenos/sessions/codex");
    assert!(single_session_file(&xdg_dir).is_file());
    assert!(!sessions_root(&fleet.root).exists());

    let mut server2 = start_with_xdg();
    let reply = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "second"}),
    );
    assert_eq!(reply["observed"]["state"], "finished");
    assert_eq!(reply["observed"]["passed"]["model"], "xdg-model");
}

#[test]
fn d19_no_model_rule_handles_every_branch_across_restarts() {
    let fleet = Fleet::new(&["agy", "codex"]);
    fs::create_dir_all(agy_store(&fleet)).unwrap();
    for id in [
        "explicit-start",
        "reported-start",
        "unreported-start",
        "unsafe-reported",
        "padded-dash-reported",
        "oversized-reported",
    ] {
        fs::write(agy_store(&fleet).join(format!("{id}.db")), []).unwrap();
    }
    let long_model = "m".repeat(257);
    let agy_turn = |id: &str, model: Option<&str>| {
        let mut body = json!({
            "status": "SUCCESS",
            "conversation_id": id,
            "response": "ok"
        });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        json!({"stdout": body.to_string()})
    };

    fleet.script(
        "agy",
        json!([
            // Branch 1: start passed explicit model (with surrounding spaces); provider reported a different one.
            agy_turn("explicit-start", Some("provider-reported")),
            // Branch 2: start passed no model; provider reported a valid padded model.
            agy_turn("reported-start", Some("  provider-chosen  ")),
            // Branch 3: start passed no model; provider reported none.
            agy_turn("unreported-start", None),
            // Branch 4: start passed no model; provider reported an unsafe flag-like model.
            agy_turn("unsafe-reported", Some("--evil-flag")),
            // Branch 5: start passed no model; provider reported leading spaces before '-'.
            agy_turn("padded-dash-reported", Some("  --padded-evil")),
            // Branch 6: start passed no model; provider reported an over-256-byte model.
            agy_turn("oversized-reported", Some(&long_model)),
            // Replies after restart:
            agy_turn("explicit-start", Some("provider-reported")),
            agy_turn("reported-start", Some("provider-chosen")),
            agy_turn("reported-start", Some("caller-override")),
        ]),
    );
    fleet.script(
        "codex",
        json!([{"stdout": CODEX_FINISHED}, {"stdout": CODEX_FINISHED}]),
    );

    let mut server1 = fleet.server("claude");
    // Caller-supplied model names reject leading '-' after trimming and >256 bytes (P2 #9).
    for bad_model in ["  --flag", "-x", &long_model] {
        let rejected = call(
            &mut server1,
            "antigravity",
            json!({"prompt": "reject", "model": bad_model}),
        );
        assert_eq!(
            rejected["observed"]["failure"]["kind"], "invalid_request",
            "{bad_model}: {rejected}"
        );
    }
    assert_eq!(
        call(
            &mut server1,
            "antigravity",
            json!({"prompt": "s1", "model": "  caller-start-model  "})
        )["observed"]["state"],
        "finished"
    );
    let r2 = call(&mut server1, "antigravity", json!({"prompt": "s2"}));
    assert_eq!(r2["observed"]["state"], "finished");
    assert_eq!(r2["reported"]["model"], "provider-chosen");
    let r3 = call(&mut server1, "antigravity", json!({"prompt": "s3"}));
    assert_eq!(r3["observed"]["state"], "finished");
    assert!(r3["reported"]["model"].is_null());
    let r4 = call(&mut server1, "antigravity", json!({"prompt": "s4"}));
    assert_eq!(r4["observed"]["state"], "finished");
    assert!(r4["reported"]["model"].is_null());
    let r5 = call(&mut server1, "antigravity", json!({"prompt": "s5"}));
    assert_eq!(r5["observed"]["state"], "finished");
    assert!(r5["reported"]["model"].is_null());
    let r6 = call(&mut server1, "antigravity", json!({"prompt": "s6"}));
    assert_eq!(r6["observed"]["state"], "finished");
    assert!(r6["reported"]["model"].is_null());
    // Codex start without model.
    assert_eq!(
        call(&mut server1, "codex", json!({"prompt": "c1"}))["observed"]["state"],
        "finished"
    );
    assert!(server1.finish().0.success());

    // Fresh server after restart tests every branch from disk.
    let mut server2 = fleet.server("claude");

    // 1) Start named a model -> reply repeats start's passed model (not provider's reported model).
    let b1 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "explicit-start", "prompt": "r1"}),
    );
    assert_eq!(b1["observed"]["state"], "finished", "{b1}");
    assert_eq!(b1["observed"]["passed"]["model"], "caller-start-model");

    // 2) Start named no model, provider reported one -> reply repeats provider-reported model.
    let b2 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "reported-start", "prompt": "r2"}),
    );
    assert_eq!(b2["observed"]["state"], "finished", "{b2}");
    assert_eq!(b2["observed"]["passed"]["model"], "provider-chosen");

    // 3) Caller explicitly overrides model on reply -> uses caller's model.
    let b3 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "reported-start", "prompt": "r3", "model": "caller-override"}),
    );
    assert_eq!(b3["observed"]["state"], "finished", "{b3}");
    assert_eq!(b3["observed"]["passed"]["model"], "caller-override");

    // 4) Start named no model, provider reported none -> refused with request for a model.
    let b4 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "unreported-start", "prompt": "r4"}),
    );
    assert_eq!(b4["observed"]["state"], "failed", "{b4}");
    assert_eq!(b4["observed"]["failure"]["kind"], "invalid_request");
    assert!(
        b4["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("model")
    );

    // 5) Start named no model, provider reported unsafe/padded-dash/oversized strings -> ignored, reply refused.
    for id in [
        "unsafe-reported",
        "padded-dash-reported",
        "oversized-reported",
    ] {
        let refused = call(
            &mut server2,
            "antigravity-reply",
            json!({"thread_id": id, "prompt": "r5"}),
        );
        assert_eq!(refused["observed"]["state"], "failed", "{id}: {refused}");
        assert_eq!(refused["observed"]["failure"]["kind"], "invalid_request");
    }

    // 6) Codex start passed no model -> reply passes no model (reply_model is false).
    let b6 = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "c2"}),
    );
    assert_eq!(b6["observed"]["state"], "finished", "{b6}");
    assert!(b6["observed"]["passed"]["model"].is_null());

    // Verify exact CLI invocations.
    let agy_calls: Vec<Value> = fleet
        .records()
        .into_iter()
        .filter(|r| r["cli"] == "agy" && argument(r, "--print").is_some())
        .collect();
    assert_eq!(agy_calls.len(), 9);
    assert_eq!(
        argument(&agy_calls[6], "--model"),
        Some("caller-start-model")
    );
    assert_eq!(argument(&agy_calls[7], "--model"), Some("provider-chosen"));
    assert_eq!(argument(&agy_calls[8], "--model"), Some("caller-override"));

    let codex_calls: Vec<Value> = fleet
        .records()
        .into_iter()
        .filter(|r| r["cli"] == "codex" && r["args"][0] == "exec")
        .collect();
    assert_eq!(codex_calls.len(), 2);
    assert!(argument(&codex_calls[1], "-m").is_none());
}

#[test]
fn corrupt_or_unreadable_session_file_fails_safely_and_executes_nothing() {
    let fleet = Fleet::new(&["agy"]);
    fs::create_dir_all(agy_store(&fleet)).unwrap();
    fs::write(agy_store(&fleet).join(format!("{AGY_ID}.db")), []).unwrap();
    fleet.script(
        "agy",
        json!([
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED}
        ]),
    );

    let mut server1 = fleet.server("claude");
    let start = call(
        &mut server1,
        "antigravity",
        json!({"prompt": "seed", "model": "seed-model"}),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert!(server1.finish().0.success());

    let provider_dir = sessions_root(&fleet.root).join("antigravity");
    let session_file = single_session_file(&provider_dir);
    let valid_json: Value =
        serde_json::from_str(&fs::read_to_string(&session_file).unwrap()).unwrap();

    // Fresh server with zero invocations allowed for any corrupt or unreadable file.
    let mut server2 = fleet.server("claude");

    let assert_corrupt = |server: &mut Server, label: &str| {
        let before = fleet.records().len();
        let reply = call(
            server,
            "antigravity-reply",
            json!({
                "thread_id": AGY_ID,
                "prompt": "do not run",
                "model": "explicit",
                "cwd": fleet.root,
                "access": "full",
                "mode": "advisory"
            }),
        );
        assert_eq!(reply["observed"]["state"], "failed", "{label}: {reply}");
        assert_eq!(
            reply["observed"]["failure"]["kind"], "invalid_request",
            "{label}: {reply}"
        );
        assert!(
            reply["observed"]["failure"]["message"]
                .as_str()
                .unwrap()
                .contains("session settings file"),
            "{label}: {reply}"
        );
        assert_eq!(
            fleet.records().len(),
            before,
            "{label}: corrupt file must not reach discovery or launch"
        );
    };

    // 1. Malformed JSON.
    fs::write(&session_file, "{not valid json").unwrap();
    assert_corrupt(&mut server2, "malformed JSON");

    // 2. Unknown field (deny_unknown_fields).
    let mut extra = valid_json.clone();
    extra["command"] = json!("rm -rf /");
    fs::write(&session_file, extra.to_string()).unwrap();
    assert_corrupt(&mut server2, "unknown field");

    // 3. Wrong schema version.
    let mut wrong_version = valid_json.clone();
    wrong_version["schema_version"] = json!(99);
    fs::write(&session_file, wrong_version.to_string()).unwrap();
    assert_corrupt(&mut server2, "wrong schema_version");

    // 4. Invalid settings inside valid schema (relative cwd, and ambiguous with settings).
    let mut bad_cwd = valid_json.clone();
    bad_cwd["settings"]["cwd"] = json!("relative/path");
    fs::write(&session_file, bad_cwd.to_string()).unwrap();
    assert_corrupt(&mut server2, "relative cwd");
    let mut bad_ambiguous = valid_json.clone();
    bad_ambiguous["ambiguous"] = json!(true);
    fs::write(&session_file, bad_ambiguous.to_string()).unwrap();
    assert_corrupt(&mut server2, "ambiguous with settings");

    // 5. Oversized file (> 64 KiB).
    fs::write(&session_file, vec![b' '; 70 * 1024]).unwrap();
    assert_corrupt(&mut server2, "oversized file");

    // 6. Unreadable file.
    fs::write(&session_file, valid_json.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&session_file, fs::Permissions::from_mode(0o000)).unwrap();
        assert_corrupt(&mut server2, "unreadable chmod 000");
        fs::set_permissions(&session_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _locked = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&session_file)
            .unwrap();
        assert_corrupt(&mut server2, "unreadable exclusively locked file");
    }

    // 7. Symlink and FIFO at session path, plus symlink / wrong-mode parent directories (Unix).
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let target = fleet.root.join("decoy.json");
        fs::write(&target, valid_json.to_string()).unwrap();
        fs::remove_file(&session_file).unwrap();
        std::os::unix::fs::symlink(&target, &session_file).unwrap();
        assert_corrupt(&mut server2, "symlink session file");
        fs::remove_file(&session_file).unwrap();

        // FIFO at session path must fail immediately without blocking (O_NONBLOCK + regular-file check).
        let c_fifo = CString::new(session_file.as_os_str().as_bytes()).unwrap();
        // SAFETY: c_fifo is a valid NUL-terminated path inside this test's temporary directory.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");
        assert_corrupt(&mut server2, "FIFO session file");
        fs::remove_file(&session_file).unwrap();

        // Wrong permissions (0755) on provider directory must fail closed on both load and save.
        fs::write(&session_file, valid_json.to_string()).unwrap();
        fs::set_permissions(&provider_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_corrupt(&mut server2, "world-readable provider dir 0755");
        let saved_bad_mode = call(
            &mut server2,
            "antigravity",
            json!({"prompt": "save refuses 0755 dir", "model": "seed-model"}),
        );
        assert_eq!(saved_bad_mode["observed"]["state"], "finished");
        fs::set_permissions(&provider_dir, fs::Permissions::from_mode(0o700)).unwrap();

        // Symlink parent directory (sessions/antigravity -> real 0700 dir) must fail closed on load and save.
        let decoy_dir = fleet.root.join("decoy-provider-dir");
        fs::create_dir(&decoy_dir).unwrap();
        fs::set_permissions(&decoy_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let decoy_session = decoy_dir.join(session_file.file_name().unwrap());
        fs::write(&decoy_session, valid_json.to_string()).unwrap();
        fs::remove_dir_all(&provider_dir).unwrap();
        std::os::unix::fs::symlink(&decoy_dir, &provider_dir).unwrap();
        assert_corrupt(&mut server2, "symlink provider dir");
        let saved_symlink_dir = call(
            &mut server2,
            "antigravity",
            json!({"prompt": "save refuses symlink dir", "model": "seed-model"}),
        );
        assert_eq!(saved_symlink_dir["observed"]["state"], "finished");
        assert_eq!(
            saved_symlink_dir["observed"]["session_save_failure"]["kind"],
            "unknown"
        );
        assert_eq!(
            fs::read_to_string(&decoy_session).unwrap(),
            valid_json.to_string(),
            "save must not write through symlinked parent directory"
        );
        fs::remove_file(&provider_dir).unwrap();
        fs::create_dir(&provider_dir).unwrap();
        fs::set_permissions(&provider_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&session_file, valid_json.to_string()).unwrap();
    }

    // 7b. Directory at the advisory lock path fails closed on load (cross-platform).
    let lock_path = session_file.with_extension("lock");
    let _ = fs::remove_file(&lock_path);
    fs::create_dir(&lock_path).unwrap();
    assert_corrupt(&mut server2, "directory at lock path");
    fs::remove_dir(&lock_path).unwrap();

    // 8. Directory at session path: load fails safely AND a new start fails closed with no staging left.
    let _ = fs::remove_file(&session_file);
    fs::create_dir(&session_file).unwrap();
    assert_corrupt(&mut server2, "directory at session path");

    // Saving over a directory fails without crashing the turn and removes its staging file.
    let saved_err_turn = call(
        &mut server2,
        "antigravity",
        json!({"prompt": "save fails cleanly", "model": "seed-model"}),
    );
    assert_eq!(saved_err_turn["observed"]["state"], "finished");
    let staging_left: Vec<_> = fs::read_dir(&provider_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(
        staging_left.is_empty(),
        "left staging files: {staging_left:?}"
    );
    server2.wait_for_stderr("could not save session settings");

    // Replacing the provider sessions directory with a regular file also fails save safely.
    fs::remove_dir_all(&provider_dir).unwrap();
    fs::write(&provider_dir, "not a directory").unwrap();
    let blocked_dir_turn = call(
        &mut server2,
        "antigravity",
        json!({"prompt": "provider dir obstructed", "model": "seed-model"}),
    );
    assert_eq!(blocked_dir_turn["observed"]["state"], "finished");
    assert!(server2.finish().0.success());

    // 9. Missing / relative state dir: start still succeeds and a fresh server treats reply as unremembered.
    let relative_state_server = || {
        let mut cmd = fleet.command("claude");
        cmd.env("HOME", "relative-home")
            .env("LOCALAPPDATA", "relative-localappdata")
            .env("XDG_STATE_HOME", "relative-xdg");
        let mut server = Server::start_command(cmd);
        server.initialize("2025-11-25");
        server
    };
    let mut server3 = relative_state_server();
    let no_state_start = call(
        &mut server3,
        "antigravity",
        json!({"prompt": "no state dir", "model": "m"}),
    );
    assert_eq!(no_state_start["observed"]["state"], "finished");
    assert!(server3.finish().0.success());
    let mut server4 = relative_state_server();
    let no_state_reply = call(
        &mut server4,
        "antigravity-reply",
        json!({"thread_id": AGY_ID, "prompt": "unremembered"}),
    );
    assert!(
        no_state_reply["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("no settings")
    );
    assert!(server4.finish().0.success());
}

#[test]
fn save_refuses_to_overwrite_corrupt_session_file() {
    let fleet = Fleet::new(&["agy"]);
    fs::create_dir_all(agy_store(&fleet)).unwrap();
    fs::write(agy_store(&fleet).join(format!("{AGY_ID}.db")), []).unwrap();
    fleet.script(
        "agy",
        json!([
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_FINISHED}
        ]),
    );

    let mut server1 = fleet.server("claude");
    let seeded = call(
        &mut server1,
        "antigravity",
        json!({"prompt": "seed", "model": "m1"}),
    );
    assert_eq!(seeded["observed"]["state"], "finished", "{seeded}");

    let provider_dir = sessions_root(&fleet.root).join("antigravity");
    let session_file = single_session_file(&provider_dir);
    let sentinel = "{corrupt-sentinel-must-not-be-overwritten";
    fs::write(&session_file, sentinel).unwrap();

    // Even in the same server that previously cached AGY_ID, a non-conflicting confirmed start
    // fails closed on disk, leaves the corrupt bytes untouched, and evicts the stale cache entry.
    let same_server_start = call(
        &mut server1,
        "antigravity",
        json!({"prompt": "second", "model": "m1"}),
    );
    assert_eq!(same_server_start["observed"]["state"], "finished");
    assert_eq!(fs::read_to_string(&session_file).unwrap(), sentinel);
    server1.wait_for_stderr("corrupt");

    let before_reply = fleet.records().len();
    let refused_same = call(
        &mut server1,
        "antigravity-reply",
        json!({"thread_id": AGY_ID, "prompt": "must fail closed"}),
    );
    assert_eq!(
        refused_same["observed"]["failure"]["kind"],
        "invalid_request"
    );
    assert!(
        refused_same["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("session settings file"),
        "{refused_same}"
    );
    assert_eq!(fleet.records().len(), before_reply);
    assert!(server1.finish().0.success());

    // On a fresh server, a confirmed start also refuses to overwrite the corrupt file.
    let mut server2 = fleet.server("claude");
    let fresh_start = call(
        &mut server2,
        "antigravity",
        json!({"prompt": "third", "model": "m3"}),
    );
    assert_eq!(fresh_start["observed"]["state"], "finished");
    assert_eq!(fs::read_to_string(&session_file).unwrap(), sentinel);
    server2.wait_for_stderr("could not save session settings");
    let before_reply2 = fleet.records().len();
    let refused_fresh = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": AGY_ID, "prompt": "still corrupt"}),
    );
    assert_eq!(
        refused_fresh["observed"]["failure"]["kind"],
        "invalid_request"
    );
    assert_eq!(fleet.records().len(), before_reply2);
    assert!(server2.finish().0.success());
}

#[test]
fn concurrent_save_preserves_sticky_tombstone_and_serializes_replies() {
    // Part A: cross-process sticky tombstone under the advisory lock, and cache updated after disk.
    let fleet_a = Fleet::new(&["codex"]);
    fleet_a.script(
        "codex",
        json!([{"stdout": CODEX_FINISHED}, {"stdout": CODEX_FINISHED}]),
    );
    let mut seed_a = fleet_a.server("claude");
    let seeded_a = call(
        &mut seed_a,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded_a["observed"]["state"], "finished", "{seeded_a}");
    assert!(seed_a.finish().0.success());

    let session_a = single_session_file(&sessions_root(&fleet_a.root).join("codex"));
    let lock_path_a = session_a.with_extension("lock");
    let lock_a = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path_a)
        .unwrap();
    lock_a.lock().unwrap();

    let mut server_a = fleet_a.server("claude");
    server_a.request(1, "tools/list", json!({}));
    let before_a = fleet_a.records().len();
    server_a.start_call(10, "codex", json!({"prompt": "race", "access": "full"}));
    wait_for_codex_output_removed(&fleet_a, before_a);

    // Simulate another server writing an ambiguity tombstone while server_a waits on the lock.
    let mut tombstone: Value =
        serde_json::from_str(&fs::read_to_string(&session_a).unwrap()).unwrap();
    tombstone["ambiguous"] = json!(true);
    tombstone["settings"] = Value::Null;
    tombstone["reported_model"] = Value::Null;
    fs::write(&session_a, tombstone.to_string()).unwrap();
    drop(lock_a);

    let finished_a: Value =
        serde_json::from_str(crate::common::text(&server_a.response(10))).unwrap();
    assert_eq!(finished_a["observed"]["state"], "finished", "{finished_a}");
    let after_a: Value = serde_json::from_str(&fs::read_to_string(&session_a).unwrap()).unwrap();
    assert_eq!(
        after_a["ambiguous"], true,
        "save downgraded sticky ambiguity tombstone: {after_a}"
    );
    let reply_a = call(
        &mut server_a,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "must see tombstone"}),
    );
    assert_eq!(
        reply_a["observed"]["failure"]["kind"], "invalid_request",
        "{reply_a}"
    );
    assert!(
        reply_a["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{reply_a}"
    );
    assert!(server_a.finish().0.success());

    // Part B: in-process reply waits while a conflicting save is in flight and never dispatches stale settings.
    let fleet_b = Fleet::new(&["codex"]);
    fleet_b.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED}
        ]),
    );
    let mut server_b = server_with_save_lock_markers(&fleet_b);
    let seeded_b = call(
        &mut server_b,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded_b["observed"]["state"], "finished", "{seeded_b}");

    let session_b = single_session_file(&sessions_root(&fleet_b.root).join("codex"));
    let lock_b = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_b.with_extension("lock"))
        .unwrap();
    lock_b.lock().unwrap();

    server_b.start_call(
        20,
        "codex",
        json!({"prompt": "conflicting start", "access": "read-only"}),
    );
    // The seed emitted the first marker. The next save worker holds the key across file I/O.
    server_b.wait_for_stderr_count(SAVE_LOCK_ACQUIRED, 2);

    // Issue a reply while save #20 is still blocked on the advisory lock.
    server_b.start_call(
        21,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "must wait for save"}),
    );
    server_b.wait_for_stderr(REPLY_LOCK_WAITING);
    assert_eq!(
        fleet_b
            .records()
            .iter()
            .filter(|r| r["args"][0] == "exec")
            .count(),
        2,
        "reply dispatched before in-flight save finished"
    );
    drop(lock_b);

    let f1 = server_b.exact_next_frame();
    let f2 = server_b.exact_next_frame();
    let (start_frame, reply_frame) = if f1["id"] == 20 { (f1, f2) } else { (f2, f1) };
    assert_eq!(start_frame["id"], 20);
    assert_eq!(reply_frame["id"], 21);
    let start_body: Value = serde_json::from_str(crate::common::text(&start_frame)).unwrap();
    let reply_body: Value = serde_json::from_str(crate::common::text(&reply_frame)).unwrap();
    assert_eq!(start_body["observed"]["state"], "finished", "{start_body}");
    assert_eq!(
        reply_body["observed"]["failure"]["kind"], "invalid_request",
        "{reply_body}"
    );
    assert!(
        reply_body["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{reply_body}"
    );
    assert_eq!(
        fleet_b
            .records()
            .iter()
            .filter(|r| r["args"][0] == "exec")
            .count(),
        2,
        "ambiguous reply must never dispatch"
    );
    assert!(server_b.finish().0.success());

    let stderr = server_b.stderr();
    let save_acquired = marker_positions(&stderr, SAVE_LOCK_ACQUIRED);
    let save_releasing = marker_positions(&stderr, SAVE_LOCK_RELEASING);
    let reply_waiting = marker_positions(&stderr, REPLY_LOCK_WAITING);
    let reply_acquired = marker_positions(&stderr, REPLY_LOCK_ACQUIRED);
    assert_eq!(save_acquired.len(), 2, "{stderr}");
    assert_eq!(save_releasing.len(), 2, "{stderr}");
    assert_eq!(reply_waiting.len(), 1, "{stderr}");
    assert_eq!(reply_acquired.len(), 1, "{stderr}");
    assert!(save_acquired[1] < reply_waiting[0], "{stderr}");
    assert!(reply_waiting[0] < save_releasing[1], "{stderr}");
    assert!(save_releasing[1] < reply_acquired[0], "{stderr}");
}

#[test]
fn session_load_respects_call_deadline_and_cancellation() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script("codex", json!([{"stdout": CODEX_FINISHED}]));
    let mut server1 = fleet.server("claude");
    let seeded = call(&mut server1, "codex", json!({"prompt": "seed"}));
    assert_eq!(seeded["observed"]["state"], "finished", "{seeded}");
    assert!(server1.finish().0.success());

    let session_file = single_session_file(&sessions_root(&fleet.root).join("codex"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_file.with_extension("lock"))
        .unwrap();
    lock.lock().unwrap();

    // Deadline bounds sessions::load.
    let mut server2 = fleet.server("claude");
    let began = Instant::now();
    let timed_out = call(
        &mut server2,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "blocked", "timeout_ms": 500}),
    );
    assert_eq!(
        timed_out["observed"]["failure"]["kind"], "timeout",
        "{timed_out}"
    );
    assert!(
        timed_out["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Loading session settings"),
        "{timed_out}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "session load ignored deadline: {:?}",
        began.elapsed()
    );

    // Cancellation (connection EOF) bounds sessions::load.
    server2.start_call(
        30,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "cancel load"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    server2.close_input();
    let cancelled: Value =
        serde_json::from_str(crate::common::text(&server2.response(30))).unwrap();
    assert_eq!(cancelled["observed"]["state"], "cancelled", "{cancelled}");
    assert_eq!(
        cancelled["observed"]["failure"]["kind"], "cancellation",
        "{cancelled}"
    );
    assert!(
        cancelled["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Loading session settings"),
        "{cancelled}"
    );

    drop(lock);
    assert!(server2.finish().0.success());
}

#[test]
fn save_refuses_session_payload_above_size_limit() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script("codex", json!([{"stdout": CODEX_FINISHED}]));
    let mut cmd = fleet.command("claude");
    cmd.env("PROXENOS_TEST_SESSION_MAX_BYTES", "64");
    let mut server = Server::start_command(cmd);
    server.initialize("2025-11-25");

    let done = call(
        &mut server,
        "codex",
        json!({"prompt": "too big for tiny cap", "model": "saved-model"}),
    );
    assert_eq!(done["observed"]["state"], "finished", "{done}");
    server.wait_for_stderr("session payload exceeds size limit");
    let provider_dir = sessions_root(&fleet.root).join("codex");
    let json_files: Vec<_> = fs::read_dir(&provider_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert!(
        json_files.is_empty(),
        "oversized payload must not be written: {json_files:?}"
    );
    // Because StartStore holds Unverified and disk is missing, a reply must fail closed instead of
    // treating the unverified start as forgotten or dispatchable.
    let before_reply = fleet.records().len();
    let refused = call(
        &mut server,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must fail closed while missing on disk",
            "cwd": fleet.root,
            "access": "read-only",
            "mode": "advisory",
            "model": "saved-model"
        }),
    );
    assert_eq!(
        refused["observed"]["failure"]["kind"], "invalid_request",
        "{refused}"
    );
    assert!(
        refused["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("not been persisted"),
        "{refused}"
    );
    assert_eq!(fleet.records().len(), before_reply);
    assert!(server.finish().0.success());
}

#[test]
fn save_error_never_caches_unverified_settings_or_clears_ambiguity() {
    // Part A (P1): a tombstone on disk + `.lock` obstructed by a directory (non-InvalidData error
    // on Unix and Windows). A fresh server running a confirmed start must NOT cache an unverified
    // `Some` in StartStore; the subsequent reply must go back to disk and be refused.
    let fleet = Fleet::new(&["codex"]);
    fleet.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED}
        ]),
    );
    let mut seed = fleet.server("claude");
    assert_eq!(
        call(
            &mut seed,
            "codex",
            json!({"prompt": "s1", "access": "full"})
        )["observed"]["state"],
        "finished"
    );
    assert_eq!(
        call(
            &mut seed,
            "codex",
            json!({"prompt": "s2", "access": "read-only"})
        )["observed"]["state"],
        "finished"
    );
    assert!(seed.finish().0.success());

    let session_file = single_session_file(&sessions_root(&fleet.root).join("codex"));
    let lock_path = session_file.with_extension("lock");
    fs::remove_file(&lock_path).unwrap();
    fs::create_dir(&lock_path).unwrap();

    let mut fresh = fleet.server("claude");
    let start = call(
        &mut fresh,
        "codex",
        json!({"prompt": "s3", "access": "full"}),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    fresh.wait_for_stderr("could not save session settings");
    let before_reply = fleet.records().len();
    let refused_while_obstructed = call(
        &mut fresh,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "must not bypass disk"}),
    );
    assert_eq!(
        refused_while_obstructed["observed"]["failure"]["kind"], "invalid_request",
        "{refused_while_obstructed}"
    );
    assert_eq!(
        fleet.records().len(),
        before_reply,
        "reply must not dispatch from unverified cache while save failed"
    );

    // Removing the lock obstruction still exposes the on-disk ambiguity tombstone.
    fs::remove_dir(&lock_path).unwrap();
    let refused_tombstone = call(
        &mut fresh,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "still ambiguous on disk"}),
    );
    assert_eq!(
        refused_tombstone["observed"]["failure"]["kind"], "invalid_request",
        "{refused_tombstone}"
    );
    assert!(
        refused_tombstone["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{refused_tombstone}"
    );
    assert_eq!(fleet.records().len(), before_reply);
    assert!(fresh.finish().0.success());

    // Part B (P1): an in-memory ambiguity marker (`desired == None`) must NOT be erased when
    // `sessions::save` fails (including with InvalidData on a corrupt disk file).
    let fleet2 = Fleet::new(&["codex"]);
    fleet2.script(
        "codex",
        json!([{"stdout": CODEX_FINISHED}, {"stdout": CODEX_FINISHED}]),
    );
    let mut server2 = fleet2.server("claude");
    assert_eq!(
        call(
            &mut server2,
            "codex",
            json!({"prompt": "a1", "access": "full"})
        )["observed"]["state"],
        "finished"
    );
    let session2 = single_session_file(&sessions_root(&fleet2.root).join("codex"));
    fs::write(&session2, "{corrupt-while-ambiguity-triggered").unwrap();
    // Conflicting second start sets desired = None in memory, while save fails with InvalidData.
    assert_eq!(
        call(
            &mut server2,
            "codex",
            json!({"prompt": "a2", "access": "read-only"})
        )["observed"]["state"],
        "finished"
    );
    // Even if the corrupt file disappears afterward, StartStore kept `None` and refuses replies.
    fs::remove_file(&session2).unwrap();
    let before_reply2 = fleet2.records().len();
    let refused_mem = call(
        &mut server2,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must stay ambiguous in memory",
            "cwd": fleet2.root,
            "access": "read-only",
            "mode": "advisory"
        }),
    );
    assert_eq!(
        refused_mem["observed"]["failure"]["kind"], "invalid_request",
        "{refused_mem}"
    );
    assert!(
        refused_mem["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{refused_mem}"
    );
    assert_eq!(fleet2.records().len(), before_reply2);
    assert!(server2.finish().0.success());

    // Part C: after a save error leaves StartStore in Unverified(s), a reply goes to disk:
    // - when disk holds a conflicting non-tombstone record, the reply refuses as conflicting;
    // - when disk holds a matching record, the reply verifies the match, promotes to Known, and
    //   proceeds.
    let fleet3 = Fleet::new(&["codex"]);
    fleet3.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED, "final_message": "verified-from-disk-reply"}
        ]),
    );
    let mut seed3 = fleet3.server("claude");
    assert_eq!(
        call(
            &mut seed3,
            "codex",
            json!({"prompt": "seed-full", "access": "full"})
        )["observed"]["state"],
        "finished"
    );
    assert!(seed3.finish().0.success());

    let session3 = single_session_file(&sessions_root(&fleet3.root).join("codex"));
    let lock3 = session3.with_extension("lock");
    fs::remove_file(&lock3).unwrap();
    fs::create_dir(&lock3).unwrap();

    // Conflicting start on a fresh server fails to save (Unverified(read-only)); once the lock
    // obstruction clears, reply reads disk (access: full), detects the conflict, and refuses.
    let mut conflict_srv = fleet3.server("claude");
    let failed_conflict = call(
        &mut conflict_srv,
        "codex",
        json!({"prompt": "conflict-save-err", "access": "read-only"}),
    );
    assert_eq!(failed_conflict["observed"]["state"], "finished");
    assert_eq!(
        failed_conflict["observed"]["session_save_failure"]["kind"],
        "unknown"
    );
    fs::remove_dir(&lock3).unwrap();
    let before_conflict_reply = fleet3.records().len();
    let refused_conflict = call(
        &mut conflict_srv,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "must detect disk conflict"}),
    );
    assert_eq!(
        refused_conflict["observed"]["failure"]["kind"], "invalid_request",
        "{refused_conflict}"
    );
    assert!(
        refused_conflict["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{refused_conflict}"
    );
    assert_eq!(fleet3.records().len(), before_conflict_reply);
    assert!(conflict_srv.finish().0.success());

    // Matching start on a fresh server fails to save (Unverified(full)); once the lock obstruction
    // clears, reply reads disk (access: full), verifies the match, promotes to Known, and proceeds.
    fs::create_dir(&lock3).unwrap();
    let mut match_srv = fleet3.server("claude");
    let failed_match = call(
        &mut match_srv,
        "codex",
        json!({"prompt": "match-save-err", "access": "full"}),
    );
    assert_eq!(failed_match["observed"]["state"], "finished");
    assert_eq!(
        failed_match["observed"]["session_save_failure"]["kind"],
        "unknown"
    );
    fs::remove_dir(&lock3).unwrap();
    let verified_reply = call(
        &mut match_srv,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "matches disk record"}),
    );
    assert_eq!(
        verified_reply["observed"]["state"], "finished",
        "{verified_reply}"
    );
    assert_eq!(
        verified_reply["reported"]["content"], "verified-from-disk-reply",
        "{verified_reply}"
    );
    assert!(match_srv.finish().0.success());
}

#[test]
fn session_save_respects_call_deadline_and_cancellation() {
    // Part A: external holder of `<key>.lock` blocks start A (conflicting with disk) past its
    // short timeout. Start A returns the finished answer with session_save_failure=timeout while
    // its detached worker retains the owned key lock. A reply issued afterward must wait (never
    // bypassing the detached save to cache pre-conflict disk state), and once the external lock
    // releases, the detached worker writes the ambiguity tombstone and reconciles StartStore so
    // pending and subsequent replies are refused as ambiguous.
    let fleet = Fleet::new(&["codex"]);
    fleet.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED, "final_message": "saved-timeout-answer"}
        ]),
    );
    let mut server1 = fleet.server("claude");
    let seeded = call(
        &mut server1,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded["observed"]["state"], "finished", "{seeded}");
    assert!(server1.finish().0.success());

    let session_file = single_session_file(&sessions_root(&fleet.root).join("codex"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_file.with_extension("lock"))
        .unwrap();
    lock.lock().unwrap();

    let mut server2 = server_with_save_lock_markers(&fleet);
    server2.request(1, "tools/list", json!({}));
    let began = Instant::now();
    let timed = call(
        &mut server2,
        "codex",
        json!({
            "prompt": "save times out",
            "access": "read-only",
            "timeout_ms": 3_000
        }),
    );
    let elapsed = began.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "blocked save ignored call deadline ({elapsed:?}): {timed}"
    );
    assert_eq!(timed["observed"]["state"], "finished", "{timed}");
    assert_eq!(
        timed["reported"]["content"], "saved-timeout-answer",
        "{timed}"
    );
    assert_eq!(
        timed["observed"]["session_save_failure"]["kind"], "timeout",
        "{timed}"
    );
    server2.wait_for_stderr("could not save session settings");
    server2.wait_for_stderr(SAVE_LOCK_ACQUIRED);

    // Reply with a short timeout waits on the detached save's owned key lock and times out.
    let before_replies = fleet.records().len();
    let reply_blocked = call(
        &mut server2,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must wait for detached save",
            "timeout_ms": 500
        }),
    );
    assert_eq!(
        reply_blocked["observed"]["failure"]["kind"], "timeout",
        "{reply_blocked}"
    );
    assert!(
        reply_blocked["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Loading session settings"),
        "{reply_blocked}"
    );

    // Issue a reply while the detached save is still holding the owned key lock, then release the
    // external file lock so the detached save detects the conflict against disk (`access: full`),
    // writes the ambiguity tombstone, and reconciles StartStore before the reply acquires the lock.
    server2.start_call(
        35,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must observe detached save ambiguity"
        }),
    );
    assert!(
        server2
            .next_frame(Instant::now() + Duration::from_millis(150))
            .is_none(),
        "reply must wait for detached save worker"
    );
    assert_eq!(fleet.records().len(), before_replies);
    drop(lock);

    let pending_reply: Value =
        serde_json::from_str(crate::common::text(&server2.response(35))).unwrap();
    assert_eq!(
        pending_reply["observed"]["failure"]["kind"], "invalid_request",
        "{pending_reply}"
    );
    assert!(
        pending_reply["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{pending_reply}"
    );
    let after_disk: Value =
        serde_json::from_str(&fs::read_to_string(&session_file).unwrap()).unwrap();
    assert_eq!(after_disk["ambiguous"], true, "{after_disk}");

    // A later reply also refuses immediately as ambiguous without dispatching.
    let later_reply = call(
        &mut server2,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "still ambiguous"
        }),
    );
    assert_eq!(
        later_reply["observed"]["failure"]["kind"], "invalid_request",
        "{later_reply}"
    );
    assert!(
        later_reply["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{later_reply}"
    );
    assert_eq!(fleet.records().len(), before_replies);
    assert!(server2.finish().0.success());

    // Part B: cancellation (connection EOF) during a blocked save still returns the finished
    // answer promptly with observed.session_save_failure = cancellation while lock is still held.
    let fleet_b = Fleet::new(&["codex"]);
    fleet_b.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED, "final_message": "saved-cancel-answer"}
        ]),
    );
    let mut server_b = server_with_save_lock_markers(&fleet_b);
    let seeded_b = call(
        &mut server_b,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded_b["observed"]["state"], "finished", "{seeded_b}");

    let session_b = single_session_file(&sessions_root(&fleet_b.root).join("codex"));
    let lock_b = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_b.with_extension("lock"))
        .unwrap();
    lock_b.lock().unwrap();

    server_b.start_call(
        40,
        "codex",
        json!({"prompt": "save cancelled", "model": "cancelled-save-model"}),
    );
    server_b.wait_for_stderr_count(SAVE_LOCK_ACQUIRED, 2);
    let cancel_began = Instant::now();
    server_b.close_input();
    let cancelled: Value =
        serde_json::from_str(crate::common::text(&server_b.response(40))).unwrap();
    let cancel_elapsed = cancel_began.elapsed();
    assert!(
        cancel_elapsed < Duration::from_secs(6),
        "blocked save ignored cancellation ({cancel_elapsed:?}): {cancelled}"
    );
    assert_eq!(cancelled["observed"]["state"], "finished", "{cancelled}");
    assert_eq!(
        cancelled["reported"]["content"], "saved-cancel-answer",
        "{cancelled}"
    );
    assert_eq!(
        cancelled["observed"]["session_save_failure"]["kind"], "cancellation",
        "{cancelled}"
    );

    drop(lock_b);
    assert!(server_b.finish().0.success());
    let after_cancel_disk: Value =
        serde_json::from_str(&fs::read_to_string(&session_b).unwrap()).unwrap();
    assert_eq!(after_cancel_disk["ambiguous"], true, "{after_cancel_disk}");
}

#[test]
fn pre_spawn_save_timeout_and_cancellation_fail_closed_on_conflict() {
    // Sub-case (a): a conflicting second start in the same process times out while waiting for
    // `key_lock` (held by an earlier identical start's detached save worker), so no save worker is
    // spawned for the conflicting start. The caller must still fail StartStore closed to None so
    // the in-flight worker preserves None and a later reply is refused as conflicting.
    let fleet_a = Fleet::new(&["codex"]);
    fleet_a.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED, "final_message": "pre-spawn-timeout-answer"}
        ]),
    );
    let mut server_a = server_with_save_lock_markers(&fleet_a);
    let seeded_a = call(
        &mut server_a,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded_a["observed"]["state"], "finished", "{seeded_a}");

    let session_a = single_session_file(&sessions_root(&fleet_a.root).join("codex"));
    let lock_a = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_a.with_extension("lock"))
        .unwrap();
    lock_a.lock().unwrap();

    // Start an identical repeat whose detached worker holds `key_lock` while blocked on `<key>.lock`.
    let holder_a = call(
        &mut server_a,
        "codex",
        json!({"prompt": "hold key lock", "access": "full", "timeout_ms": 3_000}),
    );
    assert_eq!(holder_a["observed"]["state"], "finished", "{holder_a}");
    assert_eq!(
        holder_a["observed"]["session_save_failure"]["kind"], "timeout",
        "{holder_a}"
    );
    server_a.wait_for_stderr_count(SAVE_LOCK_ACQUIRED, 2);

    // Conflicting start times out in `key_lock.lock_owned().await` before spawning a save worker.
    let conflict_timeout = call(
        &mut server_a,
        "codex",
        json!({
            "prompt": "conflicting pre-spawn timeout",
            "access": "read-only",
            "timeout_ms": 3_000
        }),
    );
    assert_eq!(
        conflict_timeout["observed"]["state"], "finished",
        "{conflict_timeout}"
    );
    assert_eq!(
        conflict_timeout["reported"]["content"], "pre-spawn-timeout-answer",
        "{conflict_timeout}"
    );
    assert_eq!(
        conflict_timeout["observed"]["session_save_failure"]["kind"], "timeout",
        "{conflict_timeout}"
    );

    // Release `<key>.lock` so the identical repeat's worker finishes saving `access: full` to disk
    // and reconciles StartStore, which must preserve the None ambiguity marker.
    drop(lock_a);
    let before_reply_a = fleet_a.records().len();
    let reply_a = call(
        &mut server_a,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must fail closed after pre-spawn timeout"
        }),
    );
    assert_eq!(
        reply_a["observed"]["failure"]["kind"], "invalid_request",
        "{reply_a}"
    );
    assert!(
        reply_a["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{reply_a}"
    );
    assert_eq!(fleet_a.records().len(), before_reply_a);
    assert!(server_a.finish().0.success());

    // Sub-case (b): same setup, except the conflicting start is cancelled via
    // `notifications/cancelled` while waiting in `key_lock.lock_owned().await` before a worker is
    // spawned.
    let fleet_b = Fleet::new(&["codex"]);
    fleet_b.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED}
        ]),
    );
    let mut server_b = server_with_save_lock_markers(&fleet_b);
    let seeded_b = call(
        &mut server_b,
        "codex",
        json!({"prompt": "seed", "access": "full"}),
    );
    assert_eq!(seeded_b["observed"]["state"], "finished", "{seeded_b}");

    let session_b = single_session_file(&sessions_root(&fleet_b.root).join("codex"));
    let lock_b = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_b.with_extension("lock"))
        .unwrap();
    lock_b.lock().unwrap();

    let holder_b = call(
        &mut server_b,
        "codex",
        json!({"prompt": "hold key lock", "access": "full", "timeout_ms": 3_000}),
    );
    assert_eq!(holder_b["observed"]["state"], "finished", "{holder_b}");
    assert_eq!(
        holder_b["observed"]["session_save_failure"]["kind"], "timeout",
        "{holder_b}"
    );
    server_b.wait_for_stderr_count(SAVE_LOCK_ACQUIRED, 2);

    let before_cancel = fleet_b.records().len();
    server_b.start_call(
        42,
        "codex",
        json!({"prompt": "conflicting pre-spawn cancel", "access": "read-only"}),
    );
    wait_for_codex_output_removed(&fleet_b, before_cancel);

    server_b.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 42, "reason": "test" }
    }));
    server_b
        .wait_for_stderr("could not save session settings: Saving session settings was cancelled.");

    drop(lock_b);
    let before_reply_b = fleet_b.records().len();
    let reply_b = call(
        &mut server_b,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "must fail closed after pre-spawn cancellation"
        }),
    );
    assert_eq!(
        reply_b["observed"]["failure"]["kind"], "invalid_request",
        "{reply_b}"
    );
    assert!(
        reply_b["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting"),
        "{reply_b}"
    );
    assert_eq!(fleet_b.records().len(), before_reply_b);
    assert!(server_b.finish().0.success());

    // Sub-case (c): a start whose save times out while blocked on `<key>.lock` leaves
    // Unverified(full) in StartStore; once `<key>.lock` is released and the detached save lands,
    // a waiting reply proceeds with the landed settings.
    let fleet_c = Fleet::new(&["codex"]);
    fleet_c.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED},
            {"stdout": CODEX_FINISHED, "final_message": "reply-after-save-landed"}
        ]),
    );
    let mut seed_c = fleet_c.server("claude");
    let seeded_c = call(
        &mut seed_c,
        "codex",
        json!({"prompt": "locate lock", "access": "full"}),
    );
    assert_eq!(seeded_c["observed"]["state"], "finished", "{seeded_c}");
    assert!(seed_c.finish().0.success());

    let session_c = single_session_file(&sessions_root(&fleet_c.root).join("codex"));
    let lock_c = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_c.with_extension("lock"))
        .unwrap();
    lock_c.lock().unwrap();
    fs::remove_file(&session_c).unwrap();

    let mut server_c = fleet_c.server("claude");
    server_c.request(1, "tools/list", json!({}));
    let timed_c = call(
        &mut server_c,
        "codex",
        json!({"prompt": "delayed save", "access": "full", "timeout_ms": 3_000}),
    );
    assert_eq!(timed_c["observed"]["state"], "finished", "{timed_c}");
    assert_eq!(
        timed_c["observed"]["session_save_failure"]["kind"], "timeout",
        "{timed_c}"
    );

    let before_reply_c = fleet_c.records().len();
    server_c.start_call(
        36,
        "codex-reply",
        json!({"thread_id": CODEX_THREAD, "prompt": "waits for save to land"}),
    );
    assert!(
        server_c
            .next_frame(Instant::now() + Duration::from_millis(150))
            .is_none(),
        "reply must wait for in-flight save"
    );
    assert_eq!(fleet_c.records().len(), before_reply_c);

    drop(lock_c);
    let landed_reply: Value =
        serde_json::from_str(crate::common::text(&server_c.response(36))).unwrap();
    assert_eq!(
        landed_reply["observed"]["state"], "finished",
        "{landed_reply}"
    );
    assert_eq!(
        landed_reply["reported"]["content"], "reply-after-save-landed",
        "{landed_reply}"
    );
    let reply_record = fleet_c.records().pop().unwrap();
    assert!(
        reply_record["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "sandbox_mode=\"danger-full-access\""),
        "reply should inherit full access from the landed save: {reply_record}"
    );
    assert!(server_c.finish().0.success());
}
