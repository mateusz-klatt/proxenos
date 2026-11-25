use serde_json::{Value, json};
use std::path::Path;

use crate::provider_support::{Fleet, call};

const FINISHED: &str = include_str!("../fixtures/codex/finished.jsonl");
const EMPTY: &str = include_str!("../fixtures/codex/empty.jsonl");
const THREAD: &str = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";

fn invocations(fleet: &Fleet) -> Vec<Value> {
    fleet
        .records()
        .into_iter()
        .filter(|record| record["args"][0] == "exec")
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

#[test]
fn codex_starts_and_resumes_with_the_same_settings_and_exact_prompt() {
    let fleet = Fleet::new(&["codex"]);
    let cwd = fleet.root.join("chosen workspace");
    std::fs::create_dir(&cwd).unwrap();
    fleet.script(
        "codex",
        json!([
            {"stdout":FINISHED, "final_message":"  Recovered from the completed turn.\n"},
            {"stdout":FINISHED},
            {"stdout":FINISHED}
        ]),
    );
    let mut server = fleet.server("claude");
    let task = "--analyze\nthis \"argument\"";
    let instructions = "Keep the delimiters.\n\"Quoted\" \\ \u{007f}";
    let start = call(
        &mut server,
        "codex",
        json!({
            "prompt":task, "mode":"advisory", "model":"test-model", "effort":"high",
            "cwd":cwd, "access":"read-only", "instructions":instructions, "timeout_ms":5_000
        }),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert_eq!(start["reported"]["thread_id"], THREAD);
    assert_eq!(
        start["reported"]["content"],
        "  Recovered from the completed turn.\n"
    );
    assert_eq!(start["reported"]["usage"]["input_tokens"], 17);
    assert!(start["reported"]["model"].is_null());
    assert_eq!(start["observed"]["passed"]["model"], "test-model");

    let reply = call(
        &mut server,
        "codex-reply",
        json!({"thread_id":THREAD, "prompt":"--more"}),
    );
    assert_eq!(reply["observed"]["state"], "finished", "{reply}");
    assert_eq!(reply["reported"]["content"], "The two paragraphs agree.");
    assert!(
        reply["observed"]["attempts"][0]["deadline_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("missing deadline_ms in envelope: {reply}"))
            > 5_000,
        "reply inherited the start timeout: {reply}"
    );
    let refused = call(
        &mut server,
        "codex-reply",
        json!({
            "thread_id":THREAD, "prompt":"act", "mode":"implementation"
        }),
    );
    assert_eq!(refused["observed"]["state"], "failed", "{refused}");
    assert!(refused.to_string().to_lowercase().contains("mode"));
    let amended = call(
        &mut server,
        "codex-reply",
        json!({"thread_id":THREAD,"prompt":"explain","instructions":"Different instructions"}),
    );
    assert_eq!(amended["observed"]["state"], "failed", "{amended}");
    assert!(
        amended["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("instructions")
    );
    let changed = call(
        &mut server,
        "codex-reply",
        json!({
            "thread_id":THREAD, "prompt":"again", "access":"full", "effort":"low", "model":"next"
        }),
    );
    assert_eq!(changed["observed"]["state"], "finished", "{changed}");

    let records = invocations(&fleet);
    assert_eq!(records.len(), 3);
    for (i, record) in records.iter().enumerate() {
        let args = argv(record);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json") && args.contains(&"--skip-git-repo-check"));
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "approval_policy=\"never\""])
        );
        let (model, effort, sandbox) = if i < 2 {
            (
                "test-model",
                "model_reasoning_effort=\"high\"",
                "sandbox_mode=\"read-only\"",
            )
        } else {
            (
                "next",
                "model_reasoning_effort=\"low\"",
                "sandbox_mode=\"danger-full-access\"",
            )
        };
        assert!(args.windows(2).any(|w| w == ["-m", model]));
        assert!(args.windows(2).any(|w| w == ["-c", effort]));
        assert!(args.windows(2).any(|w| w == ["-c", sandbox]));
        assert_eq!(
            Path::new(record["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            cwd.canonicalize().unwrap()
        );
        assert_eq!(record["env"]["CLAUDE_DELEGATOR_CODEX_DEPTH"], "1");
        let guidance = args
            .iter()
            .find_map(|arg| arg.strip_prefix("developer_instructions="));
        if i == 0 {
            assert!(args.ends_with(&["--", task]));
            let guidance: String = serde_json::from_str(guidance.unwrap()).unwrap();
            assert!(guidance.contains(instructions));
        } else {
            assert_eq!(args[1], "resume");
            assert!(guidance.is_none());
            assert!(args.ends_with(&["--", THREAD, if i == 1 { "--more" } else { "again" }]));
        }
    }
}

#[test]
fn codex_requires_a_completion_and_confirms_the_id_on_resume() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script("codex", json!([
        {"stdout":"this is not JSON\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"finished!\"}}\n"},
        {"stdout":FINISHED},
        {"stdout":FINISHED.replace(THREAD, "unrelated-thread")}
    ]));
    let mut server = fleet.server("claude");
    let dash = call(&mut server, "codex", json!({"prompt":"-"}));
    assert_eq!(dash["observed"]["state"], "failed", "{dash}");
    let incomplete = call(&mut server, "codex", json!({"prompt":"check"}));
    assert_eq!(incomplete["observed"]["state"], "failed", "{incomplete}");
    assert!(incomplete["reported"]["content"].is_null());
    let started = call(&mut server, "codex", json!({"prompt":"--help"}));
    assert_eq!(started["observed"]["state"], "finished", "{started}");
    let mismatch = call(
        &mut server,
        "codex-reply",
        json!({"prompt":"continue","thread_id":THREAD}),
    );
    assert_eq!(mismatch["observed"]["state"], "failed", "{mismatch}");
    assert!(mismatch["reported"]["content"].is_null());
    assert_eq!(mismatch["reported"]["failure"]["kind"], "unknown");
    let records = invocations(&fleet);
    assert_eq!(records.len(), 3);
    assert!(!argv(&records[0]).contains(&"-m"));
    assert!(
        !argv(&records[0])
            .iter()
            .any(|arg| arg.starts_with("model_reasoning_effort"))
    );
}

fn failed(message: &str) -> String {
    json!({"type":"turn.failed","error":{"message":message}}).to_string()
}

#[test]
fn codex_classifies_diagnostics_without_treating_a_status_or_a_model_answer_as_quota() {
    let fleet = Fleet::new(&["codex"]);
    let backend = |status: u32, error: Value| {
        failed(&json!({"type":"error","status":status,"error":error}).to_string())
    };
    let cases = [
        (
            backend(
                429,
                json!({"code":"usage_limit_reached","resets_at":1234567890,"message":"Window closed"}),
            ),
            "quota",
            Some("1234567890"),
        ),
        (
            backend(
                429,
                json!({"type":"insufficient_quota","resets_at":"2030-01-01T00:00:00Z","message":"No capacity"}),
            ),
            "quota",
            Some("2030-01-01T00:00:00Z"),
        ),
        (
            backend(429, json!({"message":"Try later"})),
            "unknown",
            None,
        ),
        (
            backend(401, json!({"message":"Session expired"})),
            "authentication",
            None,
        ),
        (
            backend(
                403,
                json!({"code":"invalid_api_key","message":"Invalid credentials"}),
            ),
            "authentication",
            None,
        ),
        (
            backend(
                400,
                json!({"code":"content_filter","message":"Request rejected"}),
            ),
            "refusal",
            None,
        ),
        (
            failed("Request flagged for possible cybersecurity risk"),
            "refusal",
            None,
        ),
        (
            failed("You've hit your usage limit. Please try again at 3:00 PM."),
            "quota",
            Some("3:00 PM"),
        ),
        (failed("The turn failed"), "unknown", None),
        ("{\"type\":\"turn.failed\"}".to_owned(), "unknown", None),
        (
            format!(
                "{}\n{}",
                json!({"type":"error","message":"401 Unauthorized"}),
                json!({"type":"turn.failed"})
            ),
            "authentication",
            None,
        ),
    ];
    let mut script: Vec<_> = cases
        .iter()
        .map(|(stdout, _, _)| json!({"stdout":stdout,"code":1}))
        .collect();
    script.push(json!({"stderr":"unexpected status 401 Unauthorized", "code":1}));
    script.push(json!({"stdout":FINISHED.replace("The two paragraphs agree.", "You've hit your usage limit.")}));
    fleet.script("codex", json!(script));
    let mut server = fleet.server("claude");
    for (_, kind, reset) in cases {
        let result = call(&mut server, "codex", json!({"prompt":"check"}));
        assert_eq!(result["observed"]["state"], "failed", "{result}");
        assert_eq!(result["reported"]["failure"]["kind"], kind, "{result}");
        assert_eq!(result["reported"]["failure"]["reset_at"].as_str(), reset);
    }
    let stderr = call(&mut server, "codex", json!({"prompt":"check"}));
    assert_eq!(
        stderr["reported"]["failure"]["kind"], "authentication",
        "{stderr}"
    );
    let answer = call(&mut server, "codex", json!({"prompt":"check"}));
    assert_eq!(answer["observed"]["state"], "finished", "{answer}");
    assert_eq!(
        answer["reported"]["content"],
        "You've hit your usage limit."
    );
    assert!(answer["reported"]["failure"].is_null());
}

#[test]
fn codex_requests_a_missing_final_answer_once_in_the_original_thread() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script(
        "codex",
        json!([{"stdout":EMPTY}, {"stdout":FINISHED}, {"stdout":EMPTY},{"stdout":EMPTY}]),
    );
    let mut server = fleet.server("claude");
    let answered = call(
        &mut server,
        "codex",
        json!({"prompt":"Change one paragraph."}),
    );
    assert_eq!(answered["observed"]["state"], "finished", "{answered}");
    assert_eq!(
        answered["observed"]["attempts"].as_array().unwrap().len(),
        2
    );
    assert_eq!(answered["reported"]["content"], "The two paragraphs agree.");
    let empty = call(
        &mut server,
        "codex-reply",
        json!({"thread_id":THREAD, "prompt":"Check my edit."}),
    );
    assert_eq!(empty["observed"]["state"], "failed", "{empty}");
    assert_eq!(empty["observed"]["attempts"].as_array().unwrap().len(), 2);
    assert!(
        empty["reported"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("final message")
    );
    let records = invocations(&fleet);
    assert_eq!(records.len(), 4);
    for index in [1, 3] {
        let args = argv(&records[index]);
        assert!(args.starts_with(&["exec", "resume"]));
        assert!(args.windows(2).any(|w| w == ["--", THREAD]));
        assert_ne!(args.last(), argv(&records[index - 1]).last());
    }
}

#[test]
fn codex_retries_only_an_explicit_missing_answer_diagnostic() {
    let fleet = Fleet::new(&["codex"]);
    let started = json!({"type":"thread.started","thread_id":THREAD}).to_string();
    let filter = "Request flagged for possible cybersecurity risk";
    let missing = "Turn ended without a final message.";
    let previous = json!({"type":"error","message":filter});
    let step = |terminal: String| json!({"stdout":format!("{started}\n{terminal}"),"code":1});
    fleet.script(
        "codex",
        json!([
            step(failed(&format!("{filter}; the turn ended without a final message."))),
            step(format!("{previous}\n{}",failed(missing))),
            step(failed("Transport closed without a final message.")),
            step(failed(missing)),{"stdout":FINISHED}
        ]),
    );
    let mut server = fleet.server("claude");
    for kind in ["refusal", "refusal", "unknown"] {
        let result = call(
            &mut server,
            "codex",
            json!({"prompt":"Check the paragraph."}),
        );
        assert_eq!(result["observed"]["state"], "failed", "{result}");
        assert_eq!(result["reported"]["failure"]["kind"], kind);
        assert_eq!(result["observed"]["attempts"].as_array().unwrap().len(), 1);
    }
    let retried = call(
        &mut server,
        "codex",
        json!({"prompt":"Check the paragraph."}),
    );
    assert_eq!(retried["observed"]["state"], "finished", "{retried}");
    assert_eq!(retried["observed"]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(invocations(&fleet).len(), 5);
}

#[test]
fn codex_rejects_non_uuid_thread_ids_on_reply_and_retry() {
    let fleet = Fleet::new(&["codex"]);
    let named_empty = EMPTY.replace(THREAD, "named-session");
    fleet.script("codex", json!([{"stdout": named_empty}]));
    let mut server = fleet.server("claude");
    for bad_id in [
        "named-session",
        "EEB3A731-D53A-4444-8A6A-4D9D89E76073",
        "eeb3a731d53a44448a6a4d9d89e76073",
    ] {
        let rejected = call(
            &mut server,
            "codex-reply",
            json!({
                "thread_id": bad_id, "prompt": "resume", "cwd": fleet.root,
                "access": "read-only", "mode": "advisory"
            }),
        );
        assert_eq!(
            rejected["observed"]["failure"]["kind"], "invalid_request",
            "{bad_id}: {rejected}"
        );
    }
    assert!(
        fleet.records().is_empty(),
        "non-UUID reply IDs must fail before discovery or dispatch"
    );
    let no_retry = call(&mut server, "codex", json!({"prompt": "work"}));
    assert_eq!(no_retry["observed"]["state"], "failed", "{no_retry}");
    assert_eq!(
        no_retry["observed"]["attempts"]
            .as_array()
            .unwrap_or_else(|| panic!("missing attempts: {no_retry}"))
            .len(),
        1
    );
    assert!(
        no_retry["reported"]["thread_id"].is_null(),
        "non-UUID thread_id must not be returned or cached: {no_retry}"
    );
    assert_eq!(invocations(&fleet).len(), 1);
}

#[test]
fn codex_filters_usage_to_expected_scalar_fields_and_drops_invalid_thread_ids() {
    let fleet = Fleet::new(&["codex"]);
    let bloated_usage = format!(
        "{}\n{}\n{}\n",
        json!({"type": "thread.started", "thread_id": "x".repeat(200)}),
        json!({"type": "item.completed", "item": {"type": "agent_message", "text": "done"}}),
        json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": "not-a-number",
                "output_tokens": 9,
                "reasoning_output_tokens": 4,
                "junk": "z".repeat(50_000),
                "nested": {"a": 1}
            }
        })
    );
    let only_junk_usage = format!(
        "{}\n{}\n{}\n",
        json!({"type": "thread.started", "thread_id": THREAD}),
        json!({"type": "item.completed", "item": {"type": "agent_message", "text": "done"}}),
        json!({"type": "turn.completed", "usage": {"junk": 1}})
    );
    fleet.script(
        "codex",
        json!([{"stdout": bloated_usage}, {"stdout": only_junk_usage}]),
    );
    let mut server = fleet.server("claude");
    let first = call(&mut server, "codex", json!({"prompt": "first"}));
    assert_eq!(first["observed"]["state"], "finished", "{first}");
    assert!(first["reported"]["thread_id"].is_null(), "{first}");
    assert_eq!(
        first["reported"]["usage"],
        json!({"output_tokens": 9, "reasoning_output_tokens": 4}),
        "{first}"
    );
    let second = call(&mut server, "codex", json!({"prompt": "second"}));
    assert_eq!(second["observed"]["state"], "finished", "{second}");
    assert_eq!(second["reported"]["thread_id"], THREAD, "{second}");
    assert!(second["reported"]["usage"].is_null(), "{second}");
}

#[test]
fn codex_reads_an_atomically_replaced_final_message_file() {
    let fleet = Fleet::new(&["codex"]);
    let replaced = "Replaced atomically.\n";
    fleet.script(
        "codex",
        json!([{"stdout": FINISHED, "replace_final_message": replaced}]),
    );
    let mut server = fleet.server("claude");
    let result = call(&mut server, "codex", json!({"prompt": "write answer"}));
    assert_eq!(result["observed"]["state"], "finished", "{result}");
    assert_eq!(result["reported"]["content"], replaced);
    assert_eq!(
        result["observed"]["attempts"][0]["provider_file_bytes"],
        replaced.len()
    );
    assert_eq!(
        result["observed"]["attempts"][0]["provider_file_captured"],
        replaced.len()
    );
}

#[test]
fn codex_retry_keeps_confirmed_thread_id_when_second_attempt_fails_early() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script(
        "codex",
        json!([
            {"stdout": EMPTY},
            {"code": 1, "stderr": "transport dropped"}
        ]),
    );
    let mut server = fleet.server("claude");
    let result = call(&mut server, "codex", json!({"prompt": "check"}));
    assert_eq!(result["observed"]["state"], "failed", "{result}");
    assert_eq!(result["observed"]["failure"]["kind"], "abnormal_exit");
    assert_eq!(
        result["observed"]["attempts"]
            .as_array()
            .unwrap_or_else(|| panic!("missing attempts: {result}"))
            .len(),
        2
    );
    assert_eq!(result["reported"]["thread_id"], THREAD, "{result}");
    assert_eq!(result["reported"]["diagnostic"], "transport dropped");
}
