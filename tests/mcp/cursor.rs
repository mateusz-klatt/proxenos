use serde_json::{Value, json};

use crate::provider_support::{Fleet, assert_observed_failure, assert_reported_failure, call};

const START: &str = include_str!("../fixtures/cursor/synthetic-start.stdout.json");
const REPLY: &str = include_str!("../fixtures/cursor/synthetic-reply.stdout.json");
const LOGIN_OUT: &str = include_str!("../fixtures/cursor/captured-missing-login.stdout.txt");
const LOGIN_ERR: &str = include_str!("../fixtures/cursor/captured-missing-login.stderr.txt");
const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const OTHER: &str = "00000000-0000-4000-8000-000000000002";

#[test]
fn start_and_explicit_reply_use_the_documented_json_shape_and_distinct_model_rules() {
    let fleet = Fleet::new(&["cursor-agent"]);
    fleet.script("cursor-agent", json!([{"stdout":START},{"stdout":REPLY}]));
    let mut server = fleet.server("codex");
    let start = call(
        &mut server,
        "cursor",
        json!({
            "prompt":"--remember juniper", "model":"fixture-model", "cwd":fleet.root,
            "instructions":"Use one sentence."
        }),
    );
    let reply = call(
        &mut server,
        "cursor-reply",
        json!({"thread_id":SESSION,"prompt":"Recall the word."}),
    );
    for (result, answer) in [
        (&start, "Stored the sample word."),
        (&reply, "The sample word was juniper."),
    ] {
        assert_eq!(result["observed"]["state"], "finished", "{result}");
        assert_eq!(result["reported"]["thread_id"], SESSION);
        assert_eq!(result["reported"]["content"], answer);
        assert!(result["reported"]["model"].is_null());
        assert!(result["reported"]["usage"].is_null());
    }
    assert_eq!(start["observed"]["passed"]["model"], "fixture-model");
    assert!(reply["observed"]["passed"].get("model").is_none());
    let invocations: Vec<_> = fleet
        .records()
        .into_iter()
        .filter(|r| r["args"][0] == "--print")
        .collect();
    assert_eq!(invocations.len(), 2);
    let resume = format!("--resume={SESSION}");
    for (index, record) in invocations.iter().enumerate() {
        let args = record["args"].as_array().unwrap();
        let (prompt, flags) = args.split_last().unwrap();
        let mut expected = vec!["--print", "--output-format", "json", "--force"];
        if index == 0 {
            expected.extend(["--model", "fixture-model"]);
        } else {
            expected.push(&resume);
        }
        expected.push("--");
        assert_eq!(json!(flags), json!(expected));
        assert!(prompt.as_str().unwrap().contains("Use one sentence."));
        assert!(prompt.as_str().unwrap().ends_with(if index == 0 {
            "--remember juniper"
        } else {
            "Recall the word."
        }));
        assert_eq!(
            std::fs::canonicalize(record["cwd"].as_str().unwrap()).unwrap(),
            fleet.root.canonicalize().unwrap()
        );
    }
}

#[test]
fn schemas_and_validation_require_explicit_sessions_and_offer_only_full_access() {
    let fleet = Fleet::new(&["cursor-agent"]);
    let mut own = fleet.server("cursor");
    let own_tools = own.request(9, "tools/list", json!({}));
    assert!(
        !own_tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str().unwrap().starts_with("cursor"))
    );
    let mut server = fleet.server("codex");
    let listed = server.request(10, "tools/list", json!({}));
    for name in ["cursor", "cursor-reply"] {
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap();
        let props = &tool["inputSchema"]["properties"];
        assert_eq!(props.get("model").is_some(), name == "cursor");
        assert_eq!(props["access"]["enum"], json!(["full"]));
        assert!(props.get("effort").is_none());
    }
    for id in ["sample title", "00000000", "-1", "--continue"] {
        let value = call(
            &mut server,
            "cursor-reply",
            json!({"prompt":"resume","thread_id":id,"cwd":fleet.root,"access":"full","mode":"advisory"}),
        );
        assert_observed_failure(&value, "invalid_request", "canonical");
    }
    let model = call(
        &mut server,
        "cursor-reply",
        json!({"prompt":"resume","thread_id":SESSION,"model":"another","cwd":fleet.root,"access":"full","mode":"advisory"}),
    );
    assert_observed_failure(&model, "invalid_request", "model");
    let access = call(
        &mut server,
        "cursor",
        json!({"prompt":"check","access":"read-only"}),
    );
    assert_observed_failure(&access, "invalid_request", "read-only");
    assert!(
        fleet
            .records()
            .iter()
            .all(|r| r["args"] == json!(["--version"]))
    );
}

#[test]
fn native_authentication_failure_is_reported_separately_from_process_status() {
    let fleet = Fleet::new(&["cursor-agent"]);
    fleet.script(
        "cursor-agent",
        json!([
            {"code":1,"stderr":LOGIN_ERR,"stdout":LOGIN_OUT},
            {"code":1,"stderr":"tool output mentioned authentication"},
            {"code":1,"stderr":LOGIN_ERR,"stdout":START},
            {"code":2,"stderr":LOGIN_ERR}
        ]),
    );
    let mut server = fleet.server("codex");
    let auth = call(&mut server, "cursor", json!({"prompt":"check"}));
    assert_reported_failure(
        &auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        "Authentication required",
    );
    assert_eq!(auth["reported"]["diagnostic"], LOGIN_ERR);
    for code in [1, 1, 2] {
        let other = call(&mut server, "cursor", json!({"prompt":"check"}));
        assert_observed_failure(&other, "abnormal_exit", &format!("exit code {code}"));
        assert!(other["reported"]["content"].is_null());
        assert!(other["reported"]["thread_id"].is_null());
    }
    let record = fleet
        .records()
        .into_iter()
        .find(|r| r["args"][0] == "--print")
        .unwrap();
    assert!(
        !record["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--model")
    );
}

#[test]
fn completion_needs_one_success_result_and_the_requested_session() {
    let fleet = Fleet::new(&["cursor-agent"]);
    let terminal: Value = serde_json::from_str(START).unwrap();
    let mut steps = Vec::new();
    for (key, value) in [
        ("type", json!("event")),
        ("subtype", json!("pending")),
        ("is_error", json!(true)),
        ("result", json!(" \n")),
        ("session_id", Value::Null),
    ] {
        let mut invalid = terminal.clone();
        invalid[key] = value;
        steps.push(json!({"stdout":invalid.to_string()}));
    }
    steps.push(json!({"stdout":format!("{START}{REPLY}")}));
    steps.push(
        json!({"stdout":json!(["result","success",false,"array answer",SESSION]).to_string()}),
    );
    let incomplete_count = steps.len();
    let mut invalid = terminal.clone();
    invalid["session_id"] = json!("title");
    steps.extend([
        json!({"stdout":invalid.to_string()}),
        json!({"stdout":START}),
    ]);
    fleet.script("cursor-agent", json!(steps));
    let mut server = fleet.server("codex");
    for _ in 0..incomplete_count {
        let invalid = call(&mut server, "cursor", json!({"prompt":"check"}));
        assert_observed_failure(&invalid, "unknown", "without evidence");
        assert!(invalid["reported"]["content"].is_null());
        assert!(invalid["reported"]["thread_id"].is_null());
    }
    let bad_id = call(&mut server, "cursor", json!({"prompt":"check"}));
    let foreign = call(
        &mut server,
        "cursor-reply",
        json!({"prompt":"check","thread_id":OTHER,"cwd":fleet.root,"mode":"advisory","access":"full"}),
    );
    for invalid in [bad_id, foreign] {
        assert_reported_failure(&invalid, None, "unknown", "confirm the session");
        assert!(invalid["reported"]["content"].is_null());
        assert!(invalid["reported"]["thread_id"].is_null());
    }
    fleet.assert_steps_remaining("cursor-agent", 0);
}

#[test]
fn a_valid_captured_prefix_cannot_hide_a_second_terminal_result() {
    let fleet = Fleet::new(&["cursor-agent"]);
    let stdout = format!("{START}{}{REPLY}", " ".repeat(4 << 20));
    fleet.script("cursor-agent", json!([{"stdout":stdout}]));
    let mut server = fleet.server("codex");
    let result = call(&mut server, "cursor", json!({"prompt":"check"}));
    assert_observed_failure(&result, "capture", "capture limit");
    assert!(result["reported"]["content"].is_null());
    assert!(result["reported"]["thread_id"].is_null());
}
