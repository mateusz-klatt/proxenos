//! Persisted session settings files, D19 no-model branches, and corrupt-file safety (D19).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::common::Server;
use crate::provider_support::{Fleet, call};

const CODEX_FINISHED: &str = include_str!("../fixtures/codex/finished.jsonl");
const CODEX_THREAD: &str = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";
const AGY_FINISHED: &str = include_str!("../fixtures/antigravity/finished.json");
const AGY_ID: &str = "paper-crane";

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

#[test]
fn reply_after_server_restart_uses_persisted_file_and_stores_no_secrets() {
    let fleet = Fleet::new(&["codex"]);
    let cwd = fleet.root.join("workspace");
    fs::create_dir(&cwd).unwrap();
    fleet.script(
        "codex",
        json!([
            {"stdout": CODEX_FINISHED, "final_message": "SECRET_ANSWER_MARKER"},
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

    // An expired session file is treated as unremembered and cleaned up on a fresh server.
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
    assert!(!session_file.exists());
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
    ] {
        fs::write(agy_store(&fleet).join(format!("{id}.db")), []).unwrap();
    }
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
            // Branch 1: start passed explicit model; provider reported a different one.
            agy_turn("explicit-start", Some("provider-reported")),
            // Branch 2: start passed no model; provider reported a valid model.
            agy_turn("reported-start", Some("provider-chosen")),
            // Branch 3: start passed no model; provider reported none.
            agy_turn("unreported-start", None),
            // Branch 4: start passed no model; provider reported an unsafe flag-like model.
            agy_turn("unsafe-reported", Some("--evil-flag")),
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
    assert_eq!(
        call(
            &mut server1,
            "antigravity",
            json!({"prompt": "s1", "model": "caller-start-model"})
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

    // 5) Start named no model, provider reported an unsafe string -> ignored, reply refused.
    let b5 = call(
        &mut server2,
        "antigravity-reply",
        json!({"thread_id": "unsafe-reported", "prompt": "r5"}),
    );
    assert_eq!(b5["observed"]["state"], "failed", "{b5}");
    assert_eq!(b5["observed"]["failure"]["kind"], "invalid_request");

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
    assert_eq!(agy_calls.len(), 7);
    assert_eq!(
        argument(&agy_calls[4], "--model"),
        Some("caller-start-model")
    );
    assert_eq!(argument(&agy_calls[5], "--model"), Some("provider-chosen"));
    assert_eq!(argument(&agy_calls[6], "--model"), Some("caller-override"));

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
            {"stdout": AGY_FINISHED}
        ]),
    );

    let mut server1 = fleet.server("claude");
    let start = call(
        &mut server1,
        "antigravity",
        json!({"prompt": "seed", "model": "seed-model"}),
    );
    assert_eq!(start["observed"]["state"], "finished");
    assert!(server1.finish().0.success());

    let provider_dir = sessions_root(&fleet.root).join("antigravity");
    let session_file = single_session_file(&provider_dir);
    let valid_json: Value =
        serde_json::from_str(&fs::read_to_string(&session_file).unwrap()).unwrap();

    // Fresh server with zero invocations allowed for any corrupt or unreadable file.
    let invocations_before = fleet.records().len();
    let mut server2 = fleet.server("claude");

    let mut assert_corrupt = |label: &str| {
        let reply = call(
            &mut server2,
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
            invocations_before,
            "{label}: corrupt file must not reach discovery or launch"
        );
    };

    // 1. Malformed JSON.
    fs::write(&session_file, "{not valid json").unwrap();
    assert_corrupt("malformed JSON");

    // 2. Unknown field (deny_unknown_fields).
    let mut extra = valid_json.clone();
    extra["command"] = json!("rm -rf /");
    fs::write(&session_file, extra.to_string()).unwrap();
    assert_corrupt("unknown field");

    // 3. Wrong schema version.
    let mut wrong_version = valid_json.clone();
    wrong_version["schema_version"] = json!(99);
    fs::write(&session_file, wrong_version.to_string()).unwrap();
    assert_corrupt("wrong schema_version");

    // 4. Invalid settings inside valid schema (relative cwd, and ambiguous with settings).
    let mut bad_cwd = valid_json.clone();
    bad_cwd["settings"]["cwd"] = json!("relative/path");
    fs::write(&session_file, bad_cwd.to_string()).unwrap();
    assert_corrupt("relative cwd");
    let mut bad_ambiguous = valid_json.clone();
    bad_ambiguous["ambiguous"] = json!(true);
    fs::write(&session_file, bad_ambiguous.to_string()).unwrap();
    assert_corrupt("ambiguous with settings");

    // 5. Oversized file (> 64 KiB).
    fs::write(&session_file, vec![b' '; 70 * 1024]).unwrap();
    assert_corrupt("oversized file");

    // 6. Unreadable file.
    fs::write(&session_file, valid_json.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&session_file, fs::Permissions::from_mode(0o000)).unwrap();
        assert_corrupt("unreadable chmod 000");
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
        assert_corrupt("unreadable exclusively locked file");
    }

    // 7. Symlink at session path (Unix).
    #[cfg(unix)]
    {
        let target = fleet.root.join("decoy.json");
        fs::write(&target, valid_json.to_string()).unwrap();
        fs::remove_file(&session_file).unwrap();
        std::os::unix::fs::symlink(&target, &session_file).unwrap();
        assert_corrupt("symlink session file");
        fs::remove_file(&session_file).unwrap();
    }

    // 8. Directory at session path: load fails safely AND a new start cleans up its staging file.
    let _ = fs::remove_file(&session_file);
    fs::create_dir(&session_file).unwrap();
    assert_corrupt("directory at session path");

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
    assert!(server2.stderr().contains("could not save session settings"));

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
