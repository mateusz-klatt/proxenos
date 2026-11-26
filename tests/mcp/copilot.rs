use serde_json::{Value, json};

use crate::provider_support::{Fleet, assert_observed_failure, assert_reported_failure, call};

const START: &str = include_str!("../fixtures/copilot/captured-start-suffix.stdout.jsonl");
const REPLY: &str = include_str!("../fixtures/copilot/captured-reply-suffix.stdout.jsonl");
const LOGIN_OUT: &str = include_str!("../fixtures/copilot/captured-missing-login.stdout.jsonl");
const LOGIN_ERR: &str = include_str!("../fixtures/copilot/captured-missing-login.stderr.txt");
const UNKNOWN_ERR: &str = include_str!("../fixtures/copilot/captured-unknown-session.stderr.txt");
const SESSION: &str = "862fd7f3-7ae3-47d6-b775-a11817f3dc50";
const ABSENT: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn start_and_explicit_resume_use_the_native_turn_and_keep_the_start_settings() {
    let fleet = Fleet::new(&["copilot"]);
    fleet.script("copilot", json!([{"stdout": START},{"stdout": REPLY}]));
    let mut server = fleet.server("codex");
    let start = call(
        &mut server,
        "copilot",
        json!({
            "prompt":"--explain this", "cwd":fleet.root, "model":"proxenos-local",
            "effort":"medium", "instructions":"Keep the answer brief."
        }),
    );
    let reply = call(
        &mut server,
        "copilot-reply",
        json!({"prompt":"Check again.", "thread_id":SESSION}),
    );
    for response in [&start, &reply] {
        assert_eq!(response["observed"]["state"], "finished", "{response}");
        assert_eq!(response["reported"]["thread_id"], SESSION);
        assert_eq!(response["reported"]["model"], "proxenos-local");
        assert_eq!(
            response["reported"]["content"],
            "Deterministic fixture answer."
        );
        assert_eq!(response["reported"]["usage"], json!({"premium_requests":0}));
    }
    let invocations: Vec<_> = fleet
        .records()
        .into_iter()
        .filter(|r| r["args"][0] == "--output-format")
        .collect();
    assert_eq!(invocations.len(), 2);
    let resume_argument = format!("--resume={SESSION}");
    for (i, record) in invocations.iter().enumerate() {
        let argv: Vec<_> = record["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        for pair in [
            ["--output-format", "json"],
            ["--stream", "off"],
            ["--model", "proxenos-local"],
            ["--reasoning-effort", "medium"],
        ] {
            assert!(argv.windows(2).any(|s| s == pair), "{argv:?}");
        }
        assert!(argv.contains(&"--allow-all"));
        assert!(argv.contains(&"--no-custom-instructions"));
        assert_eq!(argv.contains(&resume_argument.as_str()), i == 1);
        assert!(!argv.contains(&"--continue"));
        let field = argv.iter().position(|s| *s == "-p").unwrap();
        assert!(argv[field + 1].contains("Keep the answer brief."));
        assert!(argv[field + 1].ends_with(if i == 0 {
            "--explain this"
        } else {
            "Check again."
        }));
        assert_eq!(record["env"]["CLAUDE_DELEGATOR_COPILOT_DEPTH"], "1");
    }
}

#[test]
fn listing_and_access_validation_match_copilots_available_interface() {
    let fleet = Fleet::new(&["copilot"]);
    let mut own = fleet.server("copilot");
    let listed = own.request(12, "tools/list", json!({}));
    assert!(
        !listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str().unwrap().starts_with("copilot"))
    );
    let mut server = fleet.server("codex");
    let listed = server.request(14, "tools/list", json!({}));
    for name in ["copilot", "copilot-reply"] {
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap();
        let props = &tool["inputSchema"]["properties"];
        assert_eq!(props["access"]["enum"], json!(["full"]));
        assert!(
            props["effort"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("xhigh"))
        );
    }
    for id in ["name", "862fd7f3", "--continue"] {
        let result = call(
            &mut server,
            "copilot-reply",
            json!({"prompt":"resume","thread_id":id,"cwd":fleet.root,"mode":"advisory","access":"full"}),
        );
        assert_observed_failure(&result, "invalid_request", "canonical");
    }
    let denied = call(
        &mut server,
        "copilot",
        json!({"prompt":"check","access":"read-only"}),
    );
    assert_observed_failure(&denied, "invalid_request", "read-only");
    assert!(
        fleet
            .records()
            .iter()
            .all(|r| r["args"] == json!(["--version"]))
    );
}

#[test]
fn native_failures_remain_separate_from_process_status_and_requested_session() {
    let fleet = Fleet::new(&["copilot"]);
    fleet.script(
        "copilot",
        json!([
            {"code":1,"stdout":LOGIN_OUT,"stderr":LOGIN_ERR},
            {"code":1,"stderr":UNKNOWN_ERR},
            {"code":1,"stderr":"tool printed: Error: No authentication information found."},
            {"code":1,"stdout":LOGIN_OUT,"stderr":"a different failure"},
            {"code":1,"stderr":UNKNOWN_ERR}
        ]),
    );
    let mut server = fleet.server("codex");
    let auth = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_reported_failure(
        &auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        "No authentication information found",
    );
    assert!(auth["reported"]["content"].is_null());
    assert!(auth["reported"]["thread_id"].is_null());
    assert_eq!(auth["reported"]["diagnostic"], LOGIN_ERR);
    let missing = call(
        &mut server,
        "copilot-reply",
        json!({"prompt":"check","thread_id":ABSENT,"cwd":fleet.root,"access":"full","mode":"advisory"}),
    );
    assert_reported_failure(
        &missing,
        Some(("abnormal_exit", "exit code 1")),
        "invalid_request",
        ABSENT,
    );
    let other = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_observed_failure(&other, "abnormal_exit", "exit code 1");
    let json_error = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_reported_failure(
        &json_error,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "reported exit code 1",
    );
    let wrong = call(
        &mut server,
        "copilot-reply",
        json!({"prompt":"check","thread_id":SESSION,"cwd":fleet.root,"access":"full","mode":"advisory"}),
    );
    assert_observed_failure(&wrong, "abnormal_exit", "exit code 1");
}

fn stream(events: &[Value]) -> String {
    events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn only_the_final_completed_main_turn_can_supply_answer_bytes() {
    let fleet = Fleet::new(&["copilot"]);
    let start = json!({"type":"assistant.turn_start","data":{"turnId":"main"}});
    let message = json!({"type":"assistant.message","data":{"turnId":"main","content":"answer","toolRequests":[]}});
    let end = json!({"type":"assistant.turn_end","data":{"turnId":"main"}});
    let result = json!({"type":"result","exitCode":0,"sessionId":SESSION});
    let tool = json!({"type":"assistant.message","data":{"turnId":"main","content":"tool preamble","toolRequests":[{"name":"view"}]}});
    let agent = json!({"type":"assistant.message","agentId":"other","data":{"turnId":"main","content":"foreign","toolRequests":[]}});
    let premature = json!({"type":"assistant.turn_end","data":{"turnId":"other"}});
    let wrong_id = json!({"type":"result","sessionId":"not-a-uuid","exitCode":0});
    fleet.script("copilot",json!([
        {"stdout":stream(&[start.clone(),message.clone(),agent.clone(),end.clone(),result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),tool,end.clone(),result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),premature,result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),end.clone(),start.clone(),result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),end.clone(),wrong_id])},
        {"stdout":stream(&[json!({"type":"session.start","data":{"sessionId":ABSENT}}),start.clone(),message.clone(),end.clone(),result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),end.clone(),result.clone()])},
        {"stdout":stream(&[start.clone(),message.clone(),end.clone()])},
        {"code":8,"stdout":stream(&[start,message,end,result])}
    ]));
    let mut server = fleet.server("codex");
    let ok = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_eq!(ok["reported"]["content"], "answer");
    assert!(ok["reported"]["usage"].is_null());
    for text in [
        "completed assistant",
        "completed assistant",
        "completed assistant",
        "confirm the session",
        "conflicting session",
    ] {
        let value = call(&mut server, "copilot", json!({"prompt":"check"}));
        assert_reported_failure(&value, None, "unknown", text);
        assert!(value["reported"]["content"].is_null());
    }
    let foreign = call(
        &mut server,
        "copilot-reply",
        json!({"prompt":"check","thread_id":ABSENT,"cwd":fleet.root,"mode":"advisory","access":"full"}),
    );
    assert_reported_failure(&foreign, None, "unknown", "confirm the session");
    assert!(foreign["reported"]["thread_id"].is_null());
    let incomplete = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_observed_failure(&incomplete, "unknown", "without evidence");
    let exit = call(&mut server, "copilot", json!({"prompt":"check"}));
    assert_observed_failure(&exit, "abnormal_exit", "exit code 8");
    assert!(exit["reported"]["content"].is_null());
    let records = fleet.records();
    let args = records
        .iter()
        .find(|r| r["args"][0] == "--output-format")
        .unwrap()["args"]
        .as_array()
        .unwrap();
    assert!(
        !args
            .iter()
            .any(|a| a == "--model" || a == "--reasoning-effort")
    );
}
