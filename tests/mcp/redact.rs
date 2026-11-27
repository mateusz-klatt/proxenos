//! Behavioural tests for shared authentication omission and heuristic failure redaction (D2).

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, assert_observed_failure,
    assert_reported_failure, call,
};

const AGY_AUTH: &str = include_str!("../fixtures/antigravity/captured-auth-failed.stdout.json");
const CLAUDE_NOT_LOGGED_IN: &str =
    include_str!("../fixtures/claude/captured-not-logged-in.stdout.json");
const COPILOT_LOGIN_OUT: &str =
    include_str!("../fixtures/copilot/captured-missing-login.stdout.jsonl");
const COPILOT_LOGIN_ERR: &str =
    include_str!("../fixtures/copilot/captured-missing-login.stderr.txt");
const GROK_LOGIN_OUT: &str = include_str!("../fixtures/grok/captured-missing-login.stdout.json");
const GROK_LOGIN_ERR: &str = include_str!("../fixtures/grok/captured-missing-login.stderr.txt");
const KIMI_LOGIN_OUT: &str = include_str!("../fixtures/kimi/captured-missing-login.stdout.jsonl");
const KIMI_LOGIN_ERR: &str = include_str!("../fixtures/kimi/captured-missing-login.stderr.txt");
const AGY_ID: &str = "paper-crane";

#[test]
fn authentication_omits_login_material_across_all_six_providers_and_background_await() {
    let fleet = Fleet::new(&["agy", "claude", "codex", "copilot", "grok", "kimi"]);

    // Synthetic authentication outputs carrying login URLs, device codes, and tokens.
    let synthetic_codex_auth = json!({
        "type": "turn.failed",
        "error": {
            "message": json!({
                "error": {
                    "code": "invalid_api_key",
                    "message": "Synthetic Codex login failure: sk-codex-secret-999"
                }
            })
            .to_string()
        }
    })
    .to_string();
    let synthetic_codex_stderr =
        "Visit https://auth.example.invalid/oauth/authorize?code=CODEX-AUTH to sign in.\n";
    let synthetic_copilot_stderr = format!(
        "{COPILOT_LOGIN_ERR}\nOpen https://github.com/login/device and enter code: COPILOT-1234\n"
    );
    let synthetic_long_claude_auth = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "api_error_status": 401,
        "result": format!(
            "Token rejected: https://login.example.invalid/auth?code=CLAUDE-SECRET sk-ant-secret {}",
            "x".repeat(16 << 10)
        )
    })
    .to_string();
    let timeout_ready = fleet.root.join("agy-auth-timeout.ready");

    fleet.script(
        "agy",
        json!([
            {
                "stdout": AGY_AUTH,
                "stderr": "Authentication required. Please visit the URL to log in:\n  https://agy.example.invalid/oauth?code=AGY-ONE-TIME\n",
                "code": 1
            },
            {
                "stdout": AGY_AUTH,
                "stderr": "Authentication required. Please visit the URL to log in:\n  https://agy.example.invalid/oauth?code=AGY-TIMED-OUT\n",
                "ready": timeout_ready,
                "sleep_ms": 60_000
            }
        ]),
    );
    fleet.script(
        "claude",
        json!([
            {
                "stdout": CLAUDE_NOT_LOGGED_IN,
                "stderr": "Run login at https://claude.example.invalid/login?session=CLAUDE-1",
                "code": 1
            },
            {
                "stdout": synthetic_long_claude_auth,
                "stderr": "Bearer synthetic-claude-bearer-123",
                "code": 1
            }
        ]),
    );
    fleet.script(
        "codex",
        json!([
            {"stdout": synthetic_codex_auth, "stderr": synthetic_codex_stderr, "code": 1},
            {"stdout": synthetic_codex_auth, "stderr": synthetic_codex_stderr, "code": 1}
        ]),
    );
    fleet.script(
        "copilot",
        json!([
            {"stdout": COPILOT_LOGIN_OUT, "stderr": synthetic_copilot_stderr, "code": 1},
            {"stdout": COPILOT_LOGIN_OUT, "stderr": synthetic_copilot_stderr, "code": 1}
        ]),
    );
    fleet.script(
        "grok",
        json!([{"stdout": GROK_LOGIN_OUT, "stderr": GROK_LOGIN_ERR, "code": 1}]),
    );
    fleet.script(
        "kimi",
        json!([{"stdout": KIMI_LOGIN_OUT, "stderr": KIMI_LOGIN_ERR, "code": 1}]),
    );

    let mut server = fleet.server("cursor");

    let cases: &[(&str, &[&str])] = &[
        (
            "antigravity",
            &[
                "https://agy.example.invalid",
                "AGY-ONE-TIME",
                "Antigravity requires sign-in",
            ],
        ),
        (
            "claude",
            &[
                "Not logged in",
                "Please run /login",
                "https://claude.example.invalid",
                "CLAUDE-1",
            ],
        ),
        (
            "codex",
            &[
                "sk-codex-secret-999",
                "https://auth.example.invalid",
                "CODEX-AUTH",
            ],
        ),
        (
            "copilot",
            &[
                "No authentication information found",
                "https://github.com/login/device",
                "COPILOT-1234",
                "COPILOT_GITHUB_TOKEN",
            ],
        ),
        (
            "grok",
            &["Not signed in", "grok login --device-code", "XAI_API_KEY"],
        ),
        (
            "kimi",
            &["No model configured", "/login to sign in", "kimi-code.log"],
        ),
    ];

    for &(tool, forbidden) in cases {
        let result = call(&mut server, tool, json!({"prompt": "auth check"}));
        assert_reported_failure(
            &result,
            Some(("abnormal_exit", "exit code 1")),
            "authentication",
            AUTHENTICATION_FAILURE_MESSAGE,
        );
        let serialized = result.to_string();
        for needle in forbidden {
            assert!(
                !serialized.contains(needle),
                "{tool} leaked {needle:?} in envelope: {serialized}"
            );
        }
    }

    // Oversized (>16 KiB) synthetic auth error must not create failure_file or leak login text.
    let long_auth = call(&mut server, "claude", json!({"prompt": "long auth"}));
    assert_reported_failure(
        &long_auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(
        long_auth["observed"]["failure_file"].is_null(),
        "oversized auth error must not be archived to disk: {long_auth}"
    );
    let long_serialized = long_auth.to_string();
    for needle in [
        "Token rejected",
        "https://login.example.invalid",
        "CLAUDE-SECRET",
        "sk-ant-secret",
        "synthetic-claude-bearer-123",
    ] {
        assert!(!long_serialized.contains(needle), "leaked {needle:?}");
    }

    // Background wait:false + await preserves fixed auth messages and omits login material.
    let start_bg = |server: &mut Server, id: i64, tool: &str| {
        let resp = server.call(id, tool, json!({"prompt": "bg auth", "wait": false}));
        assert_ne!(resp["result"]["isError"], true, "{resp}");
        let body: Value = serde_json::from_str(text(&resp)).unwrap();
        body["handle"].as_str().unwrap().to_owned()
    };
    let h_codex = start_bg(&mut server, 10, "codex");
    let h_copilot = start_bg(&mut server, 11, "copilot");
    let awaited = server.call(
        12,
        "await",
        json!({"handles": [h_codex, h_copilot], "condition": "all"}),
    );
    let awaited_raw = text(&awaited);
    for needle in [
        "sk-codex-secret-999",
        "https://auth.example.invalid",
        "CODEX-AUTH",
        "https://github.com/login/device",
        "COPILOT-1234",
    ] {
        assert!(
            !awaited_raw.contains(needle),
            "background await leaked {needle:?}: {awaited_raw}"
        );
    }
    let awaited_json: Value = serde_json::from_str(awaited_raw).unwrap();
    for index in 0..2 {
        let item = &awaited_json["results"][index]["result"];
        assert_reported_failure(
            item,
            Some(("abnormal_exit", "exit code 1")),
            "authentication",
            AUTHENTICATION_FAILURE_MESSAGE,
        );
    }

    // PR A precedence: observed timeout still wins top-level failure over reported authentication,
    // while reported.* still omits the sign-in URL and sets observed.redacted = true.
    server.start_call(
        13,
        "antigravity",
        json!({"prompt": "timeout auth", "timeout_ms": 1_500}),
    );
    let until = Instant::now() + WAIT;
    while !timeout_ready.exists() {
        assert!(Instant::now() < until, "timed agy stub never started");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let timed: Value = serde_json::from_str(text(&server.response(13))).unwrap();
    // `assert_observed_failure` requires an empty `reported.failure`, but here the stub prints an
    // authentication error before the deadline, so assert the observed and top-level halves
    // directly.
    assert_eq!(timed["observed"]["state"], "failed", "{timed}");
    assert_eq!(timed["observed"]["failure"]["kind"], "timeout", "{timed}");
    assert_eq!(timed["failure"]["kind"], "timeout", "{timed}");
    assert_eq!(timed["failure"]["source"], "observed", "{timed}");
    assert_eq!(timed["reported"]["failure"]["kind"], "authentication");
    assert_eq!(
        timed["reported"]["failure"]["message"],
        AUTHENTICATION_FAILURE_MESSAGE
    );
    assert_eq!(timed["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC);
    assert_eq!(timed["observed"]["redacted"], true, "{timed}");
    assert!(!timed.to_string().contains("AGY-TIMED-OUT"), "{timed}");

    for cli in ["agy", "claude", "codex", "copilot", "grok", "kimi"] {
        fleet.assert_steps_remaining(cli, 0);
    }
}

#[test]
fn non_auth_failures_redact_urls_device_codes_and_secret_prefixes_while_preserving_context() {
    let fleet = Fleet::new(&["agy", "codex", "kimi"]);

    // Synthetic non-auth failures exercising every heuristic shape across JSON errors, stderr,
    // reset_at, oversized archived failures, failed discovery probes, observed paths, and await.
    let synthetic_codex_json_error = json!({
        "type": "turn.failed",
        "error": {
            "message": concat!(
                "Prefix context: visit https://auth.example.invalid/oauth/start?client_id=abc ",
                "and http://127.0.0.1:8080/login. ",
                "Check (https://example.invalid/Device/Verify) or [https://example.invalid/session#step1] ",
                "or https://example.invalid/token or https://example.invalid/code ",
                "or https://example.invalid/foo_(bar)?x=1 or https://example.invalid/v1/status?q=1. ",
                "Copy one-time code: ABCD-1234, enter A1B2C3D4, use code 123456, ",
                "device_code=WXYZ-9876; ZYXW-4321 (enter this code), ",
                "copy code:\n  QWER9876Z. ",
                "Tokens: sk-proj-secret1, sk-ant-api03-secret2, ghp_secret3, gho_secret4, ",
                "ghu_secret5, ghs_secret6, github_pat_11AAA_bbb, xai-secret7, AIzaSyDemoKey8, ",
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig., ",
                "Bearer dGVzdA==, Bearer aBcDeF, Bearer MYSECRET, Bearer\ttok-123. Suffix context."
            )
        }
    })
    .to_string();

    let synthetic_stderr = concat!(
        "Stderr lead: open https://example.invalid/auth?code=999, ",
        "enter code EFGH-5678, key sk-stderr-secret. Stderr tail.\n"
    );

    let synthetic_long_error = json!({
        "status": "ERROR",
        "response": "",
        "error": format!(
            "Long failure lead: sk-long-secret-123 and https://example.invalid/device?k=1. {} Tail.",
            "p".repeat(16 << 10)
        )
    })
    .to_string();

    let synthetic_quota_with_secret_reset = "error: failed to run prompt: Quota exhausted. Try again at https://quota.example.invalid/status?token=leak123 or sk-quota-secret.\n";

    fleet.script(
        "codex",
        json!([
            {"stdout": synthetic_codex_json_error, "stderr": synthetic_stderr, "code": 1},
            {"stdout": synthetic_codex_json_error, "stderr": synthetic_stderr, "code": 1}
        ]),
    );
    fleet.script(
        "agy",
        json!([
            {"stdout": synthetic_long_error, "code": 1},
            {"stderr": "Plain error with https://example.invalid/docs?q=nonauth and code: UVWX-9999.", "code": 2}
        ]),
    );
    fleet.script(
        "kimi",
        json!([
            {"stderr": synthetic_quota_with_secret_reset, "code": 1}
        ]),
    );

    let mut server = fleet.server("claude");

    // 1. Foreground Codex non-auth failure with all URL/device-code/secret patterns in JSON & stderr.
    let fg = call(&mut server, "codex", json!({"prompt": "non-auth fg"}));
    assert_eq!(fg["observed"]["state"], "failed", "{fg}");
    assert_eq!(fg["observed"]["redacted"], true, "{fg}");
    assert_eq!(fg["failure"]["kind"], "unknown");
    assert_eq!(fg["failure"]["source"], "reported");
    let fg_msg = fg["reported"]["failure"]["message"].as_str().unwrap();
    let fg_diag = fg["reported"]["diagnostic"].as_str().unwrap();
    assert_eq!(fg["failure"]["message"], fg_msg);
    assert!(
        fg_msg.starts_with("Prefix context: visit [redacted] "),
        "{fg_msg}"
    );
    assert!(fg_msg.ends_with(" Suffix context."), "{fg_msg}");
    assert!(!fg_msg.contains("http"), "{fg_msg}");
    assert!(fg_msg.contains("([redacted])"), "{fg_msg}");
    assert!(fg_msg.contains("[[redacted]]"), "{fg_msg}");
    assert!(
        fg_msg.contains("code: [redacted], enter [redacted], use code [redacted]"),
        "{fg_msg}"
    );
    assert!(
        fg_msg.contains("device_code=[redacted]; [redacted] (enter this code)"),
        "{fg_msg}"
    );
    assert!(
        fg_msg
            .contains("Authorization: [redacted]., [redacted], [redacted], [redacted], [redacted]"),
        "{fg_msg}"
    );
    assert_eq!(
        fg_diag,
        "Stderr lead: open [redacted], enter code [redacted], key [redacted]. Stderr tail.\n"
    );

    // 2. Background wait:false + await redacts non-auth failures identically.
    let started = server.call(20, "codex", json!({"prompt": "non-auth bg", "wait": false}));
    let handle = serde_json::from_str::<Value>(text(&started)).unwrap()["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let awaited = server.call(21, "await", json!({"handles": [handle]}));
    let bg_item = &serde_json::from_str::<Value>(text(&awaited)).unwrap()["results"][0]["result"];
    assert_eq!(bg_item["observed"]["redacted"], true, "{bg_item}");
    assert_eq!(bg_item["reported"]["failure"]["message"], fg_msg);
    assert_eq!(bg_item["reported"]["diagnostic"], fg_diag);

    let fg_raw = fg.to_string();
    for leaked in [
        "auth.example.invalid",
        "127.0.0.1",
        "ABCD-1234",
        "A1B2C3D4",
        "123456",
        "WXYZ-9876",
        "ZYXW-4321",
        "QWER9876Z",
        "sk-proj-secret1",
        "sk-ant-api03-secret2",
        "ghp_secret3",
        "gho_secret4",
        "ghu_secret5",
        "ghs_secret6",
        "github_pat_11AAA_bbb",
        "xai-secret7",
        "AIzaSyDemoKey8",
        "eyJhbGci",
        "dGVzdA==",
        "aBcDeF",
        "MYSECRET",
        "tok-123",
        "EFGH-5678",
        "sk-stderr-secret",
    ] {
        assert!(!fg_raw.contains(leaked), "leaked {leaked:?} in {fg_raw}");
    }

    // 3. Oversized (>16 KiB) non-auth failure redacts BEFORE archiving to failure_file.
    let long_err = call(
        &mut server,
        "antigravity",
        json!({"prompt": "long non-auth"}),
    );
    assert_eq!(long_err["observed"]["redacted"], true, "{long_err}");
    let failure_file = PathBuf::from(long_err["observed"]["failure_file"].as_str().unwrap());
    let archived = fs::read_to_string(&failure_file).unwrap();
    assert!(
        archived.starts_with("Long failure lead: [redacted] and [redacted]. "),
        "{archived}"
    );
    assert!(archived.ends_with(" Tail."), "{archived}");
    assert!(!archived.contains("sk-long-secret-123"));
    assert!(!archived.contains("example.invalid/device"));

    // 4. Unclassified stderr-only non-zero exit redacts reported.diagnostic.
    let stderr_only = call(&mut server, "antigravity", json!({"prompt": "stderr only"}));
    assert_observed_failure(&stderr_only, "abnormal_exit", "exit code 2");
    assert_eq!(stderr_only["observed"]["redacted"], true, "{stderr_only}");
    assert_eq!(
        stderr_only["reported"]["diagnostic"],
        "Plain error with [redacted] and code: [redacted]."
    );

    // 5. Quota reset_at carrying a query URL or secret prefix is also redacted.
    let quota = call(&mut server, "kimi", json!({"prompt": "quota reset"}));
    assert_eq!(quota["observed"]["redacted"], true, "{quota}");
    assert_eq!(quota["failure"]["kind"], "quota");
    assert_eq!(quota["failure"]["reset_at"], "[redacted] or [redacted]");
    assert!(!quota.to_string().contains("token=leak123"), "{quota}");
    assert!(!quota.to_string().contains("sk-quota-secret"), "{quota}");

    // 6. Observed failure message carrying a matching secret token (via path) is also redacted.
    let obs_err = call(
        &mut server,
        "antigravity-reply",
        json!({
            "thread_id": AGY_ID,
            "prompt": "bad store",
            "cwd": fleet.root,
            "access": "full",
            "mode": "advisory",
            "model": "m"
        }),
    );
    assert_observed_failure(&obs_err, "invalid_request", "Cannot inspect");
    assert_eq!(obs_err["observed"]["redacted"], false, "{obs_err}");

    // Trigger observed.failure.message redaction with PROXENOS_AGY_CONVERSATIONS_DIR containing sk-...
    let mut cmd = fleet.command("claude");
    cmd.env(
        "PROXENOS_AGY_CONVERSATIONS_DIR",
        fleet.root.join("sk-obs-secret99"),
    );
    let mut obs_server = Server::start_command(cmd);
    obs_server.initialize("2025-11-25");
    let obs_redacted = call(
        &mut obs_server,
        "antigravity-reply",
        json!({
            "thread_id": AGY_ID,
            "prompt": "bad store redacted",
            "cwd": fleet.root,
            "access": "full",
            "mode": "advisory",
            "model": "m"
        }),
    );
    assert_eq!(obs_redacted["observed"]["redacted"], true, "{obs_redacted}");
    assert_eq!(obs_redacted["failure"]["kind"], "invalid_request");
    assert!(
        obs_redacted["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("[redacted]"),
        "{obs_redacted}"
    );
    assert!(
        !obs_redacted.to_string().contains("sk-obs-secret99"),
        "{obs_redacted}"
    );

    // 7. Failed discovery --version probe diagnostic is redacted centrally too.
    fs::write(
        fleet.root.join("codex.version"),
        json!({"stderr": "Probe failed: sk-probe-secret https://probe.invalid/login", "code": 3})
            .to_string(),
    )
    .unwrap();
    let mut probe_server = fleet.server("claude");
    let probe_failed = call(&mut probe_server, "codex", json!({"prompt": "probe"}));
    assert_observed_failure(&probe_failed, "launch", "exit code 3");
    assert_eq!(probe_failed["observed"]["redacted"], true, "{probe_failed}");
    assert_eq!(
        probe_failed["reported"]["diagnostic"],
        "Probe failed: [redacted] [redacted]"
    );

    for cli in ["agy", "codex", "kimi"] {
        fleet.assert_steps_remaining(cli, 0);
    }
}

#[test]
fn clean_diagnostics_and_non_auth_urls_are_not_over_redacted() {
    let fleet = Fleet::new(&["agy"]);
    let clean_stderr = concat!(
        "See https://example.invalid/support and https://docs.example.com/guide#overview.\n",
        "Ignore foohttps://example.com/auth, bare https://, and http:///local.\n",
        "Words: task-1234 disk-5678 bare sk- xai- AIza.\n",
        "Bearer prose: Invalid Bearer token; Bearer Token; bearer-only; the bearer of news; Bearer realm=\"example\".\n",
        "Non-codes: Exit code 1. ID ABCDEFGH; code? UVWX9999; code! QRST8888; encode ABCD-1234; ",
        "code: read-only; code: FOO-ABCD-1234; code: TOO1234567; status 2026-11-27; ",
        "code: AB12 and more than thirty-six bytes of filler words before NOTACODE followed by enough words before code.\n"
    );
    let clean_json = json!({
        "status": "ERROR",
        "response": "",
        "error": "Upstream returned https://status.example.invalid/incidents/42 (no query)."
    })
    .to_string();

    fleet.script(
        "agy",
        json!([{"stdout": clean_json, "stderr": clean_stderr, "code": 1}]),
    );
    let mut server = fleet.server("claude");
    let result = call(&mut server, "antigravity", json!({"prompt": "clean fail"}));
    assert_reported_failure(
        &result,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "https://status.example.invalid/incidents/42",
    );
    assert_eq!(result["observed"]["redacted"], false, "{result}");
    assert_eq!(result["reported"]["diagnostic"], clean_stderr, "{result}");
    assert!(
        !result.to_string().contains("[redacted]"),
        "unexpected redaction in clean diagnostic: {result}"
    );
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn answer_content_containing_urls_codes_and_secrets_is_unchanged() {
    let fleet = Fleet::new(&["agy"]);
    let short_answer = concat!(
        "Example login URL: https://example.invalid/oauth/authorize?code=KEEP-1234\n",
        "Example code: Enter code: ABCD-1234\n",
        "Example token: sk-ant-api03-keep-in-content Authorization: Bearer keep-token-99\n"
    );
    let long_answer = format!("{short_answer}{}", "a".repeat(16 << 10));
    let step = |content: &str| {
        json!({
            "stdout": json!({
                "status": "SUCCESS",
                "conversation_id": AGY_ID,
                "response": content
            })
            .to_string()
        })
    };
    fleet.script(
        "agy",
        json!([step(short_answer), step(&long_answer), step(short_answer)]),
    );
    let mut server = fleet.server("claude");

    let short = call(
        &mut server,
        "antigravity",
        json!({"prompt": "short answer"}),
    );
    assert_eq!(short["observed"]["state"], "finished", "{short}");
    assert_eq!(short["observed"]["redacted"], false, "{short}");
    assert_eq!(short["reported"]["content"], short_answer);

    let long = call(&mut server, "antigravity", json!({"prompt": "long answer"}));
    assert_eq!(long["observed"]["state"], "finished", "{long}");
    assert_eq!(long["observed"]["redacted"], false, "{long}");
    assert!(
        long["reported"]["content"]
            .as_str()
            .unwrap()
            .starts_with(short_answer),
        "{long}"
    );
    let content_file = PathBuf::from(long["observed"]["content_file"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&content_file).unwrap(), long_answer);

    let started = server.call(
        30,
        "antigravity",
        json!({"prompt": "bg answer", "wait": false}),
    );
    let handle = serde_json::from_str::<Value>(text(&started)).unwrap()["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let awaited = server.call(31, "await", json!({"handles": [handle]}));
    let bg = &serde_json::from_str::<Value>(text(&awaited)).unwrap()["results"][0]["result"];
    assert_eq!(bg["observed"]["state"], "finished", "{bg}");
    assert_eq!(bg["observed"]["redacted"], false, "{bg}");
    assert_eq!(bg["reported"]["content"], short_answer);

    fleet.assert_steps_remaining("agy", 0);
}
