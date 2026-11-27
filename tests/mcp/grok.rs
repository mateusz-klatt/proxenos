use serde_json::{Value, json};

use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, assert_observed_failure,
    assert_reported_failure, call,
};

const START: &str = include_str!("../fixtures/grok/captured-start.stdout.json");
const REPLY: &str = include_str!("../fixtures/grok/captured-reply.stdout.json");
const LOGIN: &str = include_str!("../fixtures/grok/captured-missing-login.stdout.json");
const LOGIN_ERR: &str = include_str!("../fixtures/grok/captured-missing-login.stderr.txt");
const MISSING: &str = include_str!("../fixtures/grok/captured-unknown-session.stdout.json");
const MISSING_ERR: &str = include_str!("../fixtures/grok/captured-unknown-session.stderr.txt");
const SESSION: &str = "01a21907-cf8c-7550-ad5c-dadebae7ae4b";
const ABSENT: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn native_start_and_reply_report_per_invocation_usage_and_explicit_session() {
    let fleet = Fleet::new(&["grok"]);
    fleet.script("grok", json!([{"stdout":START},{"stdout":REPLY}]));
    let mut server = fleet.server("codex");
    let start = call(
        &mut server,
        "grok",
        json!({"prompt":"--detail","model":"proxenos-local","effort":"medium","cwd":fleet.root,"instructions":"Check the example."}),
    );
    let reply = call(
        &mut server,
        "grok-reply",
        json!({"prompt":"Review it again.","thread_id":SESSION}),
    );
    for (result, n) in [(&start, 1), (&reply, 2)] {
        assert_eq!(result["observed"]["state"], "finished", "{result}");
        assert_eq!(result["reported"]["thread_id"], SESSION);
        assert_eq!(result["reported"]["model"], "proxenos-local");
        assert_eq!(
            result["reported"]["content"],
            format!("Proxenos fixture answer {n}.")
        );
        assert_eq!(
            result["reported"]["usage"],
            json!({"input_tokens":10*n,"output_tokens":2*n,"total_tokens":12*n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":0})
        );
        assert!(result["reported"].get("session_cost_usd").is_none());
    }
    let turns: Vec<_> = fleet
        .records()
        .into_iter()
        .filter(|r| r["args"][0] == "--output-format")
        .collect();
    assert_eq!(turns.len(), 2);
    for (i, invocation) in turns.iter().enumerate() {
        let args: Vec<_> = invocation["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        let mut expected = vec!["--output-format", "json", "--always-approve"];
        if i == 1 {
            expected.extend(["--resume", SESSION]);
        }
        expected.extend([
            "--model",
            "proxenos-local",
            "--reasoning-effort",
            "medium",
            "-p",
        ]);
        let (prompt, flags) = args.split_last().unwrap();
        assert_eq!(flags, expected.as_slice());
        assert!(prompt.contains("Check the example."));
        assert!(prompt.ends_with(if i == 0 {
            "--detail"
        } else {
            "Review it again."
        }));
        assert_eq!(invocation["env"]["CLAUDE_DELEGATOR_GROK_DEPTH"], "1");
    }
}

#[test]
fn grok_is_hidden_from_itself_and_validates_access_and_session_before_dispatch() {
    let fleet = Fleet::new(&["grok"]);
    let mut own = fleet.server("grok");
    let tools = own.request(12, "tools/list", json!({}));
    assert!(
        !tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str().unwrap().starts_with("grok"))
    );
    let mut server = fleet.server("codex");
    let tools = server.request(14, "tools/list", json!({}));
    for name in ["grok", "grok-reply"] {
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap();
        assert_eq!(
            tool["inputSchema"]["properties"]["access"]["enum"],
            json!(["full"])
        );
    }
    let denied = call(
        &mut server,
        "grok",
        json!({"prompt":"inspect","access":"read-only"}),
    );
    assert_observed_failure(&denied, "invalid_request", "read-only");
    for id in ["previous title", "01a21907", "--continue"] {
        let denied = call(
            &mut server,
            "grok-reply",
            json!({"prompt":"inspect","thread_id":id,"cwd":fleet.root,"access":"full","mode":"advisory"}),
        );
        assert_observed_failure(
            &denied,
            "invalid_request",
            "Grok thread_id must be a canonical session UUID.",
        );
    }
    assert!(
        fleet
            .records()
            .iter()
            .all(|r| r["args"] == json!(["--version"]))
    );
}

#[test]
fn native_failure_json_preserves_the_difference_between_login_and_unsearched_remote_sessions() {
    let fleet = Fleet::new(&["grok"]);
    fleet.script("grok",json!([{"code":1,"stdout":LOGIN,"stderr":LOGIN_ERR},{"code":1,"stdout":MISSING,"stderr":MISSING_ERR},{"code":1,"stdout":"{\"type\":\"error\"}","stderr":"Grok exited"}]));
    let mut server = fleet.server("codex");
    let login = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_reported_failure(
        &login,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert_eq!(login["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC);
    for raw in ["Not signed in", "grok login --device-code", "XAI_API_KEY"] {
        assert!(
            !login.to_string().contains(raw),
            "login text {raw:?} leaked: {login}"
        );
    }
    let missing = call(
        &mut server,
        "grok-reply",
        json!({"prompt":"check","thread_id":ABSENT,"cwd":fleet.root,"mode":"advisory","access":"full"}),
    );
    assert_reported_failure(
        &missing,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "weren't searched",
    );
    assert_eq!(missing["reported"]["diagnostic"], MISSING_ERR);
    assert!(missing["reported"]["thread_id"].is_null());
    let empty = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_observed_failure(&empty, "abnormal_exit", "exit code 1");
}

fn altered(field: &str, replacement: Value) -> String {
    let mut event: Value = serde_json::from_str(START).unwrap();
    event[field] = replacement;
    event.to_string()
}

#[test]
fn a_json_object_in_a_truncated_stream_cannot_confirm_a_turn() {
    let fleet = Fleet::new(&["grok"]);
    let stdout = format!("{START}{}\n{{}}\n", " ".repeat(4 << 20));
    fleet.script(
        "grok",
        json!([{"stdout":stdout},{"code":9,"stdout":stdout}]),
    );
    let mut server = fleet.server("codex");
    let result = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_observed_failure(&result, "capture", "capture limit");
    let attempt = &result["observed"]["attempts"][0];
    assert_eq!(attempt["stdout_bytes"], stdout.len());
    assert_eq!(attempt["stdout_captured"], 4 << 20);
    assert!(result["reported"]["content"].is_null());
    assert!(result["reported"]["thread_id"].is_null());
    let failed = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_observed_failure(&failed, "abnormal_exit", "exit code 9");
    assert!(failed["reported"]["content"].is_null());
}

#[test]
fn incomplete_foreign_and_nonzero_results_never_return_candidate_answer_bytes() {
    let fleet = Fleet::new(&["grok"]);
    fleet.script(
        "grok",
        json!([
            {"stdout":altered("sessionId",json!("any title"))},
            {"stdout":REPLY},
            {"stdout":altered("stopReason",json!("max_tokens"))},
            {"stdout":altered("num_turns",json!(0))},
            {"stdout":altered("text",json!("   "))},
            {"code":9,"stdout":START},
            {"stdout":"not a JSON result"}
        ]),
    );
    let mut server = fleet.server("codex");
    let invalid = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_reported_failure(&invalid, None, "unknown", "confirm the session");
    let foreign = call(
        &mut server,
        "grok-reply",
        json!({"prompt":"check","thread_id":ABSENT,"cwd":fleet.root,"mode":"advisory","access":"full"}),
    );
    assert_reported_failure(&foreign, None, "unknown", "confirm the session");
    assert!(foreign["reported"]["thread_id"].is_null());
    assert!(foreign["reported"]["content"].is_null());
    for expected in ["completed turn", "completed turn", "without an answer"] {
        let result = call(&mut server, "grok", json!({"prompt":"check"}));
        assert_reported_failure(&result, None, "unknown", expected);
        assert!(result["reported"]["content"].is_null());
    }
    let failed = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_observed_failure(&failed, "abnormal_exit", "exit code 9");
    assert!(failed["reported"]["content"].is_null());
    let incomplete = call(&mut server, "grok", json!({"prompt":"check"}));
    assert_observed_failure(&incomplete, "unknown", "without evidence");
    let invocations = fleet.records();
    let args = invocations
        .iter()
        .find(|r| r["args"][0] == "--output-format")
        .unwrap()["args"]
        .as_array()
        .unwrap();
    assert!(
        !args
            .iter()
            .any(|v| v == "--model" || v == "--reasoning-effort")
    );
}

#[test]
fn model_breakdowns_are_attributed_only_to_one_named_model_and_counts_are_allowlisted() {
    let fleet = Fleet::new(&["grok"]);
    let mut mixed: Value = serde_json::from_str(START).unwrap();
    mixed["usage"] =
        json!({"input_tokens":3,"reasoning_tokens":-7,"output_tokens":"2","private":"no"});
    mixed["modelUsage"] = json!({"a":{"modelCalls":1},"b":{"modelCalls":3}});
    fleet.script(
        "grok",
        json!([
            {"stdout":mixed.to_string()},
            {"stdout":altered("modelUsage",json!({}))},
            {"stdout":altered("modelUsage",json!({"a":{"modelCalls":0}}))},
            {"stdout":altered("modelUsage",Value::Null)},
            {"stdout":altered("usage",json!({"unrelated":"field"}))}
        ]),
    );
    let mut server = fleet.server("codex");
    let several = call(
        &mut server,
        "grok",
        json!({"prompt":"check", "model":"requested"}),
    );
    assert_eq!(several["reported"]["usage"], json!({"input_tokens":3}));
    assert!(several["reported"]["model"].is_null());
    for _ in 0..3 {
        let result = call(
            &mut server,
            "grok",
            json!({"prompt":"check","model":"requested"}),
        );
        assert_eq!(result["observed"]["state"], "finished");
        assert!(result["reported"]["model"].is_null());
    }
    let no_counts = call(&mut server, "grok", json!({"prompt":"check"}));
    assert!(no_counts["reported"]["usage"].is_null());
}
