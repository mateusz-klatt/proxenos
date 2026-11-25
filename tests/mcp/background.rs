//! Background delegations, `await`, `cancel`, concurrency limit and shutdown (D17).

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{Fleet, call, turn};

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lock_file(path: &Path) -> File {
    let ready = format!("{}.ready", path.display());
    wait_until("stub did not hold lock file", || Path::new(&ready).exists());
    File::options().write(true).open(path).unwrap()
}

fn is_free(file: &File) -> bool {
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            true
        }
        Err(_) => false,
    }
}

fn raw_call(server: &mut Server, id: i64, name: &str, args: Value) -> (Value, bool) {
    let response = server.call(id, name, args);
    let is_error = response["result"]["isError"] == true;
    let body: Value = serde_json::from_str(text(&response)).unwrap();
    (body, is_error)
}

#[test]
fn wait_false_await_before_and_after_completion_and_cancel() {
    let fleet = Fleet::new(&["agy"]);
    let lock_a = fleet.root.join("a.lock");
    let go_a = fleet.root.join("a.go");
    let lock_b = fleet.root.join("b.lock");
    let lock_c = fleet.root.join("c.lock");
    let mut step_a = turn("finished from background");
    step_a["hold_file"] = json!(lock_a);
    step_a["wait_for"] = json!(go_a);
    let mut step_b = turn("should be cancelled");
    step_b["hold_file"] = json!(lock_b);
    step_b["sleep_ms"] = json!(60_000);
    let mut step_c = turn("will fail");
    step_c["hold_file"] = json!(lock_c);
    step_c["code"] = json!(17);
    step_c["stderr"] = json!("boom");
    fleet.script("agy", json!([step_a, step_b, step_c]));

    let mut server = fleet.server("claude");

    // Validation errors on await and cancel, including the 16-handle cap (P2 #7).
    let too_many_handles: Vec<String> = (0..17).map(|i| format!("h-{i}")).collect();
    for bad in [
        json!({}),
        json!({"handles": []}),
        json!({"handles": [""]}),
        json!({"handles": too_many_handles}),
        json!({"handles": ["ok"], "condition": "first"}),
        json!({"handles": ["ok"], "extra": true}),
    ] {
        let (body, is_error) = raw_call(&mut server, 1, "await", bad);
        assert!(is_error, "{body}");
        assert_eq!(body["failure"]["kind"], "invalid_request");
    }
    for bad in [
        json!({}),
        json!({"handle": ""}),
        json!({"handle": "x", "extra": 1}),
    ] {
        let (body, is_error) = raw_call(&mut server, 2, "cancel", bad);
        assert!(is_error, "{body}");
        assert_eq!(body["failure"]["kind"], "invalid_request");
    }

    // Start delegation A in the background.
    let (started_a, is_error) = raw_call(
        &mut server,
        10,
        "antigravity",
        json!({"prompt": "work a", "wait": false}),
    );
    assert!(!is_error, "{started_a}");
    assert_eq!(started_a["schema_version"], 1);
    assert_eq!(started_a["state"], "running");
    assert_eq!(started_a["provider"], "antigravity");
    let handle_a = started_a["handle"].as_str().unwrap().to_owned();
    assert_ne!(handle_a, "session");
    let held_a = lock_file(&lock_a);
    assert!(!is_free(&held_a));

    // Await before completion, then cancel the await request itself: delegation A keeps running.
    server.start_call(
        11,
        "await",
        json!({"handles": [handle_a], "condition": "all"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    server.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 11 }
    }));
    assert_eq!(
        text(&server.call(12, "test_echo", json!({"still_alive": true}))),
        r#"{"still_alive":true}"#
    );
    assert!(
        !is_free(&held_a),
        "cancelling await must not cancel the delegation"
    );

    // Unknown handles report unknown immediately; condition=any returns running + unknown at once.
    let (mixed, is_error) = raw_call(
        &mut server,
        13,
        "await",
        json!({"handles": [handle_a, "not-a-handle"], "condition": "any"}),
    );
    assert!(!is_error, "{mixed}");
    assert_eq!(mixed["condition"], "any");
    assert_eq!(mixed["satisfied"], true);
    assert_eq!(mixed["timed_out"], false);
    assert_eq!(mixed["results"][0]["state"], "running");
    assert_eq!(mixed["results"][0]["cancel_requested"], false);
    assert_eq!(mixed["results"][1]["state"], "unknown");

    // Release delegation A and await after completion.
    fs::write(&go_a, []).unwrap();
    let (done_a, is_error) = raw_call(
        &mut server,
        14,
        "await",
        json!({"handles": [handle_a], "condition": "all"}),
    );
    assert!(!is_error, "{done_a}");
    assert_eq!(done_a["satisfied"], true);
    assert_eq!(done_a["timed_out"], false);
    assert_eq!(done_a["results"][0]["state"], "finished");
    assert_eq!(
        done_a["results"][0]["result"]["reported"]["content"],
        "finished from background"
    );
    assert_eq!(
        done_a["results"][0]["result"]["reported"]["thread_id"],
        "session"
    );

    // Reading a result does not consume it, duplicate handles are deduplicated (P2 #7),
    // and cancel on a finished handle leaves it finished.
    let (again, _) = raw_call(
        &mut server,
        15,
        "await",
        json!({"handles": [handle_a, handle_a]}),
    );
    assert_eq!(again["results"].as_array().unwrap().len(), 1, "{again}");
    assert_eq!(again["results"][0]["state"], "finished");
    assert_eq!(
        again["results"][0]["result"]["reported"]["content"],
        "finished from background"
    );
    let (cancel_finished, is_error) =
        raw_call(&mut server, 16, "cancel", json!({"handle": handle_a}));
    assert!(!is_error, "{cancel_finished}");
    assert_eq!(cancel_finished["state"], "finished");
    assert_eq!(cancel_finished["cancel_requested"], false);

    // Unknown handle on cancel.
    let (cancel_unknown, is_error) =
        raw_call(&mut server, 17, "cancel", json!({"handle": "missing"}));
    assert!(!is_error, "{cancel_unknown}");
    assert_eq!(cancel_unknown["state"], "unknown");
    assert_eq!(cancel_unknown["cancel_requested"], false);

    // Start delegation B in the background and cancel it while running.
    let (started_b, is_error) = raw_call(
        &mut server,
        18,
        "antigravity",
        json!({"prompt": "work b", "wait": false}),
    );
    assert!(!is_error, "{started_b}");
    let handle_b = started_b["handle"].as_str().unwrap().to_owned();
    let held_b = lock_file(&lock_b);
    let (cancelled_b, is_error) = raw_call(&mut server, 19, "cancel", json!({"handle": handle_b}));
    assert!(!is_error, "{cancelled_b}");
    assert_eq!(cancelled_b["state"], "running");
    assert_eq!(cancelled_b["cancel_requested"], true);

    let (awaited_b, is_error) = raw_call(
        &mut server,
        20,
        "await",
        json!({"handles": [handle_b], "condition": "all"}),
    );
    assert!(!is_error, "{awaited_b}");
    assert_eq!(awaited_b["results"][0]["state"], "cancelled");
    assert_eq!(
        awaited_b["results"][0]["result"]["observed"]["failure"]["kind"],
        "cancellation"
    );
    wait_until("cancelled child lock was not released", || is_free(&held_b));

    // A background delegation that fails stores its failed envelope.
    let (started_c, _) = raw_call(
        &mut server,
        21,
        "antigravity",
        json!({"prompt": "work c", "wait": false}),
    );
    let handle_c = started_c["handle"].as_str().unwrap().to_owned();
    let (awaited_c, _) = raw_call(&mut server, 22, "await", json!({"handles": [handle_c]}));
    assert_eq!(awaited_c["results"][0]["state"], "failed");
    assert_eq!(
        awaited_c["results"][0]["result"]["observed"]["failure"]["kind"],
        "abnormal_exit"
    );

    // A handle belongs only to its creating server process.
    let mut other = fleet.server("claude");
    let (foreign, _) = raw_call(&mut other, 1, "await", json!({"handles": [handle_a]}));
    assert_eq!(foreign["results"][0]["state"], "unknown");
}

#[test]
fn await_expires_at_the_20_second_limit_without_cancelling_the_delegation() {
    let fleet = Fleet::new(&["agy"]);
    let lock = fleet.root.join("slow.lock");
    let go = fleet.root.join("slow.go");
    let mut step = turn("completed after wait limit");
    step["hold_file"] = json!(lock);
    step["wait_for"] = json!(go);
    fleet.script("agy", json!([step]));

    let mut server = fleet.server("claude");
    let (started, is_error) = raw_call(
        &mut server,
        1,
        "antigravity",
        json!({"prompt": "slow work", "wait": false}),
    );
    assert!(!is_error, "{started}");
    let handle = started["handle"].as_str().unwrap().to_owned();
    let held = lock_file(&lock);

    let began = Instant::now();
    server.start_call(2, "await", json!({"handles": [handle], "condition": "all"}));
    let frame = server
        .next_frame(Instant::now() + Duration::from_secs(30))
        .expect("await returns after 20 s limit");
    let waited = began.elapsed();
    assert_eq!(frame["id"], 2);
    let body: Value = serde_json::from_str(text(&frame)).unwrap();
    assert!(
        waited >= Duration::from_secs(19),
        "await returned too early: {waited:?}"
    );
    assert_eq!(body["satisfied"], false);
    assert_eq!(body["timed_out"], true);
    assert_eq!(body["wait_limit_ms"], 20_000);
    assert_eq!(body["results"][0]["state"], "running");
    assert!(
        !is_free(&held),
        "await timeout must not cancel an unfinished delegation"
    );

    fs::write(&go, []).unwrap();
    let (done, _) = raw_call(&mut server, 3, "await", json!({"handles": [handle]}));
    assert_eq!(done["satisfied"], true);
    assert_eq!(done["timed_out"], false);
    assert_eq!(done["results"][0]["state"], "finished");
    assert_eq!(
        done["results"][0]["result"]["reported"]["content"],
        "completed after wait limit"
    );
}

#[test]
fn fourth_concurrent_delegation_is_refused_as_busy_until_a_slot_frees() {
    let fleet = Fleet::new(&["agy"]);
    let mut steps = Vec::new();
    let mut locks = Vec::new();
    let mut releases = Vec::new();
    for i in 0..3 {
        let lock = fleet.root.join(format!("slot-{i}.lock"));
        let go = fleet.root.join(format!("slot-{i}.go"));
        let mut step = turn(&format!("done-{i}"));
        step["hold_file"] = json!(lock);
        step["wait_for"] = json!(go);
        steps.push(step);
        locks.push(lock);
        releases.push(go);
    }
    steps.push(turn("fourth-after-free"));
    fleet.script("agy", Value::Array(steps));

    let mut server = fleet.server("claude");
    let mut handles = Vec::new();
    for (i, lock) in locks.iter().enumerate() {
        let (started, is_error) = raw_call(
            &mut server,
            10 + i as i64,
            "antigravity",
            json!({"prompt": format!("task {i}"), "wait": false}),
        );
        assert!(!is_error, "{started}");
        handles.push(started["handle"].as_str().unwrap().to_owned());
        let _ = lock_file(lock);
    }

    // Both background and foreground 4th delegations fail immediately with busy.
    let busy_bg = call(
        &mut server,
        "antigravity",
        json!({"prompt": "too many bg", "wait": false}),
    );
    assert_eq!(busy_bg["observed"]["state"], "failed");
    assert_eq!(busy_bg["observed"]["failure"]["kind"], "busy");

    let busy_fg = call(
        &mut server,
        "antigravity",
        json!({"prompt": "too many fg", "wait": true}),
    );
    assert_eq!(busy_fg["observed"]["state"], "failed");
    assert_eq!(busy_fg["observed"]["failure"]["kind"], "busy");

    // Release one slot and await(any) until that slot completes.
    fs::write(&releases[0], []).unwrap();
    let (any_done, is_error) = raw_call(
        &mut server,
        20,
        "await",
        json!({"handles": handles, "condition": "any"}),
    );
    assert!(!is_error, "{any_done}");
    assert_eq!(any_done["satisfied"], true);
    assert_eq!(any_done["results"][0]["state"], "finished");

    // Now a new delegation is admitted and finishes.
    let admitted = call(&mut server, "antigravity", json!({"prompt": "slot free"}));
    assert_eq!(admitted["observed"]["state"], "finished");
    assert_eq!(admitted["reported"]["content"], "fourth-after-free");

    // Release remaining two and await(all).
    fs::write(&releases[1], []).unwrap();
    fs::write(&releases[2], []).unwrap();
    let (all_done, _) = raw_call(
        &mut server,
        21,
        "await",
        json!({"handles": [&handles[1], &handles[2]], "condition": "all"}),
    );
    assert_eq!(all_done["satisfied"], true);
    assert_eq!(all_done["results"][0]["state"], "finished");
    assert_eq!(all_done["results"][1]["state"], "finished");
}

#[test]
fn shutdown_stops_running_background_delegations_within_the_ten_second_limit() {
    let fleet = Fleet::new(&["agy"]);
    let lock_1 = fleet.root.join("bg1.lock");
    let lock_2 = fleet.root.join("bg2.lock");
    let mut step_1 = turn("never");
    step_1["hold_file"] = json!(lock_1);
    step_1["sleep_ms"] = json!(60_000);
    let mut step_2 = turn("never");
    step_2["hold_file"] = json!(lock_2);
    step_2["sleep_ms"] = json!(60_000);
    fleet.script("agy", json!([step_1, step_2]));

    let mut server = fleet.server("claude");
    for id in [1, 2] {
        let (started, is_error) = raw_call(
            &mut server,
            id,
            "antigravity",
            json!({"prompt": "long background", "wait": false}),
        );
        assert!(!is_error, "{started}");
    }
    let held_1 = lock_file(&lock_1);
    let held_2 = lock_file(&lock_2);
    assert!(!is_free(&held_1) && !is_free(&held_2));

    let (status, took) = server.finish();
    assert!(status.success(), "{status}: {}", server.stderr());
    assert!(
        took < Duration::from_secs(10),
        "shutdown with background delegations took {took:?}"
    );
    assert!(is_free(&held_1) && is_free(&held_2));
}

#[test]
fn retained_background_results_are_bounded_and_evict_the_oldest() {
    let fleet = Fleet::new(&["agy"]);
    let steps: Vec<Value> = (0..17).map(|i| turn(&format!("answer-{i}"))).collect();
    fleet.script("agy", Value::Array(steps));

    let mut server = fleet.server("claude");
    let mut handles = Vec::new();
    for i in 0..17 {
        let (started, is_error) = raw_call(
            &mut server,
            100 + i,
            "antigravity",
            json!({"prompt": format!("item {i}"), "wait": false}),
        );
        assert!(!is_error, "{started}");
        let handle = started["handle"].as_str().unwrap().to_owned();
        let (done, _) = raw_call(&mut server, 200 + i, "await", json!({"handles": [handle]}));
        assert_eq!(done["results"][0]["state"], "finished");
        handles.push(handle);
    }

    let (checked, _) = raw_call(
        &mut server,
        300,
        "await",
        json!({"handles": [&handles[0], &handles[1], &handles[16]], "condition": "all"}),
    );
    assert_eq!(checked["results"][0]["state"], "unknown");
    assert_eq!(checked["results"][1]["state"], "finished");
    assert_eq!(checked["results"][2]["state"], "finished");
}

#[test]
fn panicking_background_task_completes_handle_as_failed_and_frees_slot() {
    let fleet = Fleet::new(&["agy"]);
    fleet.script("agy", json!([turn("ok1"), turn("ok2"), turn("ok3")]));
    let mut server = fleet.server("claude");

    let (started, is_error) = raw_call(&mut server, 1, "test_panic", json!({"background": true}));
    assert!(!is_error, "{started}");
    let handle = started["handle"].as_str().unwrap().to_owned();

    let (awaited, is_error) = raw_call(
        &mut server,
        2,
        "await",
        json!({"handles": [handle], "condition": "all"}),
    );
    assert!(!is_error, "{awaited}");
    assert_eq!(awaited["satisfied"], true);
    assert_eq!(awaited["results"][0]["state"], "failed", "{awaited}");
    assert_eq!(
        awaited["results"][0]["result"]["observed"]["failure"]["kind"], "unknown",
        "{awaited}"
    );
    assert!(
        awaited["results"][0]["result"]["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("failed unexpectedly"),
        "{awaited}"
    );

    // Server remains healthy and all 3 concurrency slots are available.
    assert_eq!(
        text(&server.call(3, "test_echo", json!({"alive": true}))),
        r#"{"alive":true}"#
    );
    let mut handles = Vec::new();
    for i in 0..3 {
        let (bg, is_error) = raw_call(
            &mut server,
            10 + i,
            "antigravity",
            json!({"prompt": format!("after panic {i}"), "wait": false}),
        );
        assert!(!is_error, "{bg}");
        handles.push(bg["handle"].as_str().unwrap().to_owned());
    }
    let (done, _) = raw_call(
        &mut server,
        20,
        "await",
        json!({"handles": handles, "condition": "all"}),
    );
    assert_eq!(done["satisfied"], true);
    assert!(
        done["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["state"] == "finished"),
        "{done}"
    );
}

#[test]
fn cancelled_or_expired_wait_false_request_does_not_spawn_background_delegation() {
    const CODEX_FINISHED: &str = include_str!("../fixtures/codex/finished.jsonl");
    const CODEX_THREAD: &str = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";

    let fleet = Fleet::new(&["codex"]);
    fleet.script("codex", json!([{"stdout": CODEX_FINISHED}]));
    let mut seed = fleet.server("claude");
    assert_eq!(
        call(&mut seed, "codex", json!({"prompt": "seed"}))["observed"]["state"],
        "finished"
    );
    assert!(seed.finish().0.success());

    #[cfg(all(unix, not(target_os = "macos")))]
    let provider_dir = fleet.root.join(".local/state/proxenos/sessions/codex");
    #[cfg(target_os = "macos")]
    let provider_dir = fleet
        .root
        .join("Library/Application Support/proxenos/sessions/codex");
    #[cfg(windows)]
    let provider_dir = fleet.root.join("proxenos/sessions/codex");

    let session_file = fs::read_dir(&provider_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_file.with_extension("lock"))
        .unwrap();
    lock.lock().unwrap();

    let mut server = fleet.server("claude");
    let before = fleet.records().len();
    let timed = call(
        &mut server,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "expired wait false",
            "wait": false,
            "timeout_ms": 150
        }),
    );
    assert_eq!(timed["observed"]["failure"]["kind"], "timeout", "{timed}");

    server.start_call(
        50,
        "codex-reply",
        json!({
            "thread_id": CODEX_THREAD,
            "prompt": "cancelled wait false",
            "wait": false
        }),
    );
    std::thread::sleep(Duration::from_millis(100));
    server.close_input();
    let cancelled: Value = serde_json::from_str(text(&server.response(50))).unwrap();
    assert_eq!(cancelled["observed"]["state"], "cancelled", "{cancelled}");
    assert_eq!(
        cancelled["observed"]["failure"]["kind"], "cancellation",
        "{cancelled}"
    );
    assert_eq!(
        fleet.records().len(),
        before,
        "cancelled/expired wait:false must not spawn a provider"
    );

    drop(lock);
    assert!(server.finish().0.success());
}
