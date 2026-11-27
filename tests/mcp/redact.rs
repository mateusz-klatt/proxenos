//! Behavioural tests for shared authentication omission and heuristic failure redaction (D2).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, WITHHELD_DIAGNOSTIC,
    WITHHELD_FAILURE_MESSAGE, WITHHELD_OBSERVED_FAILURE_MESSAGE, WITHHELD_PROBE_VERSION,
    WITHHELD_SESSION_SAVE_MESSAGE, assert_observed_failure, assert_reported_failure, call,
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
fn all_detector_patterns_trigger_all_or_nothing_withholding_without_leaks() {
    let fleet = Fleet::new(&["agy", "codex"]);

    let codex_error = |message: &str| {
        json!({
            "stdout": json!({
                "type": "turn.failed",
                "error": { "message": message }
            })
            .to_string(),
            "stderr": "Stderr detail\n",
            "code": 1
        })
    };

    // Exercise each detector shape in its own message because first-hit detection short-circuits on
    // the first matching token and withholds the entire Report.
    let secret_cases: &[(&str, &str, &str)] = &[
        (
            "https auth",
            "Prefix context: visit https://auth.example.invalid/oauth/start?client_id=abc.",
            "auth.example.invalid",
        ),
        (
            "http login",
            "Visit http://127.0.0.1:8080/login to proceed.",
            "127.0.0.1",
        ),
        (
            "non-ascii query url",
            "See https://example.invalid/café?code=NA1 now.",
            "café?code=NA1",
        ),
        (
            "non-ascii host url",
            "See https://exämple.invalid/login now.",
            "exämple.invalid",
        ),
        (
            "non-ascii path auth url",
            "See https://example.invalid/üser/auth now.",
            "üser/auth",
        ),
        (
            "paren url",
            "Check (https://example.invalid/Device/Verify) today.",
            "Device/Verify",
        ),
        (
            "bracket session fragment url",
            "Check [https://example.invalid/session#step1] today.",
            "session#step1",
        ),
        (
            "token path url",
            "Open https://example.invalid/token now.",
            "/token",
        ),
        (
            "code path url",
            "Open https://example.invalid/code now.",
            "/code",
        ),
        (
            "balanced parens query url",
            "Open https://example.invalid/foo_(bar)?x=1 now.",
            "foo_(bar)",
        ),
        (
            "balanced brackets query url",
            "Open https://example.invalid/foo_[bar]?x=1 now.",
            "foo_[bar]",
        ),
        (
            "query only url",
            "Open https://example.invalid/v1/status?q=1.",
            "status?q=1",
        ),
        ("sk-", "Token sk-proj-secret1 rejected.", "sk-proj-secret1"),
        (
            "sk-ant-",
            "Token sk-ant-api03-secret2 rejected.",
            "sk-ant-api03-secret2",
        ),
        ("ghp_", "Token ghp_secret3 rejected.", "ghp_secret3"),
        ("gho_", "Token gho_secret4 rejected.", "gho_secret4"),
        ("ghu_", "Token ghu_secret5 rejected.", "ghu_secret5"),
        ("ghs_", "Token ghs_secret6 rejected.", "ghs_secret6"),
        ("ghr_", "Token ghr_secret9 rejected.", "ghr_secret9"),
        (
            "github_pat_",
            "Token github_pat_11AAA_bbb rejected.",
            "github_pat_11AAA_bbb",
        ),
        ("xai-", "Token xai-secret7 rejected.", "xai-secret7"),
        ("AIza", "Token AIzaSyDemoKey8 rejected.", "AIzaSyDemoKey8"),
        (
            "bearer jwt",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig.",
            "eyJhbGci",
        ),
        ("bearer pad", "Header Bearer dGVzdA== rejected.", "dGVzdA=="),
        (
            "bearer mixed case",
            "Header Bearer aBcDeF rejected.",
            "aBcDeF",
        ),
        (
            "bearer upper",
            "Header Bearer MYSECRET rejected.",
            "MYSECRET",
        ),
        ("bearer tab", "Header Bearer\ttok-123 rejected.", "tok-123"),
        (
            "bearer long opaque",
            "Header Bearer abcdefghijklmnop rejected.",
            "abcdefghijklmnop",
        ),
        (
            "device 4-4 code",
            "Copy one-time code: ABCD-1234.",
            "ABCD-1234",
        ),
        (
            "device 8-char enter",
            "Please enter A1B2C3D4 now.",
            "A1B2C3D4",
        ),
        ("device 6-digit code", "Use code 123456 now.", "123456"),
        (
            "device next-line after cue",
            "Copy code:\n  QWER9876Z.\n",
            "QWER9876Z",
        ),
        (
            "device same-paragraph after cue",
            "Open link and enter your verification code.\n  BNMK-4567\n",
            "BNMK-4567",
        ),
        (
            "device same-line before cue",
            "ZYXW-4321 (enter this code).\n",
            "ZYXW-4321",
        ),
        (
            "device same-paragraph before cue",
            "HJKL-8910\nPlease paste this verification code.\n",
            "HJKL-8910",
        ),
        (
            "documented conservative code trigger",
            "Documented trade-off: error code EACCES.\n",
            "EACCES",
        ),
    ];

    let mut codex_steps: Vec<Value> = secret_cases
        .iter()
        .map(|(_, msg, _)| codex_error(msg))
        .collect();
    // Add one extra copy of the first secret case for wait:false + await verification.
    codex_steps.push(codex_error(secret_cases[0].1));

    fleet.script("codex", json!(codex_steps));
    fleet.script(
        "agy",
        json!([
            {"stderr": "Plain error with https://example.invalid/docs?q=nonauth and code: UVWX-9999.", "code": 2}
        ]),
    );

    let mut server = fleet.server("claude");

    for &(label, _, forbidden) in secret_cases {
        let res = call(&mut server, "codex", json!({"prompt": label}));
        assert_eq!(res["observed"]["state"], "failed", "{label}: {res}");
        assert_eq!(res["observed"]["redacted"], true, "{label}: {res}");
        assert_eq!(res["failure"]["kind"], "unknown", "{label}: {res}");
        assert_eq!(res["failure"]["source"], "reported", "{label}: {res}");
        assert_eq!(
            res["reported"]["failure"]["message"], WITHHELD_FAILURE_MESSAGE,
            "{label}: {res}"
        );
        assert_eq!(
            res["failure"]["message"], WITHHELD_FAILURE_MESSAGE,
            "{label}: {res}"
        );
        assert_eq!(
            res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC,
            "{label}: {res}"
        );
        let raw = res.to_string();
        assert!(
            !raw.contains(forbidden),
            "{label} leaked {forbidden:?} in {raw}"
        );
    }

    // Background wait:false + await withholds identically across the whole envelope.
    let started = server.call(20, "codex", json!({"prompt": "non-auth bg", "wait": false}));
    let handle = serde_json::from_str::<Value>(text(&started)).unwrap()["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let awaited = server.call(21, "await", json!({"handles": [handle]}));
    let awaited_raw = text(&awaited);
    let bg_item = &serde_json::from_str::<Value>(awaited_raw).unwrap()["results"][0]["result"];
    assert_eq!(bg_item["observed"]["redacted"], true, "{bg_item}");
    assert_eq!(
        bg_item["reported"]["failure"]["message"],
        WITHHELD_FAILURE_MESSAGE
    );
    assert_eq!(bg_item["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(
        !awaited_raw.contains("auth.example.invalid"),
        "background await leaked URL: {awaited_raw}"
    );

    // Unclassified stderr-only non-zero exit withholds reported.diagnostic.
    let stderr_only = call(&mut server, "antigravity", json!({"prompt": "stderr only"}));
    assert_observed_failure(&stderr_only, "abnormal_exit", "exit code 2");
    assert_eq!(stderr_only["observed"]["redacted"], true, "{stderr_only}");
    assert_eq!(stderr_only["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(
        !stderr_only.to_string().contains("UVWX-9999"),
        "{stderr_only}"
    );

    // Observed failure message: clean passes verbatim; detector hit replaces the whole field.
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
            "prompt": "bad store withheld",
            "cwd": fleet.root,
            "access": "full",
            "mode": "advisory",
            "model": "m"
        }),
    );
    assert_eq!(obs_redacted["observed"]["redacted"], true, "{obs_redacted}");
    assert_eq!(obs_redacted["failure"]["kind"], "invalid_request");
    assert_eq!(
        obs_redacted["failure"]["message"],
        WITHHELD_OBSERVED_FAILURE_MESSAGE
    );
    assert_eq!(
        obs_redacted["observed"]["failure"]["message"],
        WITHHELD_OBSERVED_FAILURE_MESSAGE
    );
    assert!(
        !obs_redacted.to_string().contains("sk-obs-secret99"),
        "{obs_redacted}"
    );
    assert!(obs_server.finish().0.success());

    // Failed discovery --version probe diagnostic is withheld with the fixed sentence.
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
    assert_eq!(probe_failed["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(
        !probe_failed.to_string().contains("sk-probe-secret")
            && !probe_failed.to_string().contains("probe.invalid"),
        "{probe_failed}"
    );

    assert!(server.finish().0.success());
    let server_stderr = server.stderr();
    for &(_, _, forbidden) in secret_cases {
        assert!(
            !server_stderr.contains(forbidden),
            "server stderr leaked {forbidden:?}: {server_stderr}"
        );
    }

    for cli in ["agy", "codex"] {
        fleet.assert_steps_remaining(cli, 0);
    }
}

#[test]
fn all_six_providers_withhold_on_hit_and_preserve_verbatim_on_no_hit() {
    let fleet = Fleet::new(&["agy", "claude", "codex", "copilot", "grok", "kimi"]);

    let agy_oversized_hit = json!({
        "status": "ERROR",
        "response": "",
        "error": format!(
            "Long lead: sk-agy-long and https://agy.example.invalid/device?k=1. {} Archive tail: sk-agy-arch.",
            "p".repeat(17 << 10)
        )
    })
    .to_string();
    let agy_oversized_clean = format!("Clean quota error: {}", "q".repeat(17 << 10));
    let agy_oversized_clean_json = json!({
        "status": "ERROR",
        "response": "",
        "error": agy_oversized_clean
    })
    .to_string();

    let claude_hit = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "result": "Limit hit · resets at 5pm (UTC).",
        "api_error_type": "rate_limit_error"
    })
    .to_string();
    let claude_clean = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "result": "You've hit your limit · resets 5pm (UTC).",
        "api_error_type": "rate_limit_error"
    })
    .to_string();

    let codex_structured_quota_hit = json!({
        "type": "turn.failed",
        "error": {
            "message": json!({
                "error": {
                    "type": "insufficient_quota",
                    "resets_at": "2030-01-01T00:00:00Z",
                    "message": "Quota exceeded; visit https://codex.example.invalid/login?code=CDX-1."
                }
            })
            .to_string()
        }
    })
    .to_string();
    let codex_structured_reset_code_hit = json!({
        "type": "turn.failed",
        "error": {
            "message": json!({
                "error": {
                    "type": "insufficient_quota",
                    "resets_at": "ABCD-1234",
                    "message": "Quota exceeded."
                }
            })
            .to_string()
        }
    })
    .to_string();
    let codex_clean = json!({
        "type": "turn.failed",
        "error": {
            "message": json!({
                "error": {
                    "type": "insufficient_quota",
                    "resets_at": "2030-01-01T00:00:00Z",
                    "message": "Monthly budget reached."
                }
            })
            .to_string()
        }
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {
                "stdout": json!({
                    "status": "ERROR",
                    "response": "",
                    "error": "Upstream failure at https://agy.example.invalid/oauth?code=AGY-H"
                })
                .to_string(),
                "stderr": "Stderr token sk-agy-stderr\n",
                "code": 1
            },
            {"stdout": agy_oversized_hit, "stderr": "Clean agy stderr\n", "code": 1},
            {
                "stdout": json!({
                    "status": "ERROR",
                    "response": "",
                    "error": "Upstream maintenance window."
                })
                .to_string(),
                "stderr": "Clean agy diagnostic\n",
                "code": 1
            },
            {"stdout": agy_oversized_clean_json, "code": 1}
        ]),
    );
    fleet.script(
        "claude",
        json!([
            {
                "stdout": claude_hit,
                "stderr": "Auth hint https://claude.example.invalid/device\n",
                "code": 1
            },
            {
                "stdout": claude_clean,
                "stderr": "Clean claude diagnostic\n",
                "code": 1
            }
        ]),
    );
    fleet.script(
        "codex",
        json!([
            {"stdout": codex_structured_quota_hit, "stderr": "Clean codex stderr\n", "code": 1},
            {"stdout": codex_structured_reset_code_hit, "code": 1},
            {"stdout": codex_clean, "stderr": "Clean codex diagnostic\n", "code": 1}
        ]),
    );
    fleet.script(
        "copilot",
        json!([
            {
                "stdout": "{\"type\":\"result\",\"sessionId\":\"76655224-21a2-4670-925b-2681c62f95a3\",\"exitCode\":0}\n",
                "stderr": "Login url https://github.com/login/device and enter code: COP-9999\n",
                "code": 0
            },
            {"stderr": "Clean copilot diagnostic\n", "code": 2}
        ]),
    );
    fleet.script(
        "grok",
        json!([
            {
                "stdout": json!({"type": "error", "message": "Grok failed: xai-grok-secret"}).to_string(),
                "stderr": "Header Bearer grok-opaque-token\n",
                "code": 1
            },
            {
                "stdout": json!({"type": "error", "message": "Grok upstream unavailable."}).to_string(),
                "stderr": "Clean grok diagnostic\n",
                "code": 1
            }
        ]),
    );
    fleet.script(
        "kimi",
        json!([
            {
                "stderr": "Quota exhausted for sk-kimi-secret; try again at 2030-06-01T09:00:00Z.\n",
                "code": 1
            },
            {
                "stderr": "Quota exhausted; try again at 2030-06-01T09:00:00Z.\n",
                "code": 1
            }
        ]),
    );

    let mut server = fleet.server("cursor");

    // 1. Antigravity: detector hit + oversized hit never writes secrets to archive, clean preserves
    // verbatim + clean oversized writes original text to archive.
    let agy_hit = call(&mut server, "antigravity", json!({"prompt": "agy hit"}));
    assert_reported_failure(
        &agy_hit,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert_eq!(agy_hit["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert_eq!(agy_hit["observed"]["redacted"], true, "{agy_hit}");
    assert!(agy_hit["observed"]["failure_file"].is_null());
    assert!(
        !agy_hit.to_string().contains("AGY-H") && !agy_hit.to_string().contains("sk-agy-stderr")
    );

    let agy_long_hit = call(
        &mut server,
        "antigravity",
        json!({"prompt": "agy long hit"}),
    );
    assert_reported_failure(
        &agy_long_hit,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        WITHHELD_FAILURE_MESSAGE,
    );
    // Decision is report-wide: clean stderr diagnostic becomes withheld too.
    assert_eq!(agy_long_hit["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert_eq!(agy_long_hit["observed"]["redacted"], true);
    assert!(agy_long_hit["observed"]["failure_file"].is_null());
    assert!(
        !agy_long_hit.to_string().contains("sk-agy-long")
            && !agy_long_hit.to_string().contains("sk-agy-arch")
    );

    let agy_ok = call(&mut server, "antigravity", json!({"prompt": "agy clean"}));
    assert_reported_failure(
        &agy_ok,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "Upstream maintenance window.",
    );
    assert_eq!(agy_ok["reported"]["diagnostic"], "Clean agy diagnostic\n");
    assert_eq!(agy_ok["observed"]["redacted"], false);

    let agy_long_ok = call(
        &mut server,
        "antigravity",
        json!({"prompt": "agy long clean"}),
    );
    assert_eq!(agy_long_ok["observed"]["redacted"], false, "{agy_long_ok}");
    let clean_archive =
        fs::read_to_string(agy_long_ok["observed"]["failure_file"].as_str().unwrap()).unwrap();
    assert_eq!(clean_archive, agy_oversized_clean);

    // 2. Claude: hit only in stderr still taints the whole quota Report (including text reset_at),
    // while clean stays verbatim with text-derived reset_at.
    let claude_hit_res = call(&mut server, "claude", json!({"prompt": "claude hit"}));
    assert_reported_failure(
        &claude_hit_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert_eq!(
        claude_hit_res["reported"]["diagnostic"],
        WITHHELD_DIAGNOSTIC
    );
    assert!(claude_hit_res["reported"]["failure"]["reset_at"].is_null());
    assert!(claude_hit_res["failure"]["reset_at"].is_null());
    assert_eq!(claude_hit_res["observed"]["redacted"], true);
    assert!(
        !claude_hit_res
            .to_string()
            .contains("claude.example.invalid")
    );

    let claude_ok_res = call(&mut server, "claude", json!({"prompt": "claude clean"}));
    assert_reported_failure(
        &claude_ok_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "You've hit your limit · resets 5pm (UTC).",
    );
    assert_eq!(
        claude_ok_res["reported"]["failure"]["reset_at"],
        "5pm (UTC)"
    );
    assert_eq!(
        claude_ok_res["reported"]["diagnostic"],
        "Clean claude diagnostic\n"
    );
    assert_eq!(claude_ok_res["observed"]["redacted"], false);

    // 3. Codex: structured JSON resets_at stays on a tainted report when clean, is removed when it
    // carries a code itself, and clean reports pass verbatim.
    let codex_hit_res = call(&mut server, "codex", json!({"prompt": "codex hit"}));
    assert_reported_failure(
        &codex_hit_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert_eq!(
        codex_hit_res["reported"]["failure"]["reset_at"],
        "2030-01-01T00:00:00Z"
    );
    assert_eq!(codex_hit_res["failure"]["reset_at"], "2030-01-01T00:00:00Z");
    assert_eq!(codex_hit_res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert_eq!(codex_hit_res["observed"]["redacted"], true);
    assert!(!codex_hit_res.to_string().contains("codex.example.invalid"));

    let codex_reset_code = call(&mut server, "codex", json!({"prompt": "codex reset code"}));
    assert_reported_failure(
        &codex_reset_code,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert!(codex_reset_code["reported"]["failure"]["reset_at"].is_null());
    assert!(codex_reset_code["failure"]["reset_at"].is_null());
    assert_eq!(codex_reset_code["observed"]["redacted"], true);
    assert!(!codex_reset_code.to_string().contains("ABCD-1234"));

    let codex_ok_res = call(&mut server, "codex", json!({"prompt": "codex clean"}));
    assert_reported_failure(
        &codex_ok_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Monthly budget reached.",
    );
    assert_eq!(
        codex_ok_res["reported"]["failure"]["reset_at"],
        "2030-01-01T00:00:00Z"
    );
    assert_eq!(
        codex_ok_res["reported"]["diagnostic"],
        "Clean codex diagnostic\n"
    );
    assert_eq!(codex_ok_res["observed"]["redacted"], false);

    // 4. Copilot: stderr hit taints both reported.failure and reported.diagnostic; clean stays verbatim.
    let copilot_hit_res = call(&mut server, "copilot", json!({"prompt": "copilot hit"}));
    assert_reported_failure(&copilot_hit_res, None, "unknown", WITHHELD_FAILURE_MESSAGE);
    assert_eq!(
        copilot_hit_res["reported"]["diagnostic"],
        WITHHELD_DIAGNOSTIC
    );
    assert_eq!(copilot_hit_res["observed"]["redacted"], true);
    assert!(
        !copilot_hit_res.to_string().contains("COP-9999")
            && !copilot_hit_res
                .to_string()
                .contains("github.com/login/device")
    );

    let copilot_ok_res = call(&mut server, "copilot", json!({"prompt": "copilot clean"}));
    assert_observed_failure(&copilot_ok_res, "abnormal_exit", "exit code 2");
    assert_eq!(
        copilot_ok_res["reported"]["diagnostic"],
        "Clean copilot diagnostic\n"
    );
    assert_eq!(copilot_ok_res["observed"]["redacted"], false);

    // 5. Grok: hit withholds message and diagnostic; clean passes verbatim.
    let grok_hit_res = call(&mut server, "grok", json!({"prompt": "grok hit"}));
    assert_reported_failure(
        &grok_hit_res,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert_eq!(grok_hit_res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert_eq!(grok_hit_res["observed"]["redacted"], true);
    assert!(
        !grok_hit_res.to_string().contains("xai-grok-secret")
            && !grok_hit_res.to_string().contains("grok-opaque-token")
    );

    let grok_ok_res = call(&mut server, "grok", json!({"prompt": "grok clean"}));
    assert_reported_failure(
        &grok_ok_res,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "Grok upstream unavailable.",
    );
    assert_eq!(
        grok_ok_res["reported"]["diagnostic"],
        "Clean grok diagnostic\n"
    );
    assert_eq!(grok_ok_res["observed"]["redacted"], false);

    // 6. Kimi: hit withholds failure & diagnostic and removes text-derived reset_at; clean passes verbatim.
    let kimi_hit_res = call(&mut server, "kimi", json!({"prompt": "kimi hit"}));
    assert_reported_failure(
        &kimi_hit_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        WITHHELD_FAILURE_MESSAGE,
    );
    assert_eq!(kimi_hit_res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(kimi_hit_res["reported"]["failure"]["reset_at"].is_null());
    assert!(kimi_hit_res["failure"]["reset_at"].is_null());
    assert_eq!(kimi_hit_res["observed"]["redacted"], true);
    let kimi_hit_raw = kimi_hit_res.to_string();
    assert!(
        !kimi_hit_raw.contains("sk-kimi-secret") && !kimi_hit_raw.contains("2030-06-01T09:00:00Z"),
        "{kimi_hit_raw}"
    );

    let kimi_ok_res = call(&mut server, "kimi", json!({"prompt": "kimi clean"}));
    assert_reported_failure(
        &kimi_ok_res,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Quota exhausted; try again at 2030-06-01T09:00:00Z.",
    );
    assert_eq!(
        kimi_ok_res["reported"]["failure"]["reset_at"],
        "2030-06-01T09:00:00Z"
    );
    assert_eq!(kimi_ok_res["observed"]["redacted"], false);

    for cli in ["agy", "claude", "codex", "copilot", "grok", "kimi"] {
        fleet.assert_steps_remaining(cli, 0);
    }
}

#[test]
fn claude_truncated_stderr_and_quota_reset_at_with_code_withhold_text_and_archive() {
    let fleet = Fleet::new(&["claude"]);

    // Codex (a): empty stdout + >4 MiB truncated stderr parsed into a quota failure, with the
    // device code near the 4 MiB boundary and its "Enter" cue pushed beyond the capture limit.
    let truncated_quota_stderr_with_code = format!(
        "Rate limit reached; resets at 2030-04-02T15:00:00Z. {} ABCD-1234\nEnter this verification code",
        "p".repeat((4 << 20) - 64),
    );
    assert!(truncated_quota_stderr_with_code.len() > (4 << 20));

    // Clean truncated stderr on a failed turn is also withheld conservatively.
    let truncated_clean_quota_stderr =
        format!("Usage limit reached; {}", "q".repeat((4 << 20) + 128),);

    // Codex (b): quota reset_at sliced from the suffix loses the earlier cue.
    let reset_same_paragraph_json = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "result": "You've hit your limit. Enter your verification code · resets at ABCD-1234.",
        "api_error_type": "rate_limit_error"
    })
    .to_string();
    let reset_split_paragraph_json = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": "76655224-21a2-4670-925b-2681c62f95a3",
        "result": "You've hit your limit.\n\nPlease enter code in browser.\n\nQuota resets at WXYZ-5678.",
        "api_error_type": "rate_limit_error"
    })
    .to_string();

    fleet.script(
        "claude",
        json!([
            {"stderr": truncated_quota_stderr_with_code, "code": 1},
            {"stderr": truncated_clean_quota_stderr, "code": 1},
            {"stdout": reset_same_paragraph_json, "code": 1},
            {"stdout": reset_split_paragraph_json, "code": 1}
        ]),
    );
    let mut server = fleet.server("codex");

    for (label, forbidden) in [
        (
            "truncated stderr quota with boundary code",
            Some("ABCD-1234"),
        ),
        ("truncated clean stderr quota", None),
        ("reset_at same paragraph with code", Some("ABCD-1234")),
        ("reset_at split paragraph with code", Some("WXYZ-5678")),
    ] {
        let res = call(&mut server, "claude", json!({"prompt": label}));
        assert_reported_failure(
            &res,
            Some(("abnormal_exit", "exit code 1")),
            "quota",
            WITHHELD_FAILURE_MESSAGE,
        );
        assert_eq!(res["observed"]["redacted"], true, "{label}: {res}");
        assert!(
            res["reported"]["failure"]["reset_at"].is_null(),
            "{label}: {res}"
        );
        assert!(res["failure"]["reset_at"].is_null(), "{label}: {res}");
        assert!(
            res["observed"]["failure_file"].is_null(),
            "{label} must not write an archive file for withheld failure: {res}"
        );
        if res["reported"]["diagnostic"].is_string() {
            assert_eq!(
                res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC,
                "{label}: {res}"
            );
        }
        let raw = res.to_string();
        assert!(!raw.contains("2030-04-02T15:00:00Z"), "{label}: {raw}");
        if let Some(code) = forbidden {
            assert!(!raw.contains(code), "{label} leaked {code}: {raw}");
        }
    }

    fleet.assert_steps_remaining("claude", 0);
}

#[test]
fn clean_diagnostics_and_non_auth_urls_are_not_over_redacted() {
    let fleet = Fleet::new(&["agy"]);
    let far = "f".repeat(156);
    let clean_stderr = format!(
        concat!(
            "See https://example.invalid/support, https://example.invalid/café, and https://docs.example.com/guide#overview.\n",
            "Ignore foohttps://example.com/auth, bare https://, and http:///local.\n",
            "Words: task-1234 disk-5678 bare sk- xai- AIza.\n",
            "Bearer prose: Invalid Bearer token; Bearer Token; Bearer abcdefghijklmno; bearer-only; the bearer of news; Bearer realm=\"example\".\n",
            "Non-codes: Exit code 1.\n\n",
            "Upstream header.\nID ABCDEFGH.\n\n",
            "encode ABCD-1234.\n\n",
            "code: read-only; code: FOO-ABCD-1234; code: TOO1234567; status 2026-11-27.\n\n",
            "encode {far} NOTACODE {far} codex\n"
        ),
        far = far,
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
        !result.to_string().contains("withheld"),
        "unexpected withholding in clean diagnostic: {result}"
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

#[test]
fn adversarial_four_mib_diagnostics_complete_in_linear_time() {
    let fleet = Fleet::new(&["agy"]);
    let rejected_parens_url = format!(
        "https://example.invalid/{}{}",
        "a".repeat((2 << 20) - 32),
        ")]".repeat(1 << 20)
    );
    let auth_parens_url = format!(
        "https://example.invalid/login?{}{}",
        "a".repeat((2 << 20) - 128),
        ")]".repeat((1 << 20) - 64)
    );
    let http_fragments = format!(
        "{}{} https://auth.example.invalid/login",
        "http://".repeat(550_000),
        "http://a#".repeat(10_000),
    );
    assert!(rejected_parens_url.len() > 3_900_000);
    assert!(auth_parens_url.len() > 3_900_000);
    assert!(http_fragments.len() > 3_900_000);

    let auth_json_err = json!({
        "status": "ERROR",
        "response": "",
        "error": auth_parens_url
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {"stderr": rejected_parens_url, "code": 2},
            {"stdout": auth_json_err, "code": 1},
            {"stderr": http_fragments, "code": 2},
        ]),
    );
    let mut server = fleet.server("claude");

    for (label, expect_redacted) in [
        ("rejected URL with 2M parens/brackets", false),
        ("auth URL with 2M parens/brackets", true),
        ("4 MiB http:// and fragment runs", false),
    ] {
        let started = Instant::now();
        let result = call(&mut server, "antigravity", json!({"prompt": label}));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "{label} took {:?} (expected < 2s)",
            elapsed
        );
        assert_eq!(result["observed"]["redacted"], expect_redacted, "{label}");
        if expect_redacted {
            assert_eq!(
                result["reported"]["failure"]["message"], WITHHELD_FAILURE_MESSAGE,
                "{result}"
            );
            assert!(result["observed"]["failure_file"].is_null(), "{result}");
            assert!(!result.to_string().contains("login?"), "{result}");
        }
    }

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn truncation_withholds_when_matched_before_2k_or_cut_at_capture_limit() {
    let fleet = Fleet::new(&["agy", "codex"]);

    let stderr_cut_ascii = format!("{} ABCD-1234\nEnter this code", "x".repeat(2037));
    let stderr_cut_multibyte = format!("{}é ABCD-1234\nEnter this code", "x".repeat(2035));
    let stderr_secret_past_limit = format!("{}ésk-past-2k-secret", "x".repeat(2047));
    // Capture-truncated (>4 MiB) stderr even with the cue cut past 4 MiB triggers conservative
    // withholding on failed turns and failed probes.
    let capture_cut_stderr = format!(
        "Failed with {} ABCD-1234\n{}Enter this code",
        "p".repeat((4 << 20) - 64),
        "q".repeat(128),
    );
    assert!(capture_cut_stderr.len() > (4 << 20));
    fleet.script(
        "agy",
        json!([
            {"stderr": stderr_cut_ascii, "code": 2},
            {"stderr": stderr_cut_multibyte, "code": 2},
            {"stderr": stderr_secret_past_limit, "code": 2},
            {"stderr": capture_cut_stderr, "code": 2}
        ]),
    );

    let probe_cut = format!("{} WXYZ-5678\nEnter this code", "p".repeat(2037));
    fs::write(
        fleet.root.join("codex.version"),
        json!({"stderr": probe_cut, "code": 3}).to_string(),
    )
    .unwrap();

    let mut server = fleet.server("claude");

    for (label, forbidden) in [
        ("ascii boundary", "ABCD-1234"),
        ("multibyte boundary", "ABCD-1234"),
    ] {
        let result = call(&mut server, "antigravity", json!({"prompt": label}));
        assert_observed_failure(&result, "abnormal_exit", "exit code 2");
        assert_eq!(result["observed"]["redacted"], true, "{label}: {result}");
        assert_eq!(
            result["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC,
            "{label}: {result}"
        );
        assert!(
            !result.to_string().contains(forbidden),
            "{label} leaked {forbidden}: {result}"
        );
    }

    let past = call(&mut server, "antigravity", json!({"prompt": "past 2k"}));
    assert_eq!(past["observed"]["redacted"], false, "{past}");
    let past_diag = past["reported"]["diagnostic"].as_str().unwrap();
    assert_eq!(past_diag.len(), 2047);
    assert!(!past.to_string().contains("sk-past-2k-secret"), "{past}");

    let capture_cut = call(&mut server, "antigravity", json!({"prompt": "capture cut"}));
    assert_observed_failure(&capture_cut, "abnormal_exit", "exit code 2");
    assert_eq!(capture_cut["observed"]["redacted"], true, "{capture_cut}");
    assert_eq!(capture_cut["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(
        !capture_cut.to_string().contains("ABCD-1234"),
        "{capture_cut}"
    );

    let probe_failed = call(&mut server, "codex", json!({"prompt": "probe cut"}));
    assert_observed_failure(&probe_failed, "launch", "exit code 3");
    assert_eq!(probe_failed["observed"]["redacted"], true, "{probe_failed}");
    assert_eq!(probe_failed["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    assert!(
        !probe_failed.to_string().contains("WXYZ-5678"),
        "{probe_failed}"
    );

    // Capture-truncated probe stderr is also withheld on ProbeFailed.
    fs::write(
        fleet.root.join("codex.version"),
        json!({"stderr": capture_cut_stderr, "code": 3}).to_string(),
    )
    .unwrap();
    let mut probe_cut_server = fleet.server("claude");
    let probe_capture_cut = call(
        &mut probe_cut_server,
        "codex",
        json!({"prompt": "probe capture cut"}),
    );
    assert_observed_failure(&probe_capture_cut, "launch", "exit code 3");
    assert_eq!(probe_capture_cut["observed"]["redacted"], true);
    assert_eq!(
        probe_capture_cut["reported"]["diagnostic"],
        WITHHELD_DIAGNOSTIC
    );
    assert!(!probe_capture_cut.to_string().contains("ABCD-1234"));

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn session_save_serde_errors_withhold_in_envelope_and_server_stderr() {
    let fleet = Fleet::new(&["agy"]);
    let agy_finished = json!({
        "status": "SUCCESS",
        "conversation_id": AGY_ID,
        "response": "ok"
    })
    .to_string();
    fleet.script(
        "agy",
        json!([
            {"stdout": agy_finished},
            {"stdout": agy_finished},
            {"stdout": agy_finished}
        ]),
    );

    let mut server = fleet.server("claude");
    let seed = call(
        &mut server,
        "antigravity",
        json!({"prompt": "seed session", "model": "m"}),
    );
    assert_eq!(seed["observed"]["state"], "finished", "{seed}");

    #[cfg(all(unix, not(target_os = "macos")))]
    let sessions_dir = fleet
        .root
        .join(".local/state/proxenos/sessions/antigravity");
    #[cfg(target_os = "macos")]
    let sessions_dir = fleet
        .root
        .join("Library/Application Support/proxenos/sessions/antigravity");
    #[cfg(windows)]
    let sessions_dir = fleet.root.join("proxenos/sessions/antigravity");

    let session_file = fs::read_dir(&sessions_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .expect("session JSON file");

    let corrupt_payloads = [
        (
            r#"{"schema_version":1,"sk-session-save-secret":1}"#,
            "sk-session-save-secret",
        ),
        (
            r#"{"schema_version":"https://save.example.invalid/oauth?code=SAVE-1"}"#,
            "save.example.invalid",
        ),
    ];

    for (index, (corrupt, forbidden)) in corrupt_payloads.into_iter().enumerate() {
        fs::write(&session_file, corrupt).unwrap();
        let saved = call(
            &mut server,
            "antigravity",
            json!({"prompt": format!("save fail {index}"), "model": "m"}),
        );
        assert_eq!(saved["observed"]["state"], "finished", "{saved}");
        assert_eq!(saved["observed"]["redacted"], true, "{saved}");
        assert_eq!(
            saved["observed"]["session_save_failure"]["message"],
            WITHHELD_SESSION_SAVE_MESSAGE
        );
        assert!(
            !saved.to_string().contains(forbidden),
            "envelope leaked {forbidden}: {saved}"
        );
        server.wait_for_stderr_count("could not save session settings:", index + 1);
    }

    assert!(server.finish().0.success());
    let server_stderr = server.stderr();
    for forbidden in ["sk-session-save-secret", "save.example.invalid", "SAVE-1"] {
        assert!(
            !server_stderr.contains(forbidden),
            "server stderr leaked {forbidden}: {server_stderr}"
        );
    }
    assert!(
        server_stderr.contains(&format!(
            "could not save session settings: {WITHHELD_SESSION_SAVE_MESSAGE}"
        )),
        "{server_stderr}"
    );
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn antigravity_error_record_drops_response_content_and_urls() {
    let fleet = Fleet::new(&["agy"]);
    let short_auth_with_response = json!({
        "conversation_id": AGY_ID,
        "status": "ERROR",
        "response": "Please open https://agy.example.invalid/oauth?code=RESP-AUTH to sign in.",
        "error": "authentication failed or timed out"
    })
    .to_string();
    let long_auth_with_response = json!({
        "conversation_id": AGY_ID,
        "status": "ERROR",
        "response": format!(
            "Sign in at https://agy.example.invalid/oauth?code=RESP-LONG {}",
            "r".repeat(16 << 10)
        ),
        "error": "authentication failed or timed out"
    })
    .to_string();
    let non_auth_with_response = json!({
        "conversation_id": AGY_ID,
        "status": "ERROR",
        "response": "Draft with https://agy.example.invalid/oauth?code=RESP-QUOTA",
        "error": "Quota exhausted. Quota resets at 2030-04-02T15:00:00Z."
    })
    .to_string();

    fleet.script(
        "agy",
        json!([
            {"stdout": short_auth_with_response, "code": 1},
            {"stdout": long_auth_with_response, "code": 1},
            {"stdout": non_auth_with_response, "code": 1}
        ]),
    );
    let mut server = fleet.server("claude");

    for (label, forbidden) in [("short auth", "RESP-AUTH"), ("long auth", "RESP-LONG")] {
        let res = call(&mut server, "antigravity", json!({"prompt": label}));
        assert_reported_failure(
            &res,
            Some(("abnormal_exit", "exit code 1")),
            "authentication",
            AUTHENTICATION_FAILURE_MESSAGE,
        );
        assert!(res["reported"]["content"].is_null(), "{label}: {res}");
        assert!(res["observed"]["content_file"].is_null(), "{label}: {res}");
        assert!(
            res["observed"]["content_captured_bytes"].is_null(),
            "{label}: {res}"
        );
        assert!(
            !res.to_string().contains(forbidden)
                && !res.to_string().contains("agy.example.invalid"),
            "{label} leaked response URL: {res}"
        );
    }

    let quota = call(&mut server, "antigravity", json!({"prompt": "quota error"}));
    assert_reported_failure(
        &quota,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Quota exhausted",
    );
    assert!(quota["reported"]["content"].is_null(), "{quota}");
    assert!(quota["observed"]["content_file"].is_null(), "{quota}");
    assert!(!quota.to_string().contains("RESP-QUOTA"), "{quota}");
    assert!(
        !quota.to_string().contains("agy.example.invalid"),
        "{quota}"
    );

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn doctor_withholds_sign_in_prompts_from_zero_exit_probes() {
    let fleet = Fleet::new(&["codex", "agy", "kimi"]);
    fs::write(
        fleet.root.join("codex.version"),
        json!({
            "stdout": "Visit https://doctor.example.invalid/oauth?code=DOC-1 and enter code: DOCX-1234\n",
            "code": 0
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        fleet.root.join("agy.version"),
        json!({
            "stderr": "DOCX-9999\nEnter this code with sk-doctor-secret\n",
            "code": 0
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        fleet.root.join("kimi.version"),
        json!({
            "stdout": format!("2.8.0\n{}", "k".repeat((4 << 20) + 64)),
            "code": 0
        })
        .to_string(),
    )
    .unwrap();

    for args in [&["doctor", "--json"][..], &["doctor"][..]] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_proxenos"));
        cmd.args(args)
            .current_dir(&fleet.root)
            .env("PATH", fleet.root.join("bin"))
            .env("PROXENOS_STUB_DIR", &fleet.root)
            .env("HOME", &fleet.root)
            .env("USERPROFILE", &fleet.root)
            .env("LOCALAPPDATA", &fleet.root)
            .env("APPDATA", &fleet.root)
            .env_remove("XDG_STATE_HOME");
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for forbidden in [
            "doctor.example.invalid",
            "DOC-1",
            "DOCX-1234",
            "DOCX-9999",
            "sk-doctor-secret",
            "2.8.0",
        ] {
            assert!(
                !stdout.contains(forbidden) && !stderr.contains(forbidden),
                "doctor ({args:?}) leaked {forbidden:?}:\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
        assert!(stdout.contains(WITHHELD_PROBE_VERSION), "{stdout}");
    }
}

#[test]
fn already_scanned_truncated_diagnostic_does_not_rescan_cut_identifier() {
    let fleet = Fleet::new(&["agy", "codex"]);

    // Build ID `LONGBUILDID12345` (16 chars) sits across byte 2048 with `code:` nearby.
    // The 2048-byte excerpt cuts it to 8 uppercase chars (`LONGBUIL`), which must not be
    // rescanned and falsely flagged during envelope finalization.
    let stderr_cut_id = format!(
        "{} code: {} LONGBUILDID12345 tail",
        "x".repeat(1994),
        "y".repeat(38),
    );
    let probe_cut_id = format!(
        "{} code: {} LONGPROBEID12345 tail",
        "a".repeat(1994),
        "b".repeat(38),
    );
    assert_eq!(stderr_cut_id.find("LONGBUILDID12345"), Some(2040));
    assert_eq!(probe_cut_id.find("LONGPROBEID12345"), Some(2040));

    fleet.script("agy", json!([{"stderr": stderr_cut_id, "code": 2}]));
    fs::write(
        fleet.root.join("codex.version"),
        json!({"stderr": probe_cut_id, "code": 3}).to_string(),
    )
    .unwrap();

    let mut server = fleet.server("claude");

    let stderr_res = call(&mut server, "antigravity", json!({"prompt": "cut id"}));
    assert_observed_failure(&stderr_res, "abnormal_exit", "exit code 2");
    assert_eq!(stderr_res["observed"]["redacted"], false, "{stderr_res}");
    let diag = stderr_res["reported"]["diagnostic"].as_str().unwrap();
    assert_eq!(diag.len(), 2048);
    assert!(diag.ends_with(" LONGBUIL"), "{diag}");
    assert!(!stderr_res.to_string().contains("withheld"), "{stderr_res}");

    let probe_res = call(&mut server, "codex", json!({"prompt": "probe cut id"}));
    assert_observed_failure(&probe_res, "launch", "exit code 3");
    assert_eq!(probe_res["observed"]["redacted"], false, "{probe_res}");
    let probe_diag = probe_res["reported"]["diagnostic"].as_str().unwrap();
    assert_eq!(probe_diag.len(), 2048);
    assert!(probe_diag.ends_with(" LONGPROB"), "{probe_diag}");
    assert!(!probe_res.to_string().contains("withheld"), "{probe_res}");

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn unicode_whitespace_terminates_urls_and_withholds_only_auth_urls() {
    let fleet = Fleet::new(&["agy"]);
    let clean_localized_stderr = concat!(
        "通常案内: https://example.invalid/ヘルプ\u{3000}auth や login と code の説明です。\n",
        "詳細: https://example.invalid/café\u{00a0}device\u{2003}続行します。\n"
    );
    let auth_localized_stderr = concat!(
        "認証が必要です: https://auth.example.invalid/café/login?next=1\u{3000}",
        "再試行する前にブラウザでログインしてください。\n"
    );
    fleet.script(
        "agy",
        json!([
            {"stderr": clean_localized_stderr, "code": 2},
            {"stderr": auth_localized_stderr, "code": 2}
        ]),
    );

    let mut server = fleet.server("claude");

    let clean_res = call(
        &mut server,
        "antigravity",
        json!({"prompt": "localized clean"}),
    );
    assert_observed_failure(&clean_res, "abnormal_exit", "exit code 2");
    assert_eq!(clean_res["observed"]["redacted"], false, "{clean_res}");
    assert_eq!(
        clean_res["reported"]["diagnostic"], clean_localized_stderr,
        "{clean_res}"
    );

    let auth_res = call(
        &mut server,
        "antigravity",
        json!({"prompt": "localized auth"}),
    );
    assert_observed_failure(&auth_res, "abnormal_exit", "exit code 2");
    assert_eq!(auth_res["observed"]["redacted"], true, "{auth_res}");
    assert_eq!(auth_res["reported"]["diagnostic"], WITHHELD_DIAGNOSTIC);
    let raw = auth_res.to_string();
    assert!(!raw.contains("auth.example.invalid"), "{raw}");
    assert!(!raw.contains("café"), "{raw}");

    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn provider_reported_success_with_observed_nonzero_exit_preserves_content() {
    let fleet = Fleet::new(&["agy"]);
    let success_payload = json!({
        "conversation_id": AGY_ID,
        "status": "SUCCESS",
        "response": "Completed answer https://example.invalid/oauth?code=KEEP-SUCCESS"
    })
    .to_string();
    fleet.script("agy", json!([{"stdout": success_payload, "code": 1}]));

    let mut server = fleet.server("claude");
    let res = call(
        &mut server,
        "antigravity",
        json!({"prompt": "exit 1 after success"}),
    );
    assert_observed_failure(&res, "abnormal_exit", "exit code 1");
    assert_eq!(res["observed"]["redacted"], false, "{res}");
    assert_eq!(
        res["reported"]["content"],
        "Completed answer https://example.invalid/oauth?code=KEEP-SUCCESS"
    );

    fleet.assert_steps_remaining("agy", 0);
}
