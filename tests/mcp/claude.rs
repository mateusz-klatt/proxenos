use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{
    AUTHENTICATION_DIAGNOSTIC, AUTHENTICATION_FAILURE_MESSAGE, Fleet, assert_observed_failure,
    assert_reported_failure, call,
};

const START_OK: &str = include_str!("../fixtures/claude/captured-start-ok.stdout.json");
const RESUME_OK: &str = include_str!("../fixtures/claude/captured-resume-ok.stdout.json");
const CANARY_START: &str = include_str!("../fixtures/claude/captured-canary-start.stdout.json");
const CANARY_RESUME: &str = include_str!("../fixtures/claude/captured-canary-resume.stdout.json");
const UNKNOWN_MODEL_OUT: &str =
    include_str!("../fixtures/claude/captured-unknown-model.stdout.json");
const UNKNOWN_MODEL_ERR: &str =
    include_str!("../fixtures/claude/captured-unknown-model.stderr.txt");
const NOT_LOGGED_IN: &str = include_str!("../fixtures/claude/captured-not-logged-in.stdout.json");
const BOGUS_RESUME_ERR: &str = include_str!("../fixtures/claude/captured-bogus-resume.stderr.txt");
const LOCAL_COMMAND: &str = include_str!("../fixtures/claude/captured-local-command.stdout.json");
const QUOTA_ERROR: &str = include_str!("../fixtures/claude/synthetic-quota-error.stdout.json");
const ERROR_MAX_TURNS: &str =
    include_str!("../fixtures/claude/synthetic-error-max-turns.stdout.json");
const PERMISSION_DENIAL: &str =
    include_str!("../fixtures/claude/synthetic-permission-denial.stdout.json");
const SESSION_ID: &str = "76655224-21a2-4670-925b-2681c62f95a3";
const CANARY_SESSION_ID: &str = "045dfd54-03fc-4163-8786-08b36ac575f6";
const OTHER_SESSION_ID: &str = "8a4d2c10-6f3b-4c9a-9e21-1f5b7c8d9e0a";

fn invocations(fleet: &Fleet) -> Vec<Value> {
    fleet
        .records()
        .into_iter()
        .filter(|record| record["cli"] == "claude" && record["args"] != json!(["--version"]))
        .collect()
}

fn argv(record: &Value) -> Vec<&str> {
    record["args"]
        .as_array()
        .unwrap_or_else(|| panic!("missing args array in invocation record: {record}"))
        .iter()
        .map(|arg| {
            arg.as_str()
                .unwrap_or_else(|| panic!("non-string arg in invocation record: {record}"))
        })
        .collect()
}

fn flag_value<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1])
}

#[test]
fn claude_tools_and_capabilities_are_listed_for_other_clients_and_hidden_from_claude_client() {
    let fleet = Fleet::new(&["claude"]);

    let mut self_server = fleet.server("claude");
    let self_list = self_server.request(1, "tools/list", json!({}));
    let self_names: Vec<&str> = self_list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .filter(|name| !name.starts_with("test_") && !matches!(*name, "await" | "cancel" | "usage"))
        .collect();
    assert!(self_names.is_empty(), "{self_names:?}");
    for tool in ["claude", "claude-reply"] {
        let denied = self_server.call(2, tool, json!({"prompt": "self-delegation"}));
        assert_eq!(denied["error"]["code"], -32602, "{tool}: {denied}");
    }
    assert!(
        fleet.records().is_empty(),
        "claude client must not probe or invoke its own provider"
    );

    let mut codex_server = fleet.server("codex");
    let list = codex_server.request(3, "tools/list", json!({}));
    let tools: Vec<&Value> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| {
            let name = tool["name"].as_str().unwrap();
            !name.starts_with("test_") && !matches!(name, "await" | "cancel" | "usage")
        })
        .collect();
    assert_eq!(tools.len(), 2);
    for (tool, reply) in [(tools[0], false), (tools[1], true)] {
        assert_eq!(tool["name"], if reply { "claude-reply" } else { "claude" });
        let schema = &tool["inputSchema"];
        let props = &schema["properties"];
        assert_eq!(props["thread_id"].is_object(), reply);
        assert_eq!(
            props["effort"]["enum"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        let access_desc = props["access"]["description"].as_str().unwrap();
        assert!(access_desc.contains("bypassPermissions"), "{access_desc}");
        assert!(access_desc.contains("workspace trust"), "{access_desc}");
        assert!(
            access_desc.contains("--permission-mode plan"),
            "{access_desc}"
        );
        assert!(
            access_desc.contains("--setting-sources user"),
            "{access_desc}"
        );
        assert!(access_desc.contains("--disallowedTools"), "{access_desc}");
        assert!(access_desc.contains("sandbox settings"), "{access_desc}");
        assert!(access_desc.contains("not an OS sandbox"), "{access_desc}");
        assert!(props["expert"].is_null());
        if reply {
            assert!(
                props["mode"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("keeps its starting mode")
            );
            assert!(props["model"].get("description").is_none());
        }
    }
}

#[test]
fn claude_starts_and_resumes_with_captured_fixtures_settings_effort_and_caveats() {
    let fleet = Fleet::new(&["claude"]);
    let cwd = fleet.root.join("chosen workspace");
    fs::create_dir(&cwd).unwrap();
    fleet.script(
        "claude",
        json!([
            {"stdout": START_OK},
            {"stdout": RESUME_OK},
            {"stdout": RESUME_OK}
        ]),
    );
    let mut cmd = fleet.command("codex");
    cmd.env("IRIS_EXECUTION", "caller-exec")
        .env("CLAUDECODE", "1")
        .env("PROXENOS_STUB_MARKER", "keep");
    let mut server = Server::start_command(cmd);
    server.initialize("2025-11-25");

    let unsupported = call(
        &mut server,
        "claude",
        json!({"prompt": "check", "effort": "ultra"}),
    );
    assert_observed_failure(&unsupported, "invalid_request", "effort");
    fleet.assert_steps_remaining("claude", 3);

    let task = "--analyze\nthis \"argument\"";
    let instructions = "Verify every claim.";
    let start = call(
        &mut server,
        "claude",
        json!({
            "prompt": task,
            "mode": "advisory",
            "model": "haiku",
            "effort": "high",
            "cwd": cwd,
            "access": "read-only",
            "instructions": instructions,
            "timeout_ms": 5_000,
            "coordination": {"caller_agent_name": "caller-agent"}
        }),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert_eq!(start["observed"]["provider"], "claude");
    assert_eq!(start["observed"]["passed"]["model"], "haiku");
    assert_eq!(start["observed"]["passed"]["effort"], "high");
    assert_eq!(start["observed"]["passed"]["access"], "read-only");
    assert!(
        start["observed"]["access_mechanism"]
            .as_str()
            .unwrap()
            .contains("--permission-mode plan")
    );
    assert_eq!(start["reported"]["thread_id"], SESSION_ID);
    assert_eq!(start["reported"]["content"], "OK");
    assert_eq!(start["reported"]["model"], "claude-sonnet-5-5");
    assert_eq!(
        start["reported"]["usage"],
        json!({
            "input_tokens": 4,
            "cache_creation_input_tokens": 54752,
            "cache_read_input_tokens": 77426,
            "output_tokens": 38,
            "thinking_tokens": 0
        })
    );
    assert_eq!(start["reported"]["session_cost_usd"], 0.22713859999999997);
    assert!(start["reported"]["cost_usd"].is_null());
    let not_claimed = start["not_claimed"].to_string();
    assert!(
        not_claimed.contains("That project settings in the working directory"),
        "{not_claimed}"
    );
    assert!(not_claimed.contains("were withheld"), "{not_claimed}");
    assert!(
        not_claimed.contains("skips workspace trust"),
        "{not_claimed}"
    );
    assert!(
        not_claimed.contains("plan mode enforced an OS sandbox"),
        "{not_claimed}"
    );
    assert!(
        not_claimed.contains("project or local sandbox settings"),
        "{not_claimed}"
    );
    assert!(
        not_claimed.contains("user and managed settings or permission allow-lists"),
        "{not_claimed}"
    );
    fleet.assert_steps_remaining("claude", 2);

    let reply = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": SESSION_ID, "prompt": "-"}),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["reported"]["content"], "OK");
    // total_cost_usd on a reply is cumulative for the session, not per-turn.
    assert_eq!(reply["reported"]["session_cost_usd"], 0.24204239999999996);
    assert!(
        reply["reported"]["session_cost_usd"].as_f64().unwrap()
            > start["reported"]["session_cost_usd"].as_f64().unwrap()
    );
    assert_eq!(reply["reported"]["usage"]["output_tokens"], 4);
    assert!(
        reply["not_claimed"]
            .to_string()
            .contains("plan mode enforced an OS sandbox")
    );
    assert!(reply["observed"]["passed"].get("mode").is_none());
    assert!(reply["observed"]["passed"].get("instructions").is_none());
    assert!(
        reply["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap()
            > 5_000,
        "reply inherited start timeout: {reply}"
    );
    fleet.assert_steps_remaining("claude", 1);

    let bad_mode = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": SESSION_ID, "prompt": "act", "mode": "implementation"}),
    );
    assert_observed_failure(&bad_mode, "invalid_request", "mode");
    let bad_instructions = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": SESSION_ID, "prompt": "act", "instructions": "Different"}),
    );
    assert_observed_failure(
        &bad_instructions,
        "invalid_request",
        "Omit instructions on a reply",
    );
    fleet.assert_steps_remaining("claude", 1);

    let override_reply = call(
        &mut server,
        "claude-reply",
        json!({
            "thread_id": SESSION_ID,
            "prompt": "recheck",
            "access": "full",
            "model": "sonnet",
            "effort": "low"
        }),
    );
    assert_eq!(
        override_reply["observed"]["state"], "finished",
        "{override_reply}"
    );
    assert!(
        override_reply["observed"]["access_mechanism"]
            .as_str()
            .unwrap()
            .contains("bypassPermissions")
    );
    let full_not_claimed = override_reply["not_claimed"].to_string();
    assert!(
        full_not_claimed.contains("That project settings in the working directory"),
        "{full_not_claimed}"
    );
    assert!(
        full_not_claimed.contains("were withheld"),
        "{full_not_claimed}"
    );
    assert!(
        !full_not_claimed.contains("plan mode"),
        "plan-mode non-claim must only appear under read-only: {full_not_claimed}"
    );
    fleet.assert_steps_remaining("claude", 0);

    let records = invocations(&fleet);
    assert_eq!(records.len(), 3);
    for (index, record) in records.iter().enumerate() {
        let args = argv(record);
        assert!(args.contains(&"--print"));
        assert_eq!(flag_value(&args, "--output-format"), Some("json"));
        assert!(args.contains(&"--disable-slash-commands"));
        assert!(!args.contains(&"--continue"));
        assert!(!args.contains(&"-c"));
        assert!(!args.contains(&"--add-dir"));
        assert!(!args.contains(&"--dangerously-skip-permissions"));
        assert_eq!(
            Path::new(record["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            cwd.canonicalize().unwrap()
        );
        assert_eq!(record["env"]["PROXENOS_DEPTH"], "1");
        assert_eq!(record["env"]["CLAUDE_DELEGATOR_CLAUDE_DEPTH"], "1");
        assert_eq!(record["env"]["PROXENOS_STUB_MARKER"], "keep");
        assert!(record["env"]["CLAUDECODE"].is_null());
        assert!(record["env"]["IRIS_EXECUTION"].is_null());

        if index == 0 {
            assert_eq!(flag_value(&args, "--permission-mode"), Some("plan"));
            assert_eq!(flag_value(&args, "--setting-sources"), Some("user"));
            assert_eq!(flag_value(&args, "--model"), Some("haiku"));
            assert_eq!(flag_value(&args, "--effort"), Some("high"));
            assert!(flag_value(&args, "--resume").is_none());
            let system = flag_value(&args, "--append-system-prompt").unwrap();
            assert!(system.contains("Advise the caller"));
            assert!(system.contains(instructions));
            assert_eq!(args[args.len() - 2], "--");
            let prompt = args[args.len() - 1];
            assert!(prompt.starts_with(task), "{prompt}");
            assert!(prompt.contains("caller-agent"), "{prompt}");
        } else {
            assert_eq!(flag_value(&args, "--resume"), Some(SESSION_ID));
            assert!(flag_value(&args, "--append-system-prompt").is_none());
            if index == 1 {
                assert_eq!(flag_value(&args, "--permission-mode"), Some("plan"));
                assert_eq!(flag_value(&args, "--setting-sources"), Some("user"));
                assert_eq!(flag_value(&args, "--model"), Some("haiku"));
                assert_eq!(flag_value(&args, "--effort"), Some("high"));
                assert!(args.ends_with(&["--", "-"]));
            } else {
                assert_eq!(
                    flag_value(&args, "--permission-mode"),
                    Some("bypassPermissions")
                );
                assert!(flag_value(&args, "--setting-sources").is_none());
                assert_eq!(flag_value(&args, "--model"), Some("sonnet"));
                assert_eq!(flag_value(&args, "--effort"), Some("low"));
                assert!(args.ends_with(&["--", "recheck"]));
            }
        }
    }
}

#[test]
fn claude_reply_survives_server_restart_and_follows_d19_model_less_rule() {
    let fleet = Fleet::new(&["claude"]);
    let cwd = fleet.root.join("restart workspace");
    fs::create_dir(&cwd).unwrap();
    let second_start = START_OK.replace(SESSION_ID, OTHER_SESSION_ID);
    let second_resume = RESUME_OK.replace(SESSION_ID, OTHER_SESSION_ID);
    fleet.script(
        "claude",
        json!([
            {"stdout": CANARY_START},
            {"stdout": second_start},
            {"stdout": CANARY_RESUME},
            {"stdout": second_resume}
        ]),
    );

    let mut server1 = fleet.server("codex");
    let explicit = call(
        &mut server1,
        "claude",
        json!({
            "prompt": "Reply with the single word OK.",
            "model": "haiku",
            "effort": "medium",
            "access": "read-only",
            "mode": "advisory",
            "cwd": cwd,
            "instructions": "If asked for the codeword, answer ZEBRA-41.",
            "timeout_ms": 5_000
        }),
    );
    assert_eq!(explicit["observed"]["state"], "finished", "{explicit}");
    assert_eq!(explicit["reported"]["thread_id"], CANARY_SESSION_ID);
    assert_eq!(explicit["reported"]["session_cost_usd"], 0.1913911);
    let model_less = call(
        &mut server1,
        "claude",
        json!({
            "prompt": "second",
            "access": "full",
            "mode": "implementation",
            "cwd": cwd
        }),
    );
    assert_eq!(model_less["observed"]["state"], "finished", "{model_less}");
    assert_eq!(model_less["reported"]["model"], "claude-sonnet-5-5");
    assert!(server1.finish().0.success());

    let mut server2 = fleet.server("codex");
    let reply1 = call(
        &mut server2,
        "claude-reply",
        json!({"thread_id": CANARY_SESSION_ID, "prompt": "What is the codeword? Reply with it only."}),
    );
    assert_eq!(reply1["observed"]["state"], "finished", "{reply1}");
    assert_eq!(reply1["reported"]["content"], "ZEBRA-41");
    assert_eq!(reply1["reported"]["session_cost_usd"], 0.2493133);
    assert_eq!(reply1["observed"]["passed"]["model"], "haiku");
    assert_eq!(reply1["observed"]["passed"]["effort"], "medium");
    assert_eq!(reply1["observed"]["passed"]["access"], "read-only");

    let bad_mode = call(
        &mut server2,
        "claude-reply",
        json!({"thread_id": CANARY_SESSION_ID, "prompt": "change", "mode": "implementation"}),
    );
    assert_observed_failure(&bad_mode, "invalid_request", "mode");

    let repeated_instructions = call(
        &mut server2,
        "claude-reply",
        json!({
            "thread_id": CANARY_SESSION_ID,
            "prompt": "repeat instructions",
            "instructions": "If asked for the codeword, answer ZEBRA-41."
        }),
    );
    assert_observed_failure(
        &repeated_instructions,
        "invalid_request",
        "Omit instructions on a reply",
    );

    let reply2 = call(
        &mut server2,
        "claude-reply",
        json!({"thread_id": OTHER_SESSION_ID, "prompt": "after restart 2"}),
    );
    assert_eq!(reply2["observed"]["state"], "finished", "{reply2}");
    assert!(reply2["observed"]["passed"]["model"].is_null());
    assert_eq!(reply2["observed"]["passed"]["access"], "full");

    let unknown = call(
        &mut server2,
        "claude-reply",
        json!({"thread_id": "11111111-1111-4111-8111-111111111111", "prompt": "missing"}),
    );
    assert_observed_failure(&unknown, "invalid_request", "no settings");
    fleet.assert_steps_remaining("claude", 0);

    let records = invocations(&fleet);
    assert_eq!(records.len(), 4);
    assert_eq!(flag_value(&argv(&records[2]), "--model"), Some("haiku"));
    assert_eq!(flag_value(&argv(&records[2]), "--effort"), Some("medium"));
    assert_eq!(
        flag_value(&argv(&records[2]), "--permission-mode"),
        Some("plan")
    );
    assert_eq!(
        flag_value(&argv(&records[2]), "--setting-sources"),
        Some("user")
    );
    assert!(flag_value(&argv(&records[3]), "--model").is_none());
    assert!(flag_value(&argv(&records[3]), "--effort").is_none());
    assert_eq!(
        flag_value(&argv(&records[3]), "--permission-mode"),
        Some("bypassPermissions")
    );
    assert!(flag_value(&argv(&records[3]), "--setting-sources").is_none());
    for record in &records[2..] {
        assert_eq!(
            Path::new(record["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            cwd.canonicalize().unwrap()
        );
    }
}

#[test]
fn claude_rejects_non_uuid_thread_ids_and_confirms_returned_session_id() {
    let fleet = Fleet::new(&["claude"]);
    let bad_start_id = START_OK.replace(SESSION_ID, "named-session");
    let long_foreign = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "session_id": OTHER_SESSION_ID,
        "result": "s".repeat((16 << 10) + 64),
        "total_cost_usd": 0.05,
        "usage": {"input_tokens": 10, "output_tokens": 20},
        "modelUsage": {"claude-sonnet-5-5": {"inputTokens": 10}}
    })
    .to_string();
    let foreign_error = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": OTHER_SESSION_ID,
        "result": "foreign error"
    })
    .to_string();
    let missing_id_success = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "result": "unconfirmed answer"
    })
    .to_string();
    let missing_id_quota = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "result": "You've hit your limit · resets at 2030-04-02T15:00:00Z.",
        "total_cost_usd": 0.01,
        "usage": {"input_tokens": 5}
    })
    .to_string();

    fleet.script(
        "claude",
        json!([
            {"stdout": bad_start_id},
            {"stdout": long_foreign},
            {"stdout": foreign_error, "code": 1},
            {"stdout": missing_id_success},
            {"stdout": missing_id_quota, "code": 1}
        ]),
    );
    let mut server = fleet.server("codex");

    for bad_id in [
        "named-session",
        "76655224-21A2-4670-925B-2681C62F95A3",
        "7665522421a24670925b2681c62f95a3",
    ] {
        let rejected = call(
            &mut server,
            "claude-reply",
            json!({
                "thread_id": bad_id,
                "prompt": "resume",
                "cwd": fleet.root,
                "access": "read-only",
                "mode": "advisory"
            }),
        );
        assert_observed_failure(&rejected, "invalid_request", "canonical UUID");
    }
    assert!(
        fleet.records().is_empty(),
        "non-UUID reply IDs must fail before discovery or dispatch"
    );

    let dropped_id = call(&mut server, "claude", json!({"prompt": "start"}));
    assert_eq!(dropped_id["observed"]["state"], "finished", "{dropped_id}");
    assert!(dropped_id["reported"]["thread_id"].is_null());
    fleet.assert_steps_remaining("claude", 4);

    let reply_args = json!({
        "thread_id": SESSION_ID,
        "prompt": "continue",
        "cwd": fleet.root,
        "access": "read-only",
        "mode": "advisory"
    });
    for (index, (observed, expect_id)) in [
        (None, Some(OTHER_SESSION_ID)),
        (
            Some(("abnormal_exit", "exit code 1")),
            Some(OTHER_SESSION_ID),
        ),
        (None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let mismatch = call(&mut server, "claude-reply", reply_args.clone());
        assert_reported_failure(
            &mismatch,
            observed,
            "unknown",
            "did not confirm the requested session ID",
        );
        assert_eq!(mismatch["reported"]["thread_id"].as_str(), expect_id);
        assert!(mismatch["reported"]["content"].is_null());
        assert!(mismatch["reported"]["model"].is_null());
        assert!(mismatch["reported"]["usage"].is_null());
        assert!(mismatch["reported"]["session_cost_usd"].is_null());
        assert!(mismatch["observed"]["content_file"].is_null());
        fleet.assert_steps_remaining("claude", 3 - index);
    }

    let quota = call(&mut server, "claude-reply", reply_args);
    assert_reported_failure(
        &quota,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "hit your limit",
    );
    assert_eq!(
        quota["reported"]["failure"]["reset_at"],
        "2030-04-02T15:00:00Z"
    );
    assert!(quota["reported"]["thread_id"].is_null());
    assert!(quota["reported"]["content"].is_null());
    assert!(quota["reported"]["usage"].is_null());
    assert!(quota["reported"]["session_cost_usd"].is_null());
    fleet.assert_steps_remaining("claude", 0);
}

#[test]
fn claude_classifies_captured_and_synthetic_errors_and_ignores_error_text_in_answers() {
    let fleet = Fleet::new(&["claude"]);
    let quota_no_reset = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": "Credit balance is too low"
    })
    .to_string();
    let status_401 = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "api_error_status": 401,
        "result": "Session token rejected"
    })
    .to_string();
    let auth_type = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "api_error_type": "authentication_error",
        "result": "Rejected credentials"
    })
    .to_string();
    let content_refusal = json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": "Output blocked by content filtering policy"
    })
    .to_string();
    let err_stderr_fallback = json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": ""
    })
    .to_string();
    let err_subtype_fallback = json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID
    })
    .to_string();
    let err_bare_fallback = json!({
        "type": "result",
        "is_error": true,
        "num_turns": 1,
        "session_id": SESSION_ID
    })
    .to_string();
    let max_turns_draft_quota = json!({
        "type": "result",
        "subtype": "error_max_turns",
        "is_error": false,
        "num_turns": 3,
        "session_id": SESSION_ID,
        "result": "You've hit your limit · resets at 9am."
    })
    .to_string();
    let empty_success = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": "   ",
        "permission_denials": []
    })
    .to_string();
    let noisy_answer = format!(
        "warning: starting session\n{}\ntrailing noise\n",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 1,
            "session_id": SESSION_ID,
            "result": "Not logged in · You've hit your limit."
        })
    );

    fleet.script(
        "claude",
        json!([
            {"stdout": UNKNOWN_MODEL_OUT, "stderr": UNKNOWN_MODEL_ERR, "code": 1},
            {"stdout": NOT_LOGGED_IN, "code": 1},
            {"stderr": BOGUS_RESUME_ERR, "code": 1},
            {"stdout": LOCAL_COMMAND, "code": 0},
            {"stdout": QUOTA_ERROR, "code": 1},
            {"stdout": QUOTA_ERROR, "code": 0},
            {"stdout": quota_no_reset, "code": 1},
            {"stdout": status_401, "code": 1},
            {"stdout": auth_type, "code": 1},
            {"stdout": content_refusal, "code": 1},
            {"stdout": ERROR_MAX_TURNS, "code": 0},
            {"stdout": max_turns_draft_quota, "code": 0},
            {"stdout": max_turns_draft_quota, "stderr": "401 Unauthorized", "code": 0},
            {"stdout": err_stderr_fallback, "stderr": "runtime aborted", "code": 1},
            {"stdout": err_subtype_fallback, "code": 1},
            {"stdout": err_bare_fallback, "code": 1},
            {"stdout": PERMISSION_DENIAL, "code": 0},
            {"stdout": empty_success, "stderr": "401 Unauthorized", "code": 0},
            {"stdout": empty_success, "code": 0},
            {"stderr": "401 Unauthorized", "code": 1},
            {"stdout": "plain text without json", "code": 0},
            {"stdout": noisy_answer, "stderr": "401 Unauthorized", "code": 0}
        ]),
    );
    let mut server = fleet.server("codex");

    // 1. Captured unknown model (exit 1, is_error: true with subtype: success).
    let unknown_model = call(&mut server, "claude", json!({"prompt": "1"}));
    assert_reported_failure(
        &unknown_model,
        Some(("abnormal_exit", "exit code 1")),
        "unknown",
        "selected model",
    );
    assert!(unknown_model["reported"]["content"].is_null());
    assert!(
        unknown_model["reported"]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("unrecognized_model")
    );
    // Failed start must not be remembered for replies.
    let unremembered = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": unknown_model["reported"]["thread_id"], "prompt": "retry"}),
    );
    assert_observed_failure(&unremembered, "invalid_request", "no settings");

    // 2. Captured not logged in.
    let not_logged_in = call(&mut server, "claude", json!({"prompt": "2"}));
    assert_reported_failure(
        &not_logged_in,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert_eq!(
        not_logged_in["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC,
        "{not_logged_in}"
    );
    assert!(
        !not_logged_in.to_string().contains("Not logged in"),
        "{not_logged_in}"
    );

    // 3. Captured bogus resume stderr (exit 1, empty stdout).
    let bogus = call(&mut server, "claude", json!({"prompt": "3"}));
    assert_observed_failure(&bogus, "abnormal_exit", "exit code 1");
    assert!(
        bogus["reported"]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("No conversation found with session ID")
    );

    // 4. Captured local command (exit 0, is_error: false, num_turns: 0).
    let local_cmd = call(&mut server, "claude", json!({"prompt": "/cost"}));
    assert_reported_failure(&local_cmd, None, "unknown", "without running a turn");
    assert!(local_cmd["reported"]["content"].is_null());

    // 5. Synthetic quota error (exit 1) and 6. Synthetic quota error with exit 0 (is_error: true).
    for (observed, label) in [(Some(("abnormal_exit", "exit code 1")), "5"), (None, "6")] {
        let quota = call(&mut server, "claude", json!({"prompt": label}));
        assert_reported_failure(&quota, observed, "quota", "hit your limit");
        assert_eq!(quota["reported"]["failure"]["reset_at"], "5pm (UTC)");
    }

    // 7-10. Quota without reset, 401 status, authentication_error type, content refusal.
    for (label, kind, detail) in [
        ("7", "quota", "Credit balance"),
        ("8", "authentication", AUTHENTICATION_FAILURE_MESSAGE),
        ("9", "authentication", AUTHENTICATION_FAILURE_MESSAGE),
        ("10", "refusal", "content filtering policy"),
    ] {
        let result = call(&mut server, "claude", json!({"prompt": label}));
        assert_reported_failure(
            &result,
            Some(("abnormal_exit", "exit code 1")),
            kind,
            detail,
        );
        assert!(result["reported"]["failure"]["reset_at"].is_null());
    }

    // 11. Synthetic error_max_turns (non-success subtype prefers errors array over partial result).
    let max_turns = call(&mut server, "claude", json!({"prompt": "11"}));
    assert_reported_failure(&max_turns, None, "unknown", "Reached max turns (3)");
    assert_eq!(
        max_turns["reported"]["failure"]["message"],
        "Reached max turns (3)"
    );
    assert_eq!(max_turns["reported"]["model"], "claude-haiku-4-5");
    assert_eq!(max_turns["reported"]["usage"]["thinking_tokens"], 12);
    assert_eq!(max_turns["reported"]["session_cost_usd"], 0.015);

    // 11b. Non-success subtype without errors ignores agent draft text in result (never sets quota/reset_at).
    let draft_only = call(&mut server, "claude", json!({"prompt": "11b"}));
    assert_reported_failure(
        &draft_only,
        None,
        "unknown",
        "Claude stopped with subtype error_max_turns.",
    );
    assert!(draft_only["reported"]["failure"]["reset_at"].is_null());

    // 11c. Non-success subtype without errors classifies from stderr instead of agent draft text.
    let draft_stderr = call(&mut server, "claude", json!({"prompt": "11c"}));
    assert_reported_failure(
        &draft_stderr,
        None,
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(
        !draft_stderr.to_string().contains("401 Unauthorized"),
        "{draft_stderr}"
    );
    assert!(draft_stderr["reported"]["failure"]["reset_at"].is_null());

    // 12-14. Error reason fallback chain (stderr -> subtype -> generic).
    for (label, detail) in [
        ("12", "runtime aborted"),
        ("13", "subtype error_during_execution"),
        ("14", "unsuccessful result"),
    ] {
        let result = call(&mut server, "claude", json!({"prompt": label}));
        assert_reported_failure(
            &result,
            Some(("abnormal_exit", "exit code 1")),
            "unknown",
            detail,
        );
    }

    // 15. Synthetic permission_denials on empty result.
    let denied = call(&mut server, "claude", json!({"prompt": "15"}));
    assert_reported_failure(&denied, None, "refusal", "permission denials");

    // 16-17. Empty result with classified stderr vs empty stderr.
    let empty_auth = call(&mut server, "claude", json!({"prompt": "16"}));
    assert_reported_failure(
        &empty_auth,
        None,
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(
        !empty_auth.to_string().contains("401 Unauthorized"),
        "{empty_auth}"
    );
    let empty_plain = call(&mut server, "claude", json!({"prompt": "17"}));
    assert_reported_failure(&empty_plain, None, "unknown", "empty result");

    // 18. Non-JSON stderr auth failure.
    let stderr_auth = call(&mut server, "claude", json!({"prompt": "18"}));
    assert_reported_failure(
        &stderr_auth,
        Some(("abnormal_exit", "exit code 1")),
        "authentication",
        AUTHENTICATION_FAILURE_MESSAGE,
    );
    assert!(
        !stderr_auth.to_string().contains("401 Unauthorized"),
        "{stderr_auth}"
    );

    // 19. Non-JSON stdout with exit 0.
    let no_result = call(&mut server, "claude", json!({"prompt": "19"}));
    assert_observed_failure(
        &no_result,
        "unknown",
        "without evidence of a completed turn",
    );

    // 20. Terminal noise around valid JSON answer containing error-like words succeeds.
    let answer = call(&mut server, "claude", json!({"prompt": "20"}));
    assert_eq!(answer["observed"]["state"], "finished", "{answer}");
    assert_eq!(
        answer["reported"]["content"],
        "Not logged in · You've hit your limit."
    );
    assert!(answer["reported"]["failure"].is_null());
    fleet.assert_steps_remaining("claude", 0);
}

#[test]
fn claude_filters_usage_cost_and_model_from_untrusted_json() {
    let fleet = Fleet::new(&["claude"]);
    let multi_and_bloated = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": "done",
        "total_cost_usd": -5.0,
        "usage": {
            "input_tokens": "not-a-number",
            "output_tokens": 9,
            "junk": "z".repeat(50_000),
            "nested": {"a": 1}
        },
        "modelUsage": {
            "claude-sonnet-5-5": {"inputTokens": 10},
            "claude-haiku-4-5": {"inputTokens": 5}
        }
    })
    .to_string();
    let unsafe_model_and_junk = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "result": "done",
        "total_cost_usd": "free",
        "usage": {"junk": 1},
        "modelUsage": {"--evil-flag": {"inputTokens": 1}}
    })
    .to_string();
    fleet.script(
        "claude",
        json!([
            {"stdout": multi_and_bloated},
            {"stdout": unsafe_model_and_junk}
        ]),
    );
    let mut server = fleet.server("codex");

    let first = call(&mut server, "claude", json!({"prompt": "first"}));
    assert_eq!(first["observed"]["state"], "finished", "{first}");
    assert!(first["reported"]["model"].is_null());
    assert!(first["reported"]["session_cost_usd"].is_null());
    assert_eq!(first["reported"]["usage"], json!({"output_tokens": 9}));

    let second = call(&mut server, "claude", json!({"prompt": "second"}));
    assert_eq!(second["observed"]["state"], "finished", "{second}");
    assert!(second["reported"]["model"].is_null());
    assert!(second["reported"]["usage"].is_null());
    assert!(second["reported"]["session_cost_usd"].is_null());
    fleet.assert_steps_remaining("claude", 0);
}

#[test]
fn claude_maps_not_found_depth_limit_timeout_cancellation_and_capture_failures() {
    // 1. not_found when claude binary is absent.
    let absent = Fleet::new(&[]);
    let mut absent_server = absent.server("codex");
    let not_found = call(&mut absent_server, "claude", json!({"prompt": "missing"}));
    assert_observed_failure(&not_found, "not_found", "Could not find claude");

    // 2. depth_limit when recursion markers are present.
    for depth_var in ["PROXENOS_DEPTH", "CLAUDE_DELEGATOR_CLAUDE_DEPTH"] {
        let guarded = Fleet::new(&["claude"]);
        let mut cmd = guarded.command("codex");
        cmd.env(depth_var, "1");
        let mut server = Server::start_command(cmd);
        server.initialize("2025-11-25");
        let blocked = call(&mut server, "claude", json!({"prompt": "nested"}));
        assert_observed_failure(
            &blocked,
            "depth_limit",
            "delegated CLI cannot start another",
        );
        assert!(guarded.records().is_empty());
    }

    // 3. capture (stdout beyond 4 MiB without terminal result), timeout, and cancellation.
    let fleet = Fleet::new(&["claude"]);
    let overflow = "x".repeat((4 << 20) + 64);
    let timed_ready = fleet.root.join("timed.ready");
    let cancel_ready = fleet.root.join("cancel.ready");
    fleet.script(
        "claude",
        json!([
            {"stdout": overflow, "code": 0},
            {"stdout": "working", "ready": timed_ready, "sleep_ms": 60_000},
            {"stderr": "working", "ready": cancel_ready, "sleep_ms": 60_000}
        ]),
    );
    let mut server = fleet.server("codex");
    server.request(1, "tools/list", json!({}));

    let captured = call(&mut server, "claude", json!({"prompt": "flood"}));
    assert_observed_failure(&captured, "capture", "beyond the capture limit");
    fleet.assert_steps_remaining("claude", 2);

    server.start_call(10, "claude", json!({"prompt": "slow", "timeout_ms": 4_000}));
    let timed: Value = serde_json::from_str(text(&server.response(10))).unwrap();
    assert_observed_failure(&timed, "timeout", "exceeded the call's deadline");
    assert!(
        timed_ready.exists(),
        "timed stub did not start before deadline: {timed}"
    );
    assert!(
        timed["observed"]["passed"].is_object(),
        "timed call did not launch provider: {timed}"
    );
    assert_eq!(
        timed["observed"]["attempts"][0]["stdout_bytes"], 7,
        "{timed}"
    );
    fleet.assert_steps_remaining("claude", 1);

    server.start_call(11, "claude", json!({"prompt": "cancel"}));
    let until = Instant::now() + WAIT;
    while !cancel_ready.exists() {
        assert!(Instant::now() < until, "cancelled stub did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    server.close_input();
    let cancelled: Value = serde_json::from_str(text(&server.response(11))).unwrap();
    assert_observed_failure(&cancelled, "cancellation", "was cancelled");
    fleet.assert_steps_remaining("claude", 0);
}

fn flag_values<'a>(args: &[&'a str], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1])
        .collect()
}

#[test]
fn claude_forwards_project_deny_rules_under_read_only_and_fails_closed_on_bad_settings() {
    let fleet = Fleet::new(&["claude"]);
    let cwd = fleet.root.join("repo");
    let claude_dir = cwd.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let project = claude_dir.join("settings.json");
    let local = claude_dir.join("settings.local.json");
    fleet.script(
        "claude",
        json!([
            {"stdout": START_OK},
            {"stdout": RESUME_OK},
            {"stdout": START_OK},
            {"stdout": RESUME_OK}
        ]),
    );
    let mut server = fleet.server("codex");

    // 1. Valid project + local permissions.deny rules are forwarded as repeated --disallowedTools
    //    on both start and reply without duplication.
    fs::write(
        &project,
        json!({"permissions": {"deny": ["Read(./.env)", "Edit(./secrets/**)"]}}).to_string(),
    )
    .unwrap();
    fs::write(
        &local,
        json!({"permissions": {"deny": ["Bash(rm -rf *)"]}}).to_string(),
    )
    .unwrap();
    let start = call(
        &mut server,
        "claude",
        json!({"prompt": "review", "cwd": cwd, "access": "read-only"}),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    let reply = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": SESSION_ID, "prompt": "more"}),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");

    // 2. Settings files without permissions or without deny are accepted with no --disallowedTools.
    fs::write(&project, "{}").unwrap();
    fs::write(&local, r#"{"permissions":{"allow":["Read"]}}"#).unwrap();
    let no_deny = call(
        &mut server,
        "claude",
        json!({"prompt": "clean", "cwd": cwd, "access": "read-only"}),
    );
    assert_eq!(no_deny["observed"]["state"], "finished", "{no_deny}");
    fs::remove_file(&local).unwrap();

    // 3. Malformed, non-object, bad-schema, flag-like, NUL, oversized, over-limit, non-regular,
    //    and unreadable settings files fail closed with invalid_request before dispatch.
    let before_failures = invocations(&fleet).len();
    let too_long_rule = "R".repeat(513);
    let too_many_rules: Vec<String> = (0..129).map(|i| format!("Read(./secret-{i})")).collect();
    let oversized_json = format!("{{\"note\":\"{}\"}}", "a".repeat(65 * 1024));
    for (body, detail) in [
        (
            "{not valid json".to_owned(),
            "Malformed Claude settings file",
        ),
        ("[]".to_owned(), "must contain a JSON object"),
        (
            r#"{"permissions":"none"}"#.to_owned(),
            "non-object permissions field",
        ),
        (
            r#"{"permissions":{"deny":"Read(./.env)"}}"#.to_owned(),
            "non-array permissions.deny field",
        ),
        (
            r#"{"permissions":{"deny":["--model"]}}"#.to_owned(),
            "invalid permissions.deny entry",
        ),
        (
            r#"{"permissions":{"deny":["  -x"]}}"#.to_owned(),
            "invalid permissions.deny entry",
        ),
        (
            r#"{"permissions":{"deny":["Read(\u0000)"]}}"#.to_owned(),
            "invalid permissions.deny entry",
        ),
        (
            r#"{"permissions":{"deny":[123]}}"#.to_owned(),
            "invalid permissions.deny entry",
        ),
        (
            r#"{"permissions":{"deny":["   "]}}"#.to_owned(),
            "invalid permissions.deny entry",
        ),
        (
            json!({"permissions": {"deny": [too_long_rule]}}).to_string(),
            "invalid permissions.deny entry",
        ),
        (
            json!({"permissions": {"deny": too_many_rules}}).to_string(),
            "exceed the limit of 128 permissions.deny entries",
        ),
        (oversized_json, "exceeds size limit"),
    ] {
        fs::write(&project, body).unwrap();
        let rejected = call(
            &mut server,
            "claude",
            json!({"prompt": "blocked", "cwd": cwd, "access": "read-only"}),
        );
        assert_observed_failure(&rejected, "invalid_request", detail);
    }

    // Non-regular settings path (directory).
    fs::remove_file(&project).unwrap();
    fs::create_dir(&project).unwrap();
    let dir_err = call(
        &mut server,
        "claude",
        json!({"prompt": "dir", "cwd": cwd, "access": "read-only"}),
    );
    assert_observed_failure(&dir_err, "invalid_request", "not a regular file");
    fs::remove_dir(&project).unwrap();

    // Unreadable settings file and symlink (Unix) / exclusively locked file (Windows).
    fs::write(&project, r#"{"permissions":{"deny":["Read(./.env)"]}}"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&project, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = call(
            &mut server,
            "claude",
            json!({"prompt": "unreadable", "cwd": cwd, "access": "read-only"}),
        );
        assert_observed_failure(
            &unreadable,
            "invalid_request",
            "Cannot read Claude settings",
        );
        fs::set_permissions(&project, fs::Permissions::from_mode(0o600)).unwrap();

        let target = fleet.root.join("outside.json");
        fs::write(&target, "{}").unwrap();
        fs::remove_file(&project).unwrap();
        std::os::unix::fs::symlink(&target, &project).unwrap();
        let symlink_err = call(
            &mut server,
            "claude",
            json!({"prompt": "symlink", "cwd": cwd, "access": "read-only"}),
        );
        assert_observed_failure(&symlink_err, "invalid_request", "not a regular file");
        fs::remove_file(&project).unwrap();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _locked = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&project)
            .unwrap();
        let unreadable = call(
            &mut server,
            "claude",
            json!({"prompt": "locked", "cwd": cwd, "access": "read-only"}),
        );
        assert_observed_failure(
            &unreadable,
            "invalid_request",
            "Cannot read Claude settings",
        );
    }
    assert_eq!(invocations(&fleet).len(), before_failures);

    // 4. Full-access override does not parse project settings or pass --disallowedTools.
    fs::write(&project, "{not valid json").unwrap();
    let full = call(
        &mut server,
        "claude-reply",
        json!({"thread_id": SESSION_ID, "prompt": "full", "access": "full"}),
    );
    assert_eq!(full["observed"]["state"], "finished", "{full}");
    fleet.assert_steps_remaining("claude", 0);

    let records = invocations(&fleet);
    assert_eq!(records.len(), 4);
    for record in &records[..2] {
        let args = argv(record);
        assert_eq!(flag_value(&args, "--setting-sources"), Some("user"));
        assert_eq!(
            flag_values(&args, "--disallowedTools"),
            vec!["Read(./.env)", "Edit(./secrets/**)", "Bash(rm -rf *)"]
        );
    }
    assert!(flag_values(&argv(&records[2]), "--disallowedTools").is_empty());
    assert!(flag_value(&argv(&records[3]), "--setting-sources").is_none());
    assert!(flag_values(&argv(&records[3]), "--disallowedTools").is_empty());
}
