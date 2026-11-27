//! D2 top-level `failure` precedence, `authentication` fixed message, and `await` delivery.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, assert_observed_failure,
    assert_reported_failure, call,
};

const AGY_FINISHED: &str = include_str!("../fixtures/antigravity/finished.json");
const AGY_AUTH: &str = include_str!("../fixtures/antigravity/captured-auth-failed.stdout.json");
const CLAUDE_NOT_LOGGED_IN: &str =
    include_str!("../fixtures/claude/captured-not-logged-in.stdout.json");
const CLAUDE_QUOTA: &str = include_str!("../fixtures/claude/synthetic-quota-error.stdout.json");
const CODEX_MISSING_KEY_OUT: &str =
    include_str!("../fixtures/codex/captured-missing-provider-key.stdout.jsonl");
const CODEX_MISSING_KEY_ERR: &str =
    include_str!("../fixtures/codex/captured-missing-provider-key.stderr.txt");
const COPILOT_LOGIN_OUT: &str =
    include_str!("../fixtures/copilot/captured-missing-login.stdout.jsonl");
const COPILOT_LOGIN_ERR: &str =
    include_str!("../fixtures/copilot/captured-missing-login.stderr.txt");
const GROK_LOGIN_OUT: &str = include_str!("../fixtures/grok/captured-missing-login.stdout.json");
const GROK_LOGIN_ERR: &str = include_str!("../fixtures/grok/captured-missing-login.stderr.txt");
const KIMI_LOGIN_OUT: &str = include_str!("../fixtures/kimi/captured-missing-login.stdout.jsonl");
const KIMI_LOGIN_ERR: &str = include_str!("../fixtures/kimi/captured-missing-login.stderr.txt");
const KIMI_NO_ASSISTANT: &str =
    include_str!("../fixtures/kimi/synthetic-no-assistant.stdout.jsonl");

#[test]
fn top_level_failure_covers_authentication_across_all_six_providers_and_exit_zero_reports() {
    let fleet = Fleet::new(&["agy", "claude", "codex", "copilot", "grok", "kimi"]);

    // Synthetic authentication event for Codex (no captured Codex login stream in fixtures).
    let synthetic_codex_auth = json!({
        "type": "turn.failed",
        "error": {
            "message": json!({
                "error": {
                    "code": "invalid_api_key",
                    "message": "Synthetic Codex login failure: sk-should-not-surface"
                }
            })
            .to_string()
        }
    })
    .to_string();

    // Synthetic oversized (>16 KiB) Claude authentication error verifying top-level message stays fixed.
    let long_claude_auth = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "api_error_status": 401,
        "result": format!("Token rejected: {}", "x".repeat(16 << 10))
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {"stdout": AGY_FINISHED},
            {"stdout": AGY_AUTH, "code": 1}
        ]),
    );
    fleet.script(
        "claude",
        json!([
            {"stdout": CLAUDE_NOT_LOGGED_IN, "code": 1},
            {"stdout": long_claude_auth, "code": 1},
            {"stdout": CLAUDE_QUOTA, "code": 0}
        ]),
    );
    fleet.script(
        "codex",
        json!([
            {"stdout": synthetic_codex_auth, "code": 1},
            {"stdout": CODEX_MISSING_KEY_OUT, "stderr": CODEX_MISSING_KEY_ERR, "code": 1}
        ]),
    );
    fleet.script(
        "copilot",
        json!([{"stdout": COPILOT_LOGIN_OUT, "stderr": COPILOT_LOGIN_ERR, "code": 1}]),
    );
    fleet.script(
        "grok",
        json!([{"stdout": GROK_LOGIN_OUT, "stderr": GROK_LOGIN_ERR, "code": 1}]),
    );
    fleet.script(
        "kimi",
        json!([
            {"stdout": KIMI_LOGIN_OUT, "stderr": KIMI_LOGIN_ERR, "code": 1},
            {"stdout": KIMI_NO_ASSISTANT, "stderr": "401 Unauthorized", "code": 0}
        ]),
    );

    let mut server = fleet.server("cursor");

    // Success has top-level `failure: null`.
    let success = call(&mut server, "antigravity", json!({"prompt": "ok"}));
    assert_eq!(success["observed"]["state"], "finished", "{success}");
    assert_eq!(
        success.as_object().unwrap().get("failure"),
        Some(&Value::Null),
        "{success}"
    );

    // Non-zero exit with authentication across all 6 providers:
    // top-level `failure` has kind=authentication, source=reported, fixed message;
    // `observed.failure` keeps abnormal_exit; `reported.failure` and `reported.diagnostic` omit raw login text.
    for (tool, forbidden_in_envelope) in [
        ("antigravity", "Antigravity requires"),
        ("claude", "Please run /login"),
        ("codex", "sk-should-not-surface"),
        ("copilot", "No authentication information"),
        ("grok", "grok login"),
        ("kimi", "No model configured"),
    ] {
        let result = call(&mut server, tool, json!({"prompt": "auth"}));
        assert_reported_failure(
            &result,
            Some(("abnormal_exit", "exit code 1")),
            "authentication",
            AUTHENTICATION_FAILURE_MESSAGE,
        );
        let top_message = result["failure"]["message"].as_str().unwrap();
        assert_eq!(top_message, AUTHENTICATION_FAILURE_MESSAGE, "{result}");
        assert_eq!(
            result["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC,
            "{result}"
        );
        assert!(
            !result.to_string().contains(forbidden_in_envelope),
            "authentication envelope leaked provider text: {result}"
        );
    }

    // Oversized authentication error replaces reported.failure before archiving, so no failure_file is created.
    let long_auth = call(&mut server, "claude", json!({"prompt": "long auth"}));
    assert_reported_failure(
        &long_auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert_eq!(
        long_auth["failure"]["message"], AUTHENTICATION_FAILURE_MESSAGE,
        "{long_auth}"
    );
    assert!(
        long_auth["observed"]["failure_file"].is_null(),
        "{long_auth}"
    );
    assert!(
        !long_auth.to_string().contains("Token rejected"),
        "{long_auth}"
    );

    // Codex non-zero exit with non-auth captured failure keeps reported.failure over abnormal_exit.
    let codex_unknown = call(&mut server, "codex", json!({"prompt": "missing env"}));
    assert_reported_failure(
        &codex_unknown,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "PROXENOS_D5_EMPTY_TEST_KEY",
    );

    // Reported failures with exit 0 have observed.failure == null and source == "reported".
    let quota_exit_zero = call(&mut server, "claude", json!({"prompt": "quota exit 0"}));
    assert_reported_failure(&quota_exit_zero, None, "quota", "hit your limit");
    assert_eq!(
        quota_exit_zero["failure"]["reset_at"], "5pm (UTC)",
        "{quota_exit_zero}"
    );

    let auth_exit_zero = call(&mut server, "kimi", json!({"prompt": "auth exit 0"}));
    assert_reported_failure(
        &auth_exit_zero,
        None,
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(
        !auth_exit_zero.to_string().contains("401 Unauthorized"),
        "{auth_exit_zero}"
    );

    for cli in ["agy", "claude", "codex", "copilot", "grok", "kimi"] {
        fleet.assert_steps_remaining(cli, 0);
    }
}

fn wait_for_ready(path: &Path) {
    let until = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < until,
            "stub never created {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn timeout_while_report_is_non_empty_selects_observed_timeout_over_reported_failure() {
    let fleet = Fleet::new(&["agy"]);
    let ready_err = fleet.root.join("err-timeout.ready");
    let ready_ok = fleet.root.join("ok-timeout.ready");

    let error_report = json!({
        "conversation_id": "paper-crane",
        "status": "ERROR",
        "response": "",
        "error": "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z."
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {
                "stdout": error_report,
                "stderr": "hanging after report\n",
                "ready": ready_err,
                "sleep_ms": 60_000
            },
            {
                "stdout": AGY_FINISHED,
                "ready": ready_ok,
                "sleep_ms": 60_000
            }
        ]),
    );

    let mut server = fleet.server("claude");
    server.request(1, "tools/list", json!({}));

    server.start_call(
        10,
        "antigravity",
        json!({"prompt": "hang with error report", "timeout_ms": 1_500}),
    );
    wait_for_ready(&ready_err);
    let timed_err: Value = serde_json::from_str(text(&server.response(10))).unwrap();
    assert_eq!(timed_err["observed"]["state"], "failed", "{timed_err}");
    assert_eq!(
        timed_err["observed"]["failure"]["kind"], "timeout",
        "{timed_err}"
    );
    assert_eq!(
        timed_err["reported"]["thread_id"], "paper-crane",
        "{timed_err}"
    );
    assert_eq!(
        timed_err["reported"]["failure"]["kind"], "quota",
        "{timed_err}"
    );
    assert_eq!(
        timed_err["reported"]["diagnostic"], "hanging after report\n",
        "{timed_err}"
    );
    assert_eq!(timed_err["failure"]["kind"], "timeout", "{timed_err}");
    assert_eq!(timed_err["failure"]["source"], "observed", "{timed_err}");
    assert!(
        timed_err["failure"].get("reset_at").is_none(),
        "{timed_err}"
    );

    server.start_call(
        11,
        "antigravity",
        json!({"prompt": "hang with content report", "timeout_ms": 1_500}),
    );
    wait_for_ready(&ready_ok);
    let timed_ok: Value = serde_json::from_str(text(&server.response(11))).unwrap();
    assert_observed_failure(&timed_ok, "timeout", "exceeded the call's deadline");
    assert_eq!(
        timed_ok["reported"]["thread_id"], "paper-crane",
        "{timed_ok}"
    );
    assert_eq!(
        timed_ok["reported"]["content"], "The fold aligns with the mark.",
        "{timed_ok}"
    );

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn wait_false_calls_collected_through_await_carry_top_level_failure() {
    let fleet = Fleet::new(&["agy", "claude"]);
    let go_ok = fleet.root.join("bg-ok.go");
    let ready_auth = fleet.root.join("bg-auth.ready");
    let ready_timeout = fleet.root.join("bg-timeout.ready");
    let quota_json = json!({
        "conversation_id": "paper-crane",
        "status": "ERROR",
        "response": "",
        "error": "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z."
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {"stdout": AGY_FINISHED, "wait_for": go_ok},
            {"stdout": quota_json, "ready": ready_timeout, "sleep_ms": 60_000}
        ]),
    );
    fleet.script(
        "claude",
        json!([
            {"stdout": CLAUDE_NOT_LOGGED_IN, "ready": ready_auth, "code": 1},
            {"stdout": CLAUDE_QUOTA, "code": 0}
        ]),
    );

    let mut server = fleet.server("codex");
    server.request(1, "tools/list", json!({}));

    fn start_bg(server: &mut Server, id: i64, tool: &str, args: Value) -> String {
        let resp = server.call(id, tool, args);
        assert_ne!(resp["result"]["isError"], true, "{resp}");
        let body: Value = serde_json::from_str(text(&resp)).unwrap();
        assert_eq!(body["state"], "running", "{body}");
        body["handle"].as_str().unwrap().to_owned()
    }

    let h_ok = start_bg(
        &mut server,
        10,
        "antigravity",
        json!({"prompt": "ok", "wait": false}),
    );
    let h_auth = start_bg(
        &mut server,
        11,
        "claude",
        json!({"prompt": "auth", "wait": false}),
    );
    // Gate the second Claude background call on the first having consumed step 0 from claude.steps.
    wait_for_ready(&ready_auth);
    let h_quota = start_bg(
        &mut server,
        12,
        "claude",
        json!({"prompt": "quota", "wait": false}),
    );
    fs::write(&go_ok, []).unwrap();

    let awaited_first = server.call(
        13,
        "await",
        json!({"handles": [h_ok, h_auth, h_quota], "condition": "all"}),
    );
    assert_ne!(awaited_first["result"]["isError"], true, "{awaited_first}");
    let body_first: Value = serde_json::from_str(text(&awaited_first)).unwrap();
    let r_ok = &body_first["results"][0]["result"];
    let r_auth = &body_first["results"][1]["result"];
    let r_quota = &body_first["results"][2]["result"];

    assert_eq!(r_ok["observed"]["state"], "finished", "{r_ok}");
    assert_eq!(
        r_ok.as_object().unwrap().get("failure"),
        Some(&Value::Null),
        "{r_ok}"
    );
    assert_reported_failure(
        r_auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(!r_auth.to_string().contains("Not logged in"), "{r_auth}");
    assert_reported_failure(r_quota, None, "quota", "hit your limit");
    assert_eq!(r_quota["failure"]["reset_at"], "5pm (UTC)", "{r_quota}");

    // Also test a wait:false timeout while report is non-empty, collected via await.
    let h_timeout = start_bg(
        &mut server,
        14,
        "antigravity",
        json!({"prompt": "timeout", "wait": false, "timeout_ms": 1_500}),
    );
    wait_for_ready(&ready_timeout);
    let awaited_timeout = server.call(
        15,
        "await",
        json!({"handles": [h_timeout], "condition": "all"}),
    );
    let body_timeout: Value = serde_json::from_str(text(&awaited_timeout)).unwrap();
    let r_timeout = &body_timeout["results"][0]["result"];
    assert_eq!(r_timeout["observed"]["state"], "failed", "{r_timeout}");
    assert_eq!(
        r_timeout["observed"]["failure"]["kind"], "timeout",
        "{r_timeout}"
    );
    assert_eq!(
        r_timeout["reported"]["failure"]["kind"], "quota",
        "{r_timeout}"
    );
    assert_eq!(r_timeout["failure"]["kind"], "timeout", "{r_timeout}");
    assert_eq!(r_timeout["failure"]["source"], "observed", "{r_timeout}");

    assert!(fs::metadata(fleet.root.join("invocations")).is_ok());
    fleet.assert_steps_remaining("agy", 0);
    fleet.assert_steps_remaining("claude", 0);
}
