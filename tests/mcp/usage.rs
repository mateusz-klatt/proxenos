//! Account reads go through the compiled MCP server and local providers with inline synthetic account responses.
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::Fleet;

fn usage(server: &mut Server, args: Value) -> Value {
    let frame = server.call(7, "usage", args);
    assert_ne!(frame["result"]["isError"], true, "{frame}");
    let body: Value = serde_json::from_str(text(&frame)).unwrap();
    assert_eq!(body["schema_version"], 1);
    body["providers"].clone()
}

/// Wrap an inline synthetic Codex app-server JSON-RPC response in a scripted dialogue.
fn synthetic_rpc(result: Value) -> Value {
    json!({"rpc":[
        {"expect":"initialize","send":format!("{}\n",json!({"id":1,"result":{"userAgent":"synthetic-fixture","codexHome":"private-path"}}))},
        {"expect":"initialized"},
        {"expect":"account/rateLimits/read","send":format!("{}\n{result}\n", json!({"id":2,"method":"account/updated","params":{"private":"unrelated"}}))}
    ]})
}

#[test]
fn shared_usage_reports_selected_windows_and_plan_without_private_fields() {
    let fleet = Fleet::new(&["codex", "cursor-agent"]);
    // Synthetic Codex GetAccountRateLimitsResponse payload.
    let snapshot = json!({
        "rateLimits":{"limitId":"codex","planType":"plus","primary":{"usedPercent":99}},
        "rateLimitsByLimitId":{
            "codex":{"primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1700000000}},
            "codex_bengalfox":{"primary":null,"secondary":{"usedPercent":62,"windowDurationMins":null,"resetsAt":null}}
        },
        "ordinaryUsageAllowed":false,"accountId":"private-account",
        "rateLimitResetCredits":{"credits":[{"description":"private-credit"}]}
    });
    let result = json!({"id":2,"result":snapshot});
    fleet.script(
        "codex",
        json!([synthetic_rpc(result.clone()), synthetic_rpc(result)]),
    );
    // Synthetic Cursor `about --format json` payload.
    let about = json!({"subscriptionTier":"Pro Plus","userEmail":"private@example.invalid","lastRequestId":"private-request"});
    fleet.script(
        "cursor-agent",
        json!([{"stdout":about.to_string()},{"stdout":about.to_string()}]),
    );
    let mut server = fleet.server("codex");
    let listing = server.request(1, "tools/list", json!({}));
    let tool = listing["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "usage")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["providers"]["items"]["enum"],
        json!(["codex", "cursor"])
    );
    let expected_windows = json!([
        {"bucket":"codex","period":"primary","used_percent":25,"duration_minutes":300,"resets_at_unix_seconds":1700000000},
        {"bucket":"codex_bengalfox","period":"secondary","used_percent":62,"duration_minutes":null,"resets_at_unix_seconds":null}
    ]);
    let summary = usage(&mut server, json!({"providers":["codex","cursor"]}));
    assert_eq!(
        summary,
        json!({
            "codex":{
                "status":"available",
                "plan":"plus",
                "windows":expected_windows,
                "ordinary_usage_allowed":false,
                "rate_limit_reached_type":null,
                "spend_control_reached":null
            },
            "cursor":{"status":"plan_only","plan":"Pro Plus","windows":null}
        })
    );
    let full = usage(
        &mut server,
        json!({"providers":["cursor","codex"],"full":true}),
    );
    let mut expected = summary;
    expected["codex"]["model_prices"] = Value::Null;
    expected["cursor"]["model_prices"] = Value::Null;
    assert_eq!(full, expected);
    let requests = std::fs::read_to_string(fleet.root.join("codex.rpc.jsonl")).unwrap();
    let frames: Vec<Value> = requests
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 6);
    for [initialize, initialized, read] in frames.as_chunks::<3>().0 {
        assert_eq!(initialize["id"], 1);
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(initialized["method"], "initialized");
        assert_eq!(
            read,
            &json!({"id":2,"method":"account/rateLimits/read","params":{"excludeResetCreditDetails":true}})
        );
    }
    for invocation in fleet.records() {
        let args = &invocation["args"];
        assert!(
            args == &json!(["--version"])
                || args == &json!(["app-server", "--listen", "stdio://"])
                || args == &json!(["about", "--format", "json"]),
            "{invocation}"
        );
    }
}

#[test]
fn malformed_requests_do_not_launch_a_cli_and_missing_sources_stay_independent() {
    let fleet = Fleet::new(&["cursor-agent"]);
    let mut server = fleet.server("cursor");
    for args in [
        json!({}),
        json!({"providers":[]}),
        json!({"providers":["cursor","cursor"]}),
        json!({"providers":["grok"]}),
        json!({"providers":["cursor"],"full":"yes"}),
        json!({"providers":["codex"],"view":true}),
    ] {
        let frame = server.call(7, "usage", args);
        assert_eq!(frame["result"]["isError"], true);
        let err: Value = serde_json::from_str(text(&frame)).unwrap();
        assert_eq!(err["failure"]["kind"], "invalid_request");
    }
    assert!(fleet.records().is_empty());
    // Synthetic Cursor response with null subscriptionTier.
    fleet.script(
        "cursor-agent",
        json!([{"stdout":"{\"subscriptionTier\":null}"}]),
    );
    assert_eq!(
        usage(&mut server, json!({"providers":["codex","cursor"]})),
        json!({
            "codex":{"status":"unavailable","reason":"not_found"},
            "cursor":{"status":"unavailable","reason":"plan_not_reported"}
        })
    );
}

#[test]
fn cursor_refuses_incomplete_nonzero_and_malformed_account_output() {
    let fleet = Fleet::new(&["cursor-agent"]);
    // Synthetic Cursor responses exercising parse, empty-tier, capture-limit and exit-status errors.
    let prefix = r#"{"subscriptionTier":"Pro"}"#;
    fleet.script(
        "cursor-agent",
        json!([
            {"stdout":"not JSON"},
            {"stdout":"{}"},
            {"stdout":r#"{"subscriptionTier":""}"#},
            {"stdout":r#"{"subscriptionTier":123}"#},
            {"stdout":format!("{prefix}{}\n{{}}"," ".repeat(256<<10))},
            {"code":1,"stdout":prefix,"stderr":"private-account"}
        ]),
    );
    let mut server = fleet.server("codex");
    for reason in [
        "invalid_report",
        "invalid_report",
        "invalid_report",
        "invalid_report",
        "capture",
        "unavailable",
    ] {
        assert_eq!(
            usage(&mut server, json!({"providers":["cursor"]})),
            json!({"cursor":{"status":"unavailable","reason":reason}})
        );
    }
}

#[test]
fn uuid_and_token_shaped_labels_never_reach_the_client() {
    let fleet = Fleet::new(&["codex", "cursor-agent"]);
    let uuid_label = "550e8400-e29b-41d4-a716-446655440000";
    let token_label = "tok_live_secret_9f8e7d6c";
    let email_label = "user@example.invalid";
    let single_token = "bearer-secret-single-bucket";

    // Synthetic Cursor responses: documented aliases plus UUID, token and email strings.
    fleet.script(
        "cursor-agent",
        json!([
            {"stdout":json!({"subscriptionTier":"Pro+"}).to_string()},
            {"stdout":json!({"subscriptionTier":uuid_label}).to_string()},
            {"stdout":json!({"subscriptionTier":token_label}).to_string()},
            {"stdout":json!({"subscriptionTier":email_label}).to_string()},
        ]),
    );
    // Synthetic Codex responses: documented names alongside single, empty and multiple unknown buckets/plans.
    fleet.script(
        "codex",
        json!([
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{"limitId":"","planType":uuid_label,"rateLimitReachedType":token_label},
                "rateLimitsByLimitId":{
                    "codex":{"primary":{"usedPercent":10}},
                    "": {"secondary":{"usedPercent":20}},
                    uuid_label: {"primary":{"usedPercent":30}},
                    token_label: {"secondary":{"usedPercent":40}}
                }
            }})),
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{"planType":"prolite"},
                "rateLimitsByLimitId":{
                    "codex_other":{"primary":{"usedPercent":15}},
                    uuid_label: {"secondary":{"usedPercent":25}}
                }
            }})),
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{"limitId":single_token,"planType":"","primary":{"usedPercent":55}}
            }})),
        ]),
    );
    let mut server = fleet.server("codex");
    for expected_plan in ["Pro Plus", "other", "other", "other"] {
        let frame = server.call(7, "usage", json!({"providers":["cursor"]}));
        let raw = text(&frame);
        for forbidden in [uuid_label, token_label, email_label] {
            assert!(!raw.contains(forbidden), "leaked label {forbidden}: {raw}");
        }
        let body: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            body["providers"]["cursor"],
            json!({"status":"plan_only","plan":expected_plan,"windows":null})
        );
    }

    let first_frame = server.call(8, "usage", json!({"providers":["codex"]}));
    let first_raw = text(&first_frame);
    for forbidden in [uuid_label, token_label] {
        assert!(
            !first_raw.contains(forbidden),
            "leaked label {forbidden}: {first_raw}"
        );
    }
    let first: Value = serde_json::from_str(first_raw).unwrap();
    // BTreeMap orders keys lexicographically: "", uuid_label, "codex", token_label.
    assert_eq!(
        first["providers"]["codex"],
        json!({
            "status":"available",
            "plan":"other",
            "windows":[
                {"bucket":null,"period":"secondary","used_percent":20,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"other_1","period":"primary","used_percent":30,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"codex","period":"primary","used_percent":10,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"other_2","period":"secondary","used_percent":40,"duration_minutes":null,"resets_at_unix_seconds":null}
            ],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":"other",
            "spend_control_reached":null
        })
    );

    let second_frame = server.call(9, "usage", json!({"providers":["codex"]}));
    let second_raw = text(&second_frame);
    assert!(
        !second_raw.contains(uuid_label),
        "leaked label {uuid_label}: {second_raw}"
    );
    let second: Value = serde_json::from_str(second_raw).unwrap();
    assert_eq!(
        second["providers"]["codex"],
        json!({
            "status":"available",
            "plan":"prolite",
            "windows":[
                {"bucket":"other","period":"secondary","used_percent":25,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"codex_other","period":"primary","used_percent":15,"duration_minutes":null,"resets_at_unix_seconds":null}
            ],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null
        })
    );

    let third_frame = server.call(10, "usage", json!({"providers":["codex"]}));
    let third_raw = text(&third_frame);
    assert!(
        !third_raw.contains(single_token),
        "leaked label {single_token}: {third_raw}"
    );
    let third: Value = serde_json::from_str(third_raw).unwrap();
    assert_eq!(
        third["providers"]["codex"],
        json!({
            "status":"available",
            "plan":null,
            "windows":[
                {"bucket":"other","period":"primary","used_percent":55,"duration_minutes":null,"resets_at_unix_seconds":null}
            ],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null
        })
    );
}

#[test]
fn colliding_near_duplicate_codex_buckets_get_distinct_labels_and_separate_limits() {
    let fleet = Fleet::new(&["codex"]);
    let near_case = "Codex";
    let near_hyphen = "codex-other";
    // Synthetic Codex payload mixing exact bucket IDs with case/hyphen near-duplicates.
    fleet.script(
        "codex",
        json!([synthetic_rpc(json!({"id":2,"result":{
            "rateLimits":{},
            "rateLimitsByLimitId":{
                "codex":{"primary":{"usedPercent":10},"spendControlReached":false},
                near_case:{"secondary":{"usedPercent":35}},
                near_hyphen:{"primary":{"usedPercent":90},"rateLimitReachedType":"rate_limit_reached","spendControlReached":true},
                "codex_other":{"primary":{"usedPercent":15},"rateLimitReachedType":null,"spendControlReached":false}
            }
        }}))]),
    );
    let mut server = fleet.server("cursor");
    let frame = server.call(7, "usage", json!({"providers":["codex"]}));
    let raw = text(&frame);
    for forbidden in [near_case, near_hyphen] {
        assert!(!raw.contains(forbidden), "leaked label {forbidden}: {raw}");
    }
    let body: Value = serde_json::from_str(raw).unwrap();
    // BTreeMap orders keys lexicographically: "Codex", "codex", "codex-other", "codex_other".
    assert_eq!(
        body["providers"]["codex"],
        json!({
            "status":"available",
            "plan":null,
            "windows":[
                {"bucket":"other_1","period":"secondary","used_percent":35,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"codex","period":"primary","used_percent":10,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"other_2","period":"primary","used_percent":90,"duration_minutes":null,"resets_at_unix_seconds":null},
                {"bucket":"codex_other","period":"primary","used_percent":15,"duration_minutes":null,"resets_at_unix_seconds":null}
            ],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null,
            "bucket_limits":[
                {"bucket":"codex","rate_limit_reached_type":null,"spend_control_reached":false},
                {"bucket":"other_2","rate_limit_reached_type":"rate_limit_reached","spend_control_reached":true},
                {"bucket":"codex_other","rate_limit_reached_type":null,"spend_control_reached":false}
            ]
        })
    );
}

#[test]
fn codex_preserves_explicit_limit_and_permission_signals_without_windows() {
    let fleet = Fleet::new(&["codex"]);
    // Synthetic Codex responses comparing no-decision empty payloads against explicit exhaustion.
    fleet.script(
        "codex",
        json!([
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{}}})),
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{"rateLimitReachedType":"rate_limit_reached"}}})),
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{},"ordinaryUsageAllowed":false}})),
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{"spendControlReached":false},"ordinaryUsageAllowed":true}})),
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{},
                "rateLimitsByLimitId":{
                    "codex":{"spendControlReached":false}
                }
            }})),
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{},
                "rateLimitsByLimitId":{
                    "codex_bengalfox":{"spendControlReached":true}
                }
            }})),
            synthetic_rpc(json!({"id":2,"result":{
                "rateLimits":{},
                "rateLimitsByLimitId":{
                    "codex_bengalfox":{"rateLimitReachedType":"workspace_member_usage_limit_reached","spendControlReached":true}
                }
            }}))
        ]),
    );
    let mut server = fleet.server("cursor");

    let empty = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        empty["codex"],
        json!({
            "status":"no_windows",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null
        })
    );

    let reached = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        reached["codex"],
        json!({
            "status":"limit_reached",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":"rate_limit_reached",
            "spend_control_reached":null
        })
    );
    assert_ne!(reached["codex"], empty["codex"]);

    let disallowed = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        disallowed["codex"],
        json!({
            "status":"limit_reached",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":false,
            "rate_limit_reached_type":null,
            "spend_control_reached":null
        })
    );

    let allowed = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        allowed["codex"],
        json!({
            "status":"no_windows",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":true,
            "rate_limit_reached_type":null,
            "spend_control_reached":false
        })
    );

    let bucket_spend_ok = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        bucket_spend_ok["codex"],
        json!({
            "status":"no_windows",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null,
            "bucket_limits":[
                {
                    "bucket":"codex",
                    "rate_limit_reached_type":null,
                    "spend_control_reached":false
                }
            ]
        })
    );

    let bucket_spend_reached = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        bucket_spend_reached["codex"],
        json!({
            "status":"limit_reached",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null,
            "bucket_limits":[
                {
                    "bucket":"codex_bengalfox",
                    "rate_limit_reached_type":null,
                    "spend_control_reached":true
                }
            ]
        })
    );

    let bucket_exhausted = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        bucket_exhausted["codex"],
        json!({
            "status":"limit_reached",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null,
            "bucket_limits":[
                {
                    "bucket":"codex_bengalfox",
                    "rate_limit_reached_type":"workspace_member_usage_limit_reached",
                    "spend_control_reached":true
                }
            ]
        })
    );
}

#[test]
fn codex_falls_back_to_the_single_bucket_and_preserves_unavailable_windows() {
    let fleet = Fleet::new(&["codex"]);
    // Synthetic Codex responses for single-bucket fallback, empty response and malformed window.
    fleet.script("codex",json!([
        synthetic_rpc(json!({"id":2,"result":{"rateLimits":{"limitId":"codex","secondary":{"usedPercent":0}}}})),
        synthetic_rpc(json!({"id":2,"result":{"rateLimits":{},"rateLimitsByLimitId":{}}})),
        synthetic_rpc(json!({"id":2,"result":{"rateLimits":{"primary":{"usedPercent":"50"}}}}))
    ]));
    let mut server = fleet.server("codex");
    let report = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        report["codex"]["windows"],
        json!([{"bucket":"codex","period":"secondary","used_percent":0,"duration_minutes":null,"resets_at_unix_seconds":null}])
    );
    let none = usage(&mut server, json!({"providers":["codex"],"full":true}));
    assert_eq!(
        none["codex"],
        json!({
            "status":"no_windows",
            "plan":null,
            "windows":[],
            "ordinary_usage_allowed":null,
            "rate_limit_reached_type":null,
            "spend_control_reached":null,
            "model_prices":null
        })
    );
    let invalid = usage(&mut server, json!({"providers":["codex"]}));
    assert_eq!(
        invalid["codex"],
        json!({"status":"unavailable","reason":"invalid_report"})
    );
}

#[test]
fn codex_requires_a_matching_rpc_response_and_keeps_account_errors_private() {
    let fleet = Fleet::new(&["codex"]);
    let err = |id, code, message| json!({"id":id,"error":{"code":code,"message":message}});
    // Synthetic Codex RPC error and framing responses.
    let mut nonzero = synthetic_rpc(json!({"id":2,"result":{"rateLimits":{}}}));
    nonzero["code"] = json!(7);
    let mut tail = synthetic_rpc(json!({"id":2,"result":{"rateLimits":{}}}));
    tail["stdout"] = json!(" ".repeat((256 << 10) + 1));
    fleet.script(
        "codex",
        json!([
            synthetic_rpc(err(2,-32600,"codex account authentication required to read rate limits")),
            synthetic_rpc(err(2,-32600,"unrelated private failure")),
            // Synthetic malformed/oversized JSON-RPC handshake frames.
            {"rpc":[{"expect":"initialize","send":"not JSON\n"}]},
            {"rpc":[{"expect":"initialize","send":"{\"id\":999,\"result\":{}}\n"}]},
            {"rpc":[{"expect":"initialize","send":"{\"id\":1}\n"}]},
            {"rpc":[{"expect":"initialize","send":"x".repeat((256<<10)+1)}]},
            {"stderr":"x".repeat((256<<10)+1),"sleep_ms":1000}, nonzero, tail
        ]),
    );
    let mut server = fleet.server("cursor");
    for (i, reason) in [
        "authentication",
        "unavailable",
        "invalid_report",
        "invalid_report",
        "invalid_report",
        "capture",
        "capture",
        "unavailable",
        "capture",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            usage(&mut server, json!({"providers":["codex"]})),
            json!({"codex":{"status":"unavailable","reason":reason}}),
            "{i}: {reason}"
        );
    }
}

#[test]
fn a_reported_account_error_allows_the_app_server_to_exit_normally() {
    let fleet = Fleet::new(&["codex"]);
    let lock = fleet.root.join("rpc-error");
    let release = fleet.root.join("allow-exit");
    // Synthetic Codex RPC authentication error step.
    let mut step = synthetic_rpc(
        json!({"id":2,"error":{"code":-32600,"message":"codex account authentication required to read rate limits"}}),
    );
    step["hold_file"] = json!(lock);
    step["wait_for"] = json!(release);
    fleet.script("codex", json!([step]));
    let mut server = fleet.server("cursor");
    server.start_call(14, "usage", json!({"providers":["codex"]}));
    let until = Instant::now() + WAIT;
    while !lock.with_extension("ready").exists() {
        assert!(
            Instant::now() < until,
            "app-server failed to finish its RPC exchange"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        server
            .next_frame(Instant::now() + Duration::from_millis(150))
            .is_none()
    );
    let witness = std::fs::File::options().write(true).open(&lock).unwrap();
    assert!(matches!(
        witness.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    std::fs::write(release, []).unwrap();
    let report: Value = serde_json::from_str(text(&server.response(14))).unwrap();
    assert_eq!(
        report["providers"]["codex"],
        json!({"status":"unavailable","reason":"authentication"})
    );
    assert!(witness.try_lock().is_ok());
}

#[test]
fn a_closed_connection_stops_each_running_account_read() {
    for (source, cli, next) in [
        ("codex", "codex", "cursor"),
        ("cursor", "cursor-agent", "codex"),
    ] {
        let fleet = Fleet::new(&[cli]);
        let lock = fleet.root.join("holding");
        fleet.script(cli, json!([{"hold_file":lock,"sleep_ms":60_000}]));
        let mut server = fleet.server(source);
        server.start_call(12, "usage", json!({"providers":[source,next]}));
        let until = Instant::now() + WAIT;
        while !lock.with_extension("ready").is_file() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        server.close_input();
        let report: Value = serde_json::from_str(text(&server.response(12))).unwrap();
        assert_eq!(
            report["providers"][source]["reason"], "cancelled",
            "{report}"
        );
        assert_eq!(report["providers"][next]["reason"], "cancelled", "{report}");
        assert!(server.finish().0.success());
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&lock)
                .unwrap()
                .try_lock()
                .is_ok()
        );
    }
}

#[test]
fn a_timeout_releases_the_process_for_each_account_source() {
    let fleet = Fleet::new(&["codex", "cursor-agent"]);
    let mut server = fleet.server("cursor");
    for (id, source, cli) in [(20, "codex", "codex"), (21, "cursor", "cursor-agent")] {
        let lock = fleet.root.join(format!("{source}.lock"));
        fleet.script(cli, json!([{"hold_file":lock,"sleep_ms":60_000}]));
        server.start_call(id, "usage", json!({"providers":[source]}));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ids = Vec::new();
    while ids.len() < 2 {
        let frame = server
            .next_frame(deadline)
            .expect("both usage requests should time out");
        let id = frame["id"].as_i64().unwrap();
        let source = if id == 20 { "codex" } else { "cursor" };
        let report: Value = serde_json::from_str(text(&frame)).unwrap();
        assert_eq!(report["providers"][source]["reason"], "timeout", "{report}");
        let lock = fleet.root.join(format!("{source}.lock"));
        assert!(
            std::fs::File::options()
                .write(true)
                .open(lock)
                .unwrap()
                .try_lock()
                .is_ok()
        );
        ids.push(id);
    }
    ids.sort_unstable();
    assert_eq!(ids, [20, 21]);
}

#[test]
fn an_exited_account_reader_cannot_leave_its_output_pipes_with_a_descendant() {
    let fleet = Fleet::new(&["codex", "cursor-agent"]);
    let mut server = fleet.server("codex");
    for (id, source, cli, mut step) in [
        (
            13,
            "codex",
            "codex",
            // Synthetic Codex payload while a descendant holds the output pipes.
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{}}})),
        ),
        (
            14,
            "cursor",
            "cursor-agent",
            // Synthetic Cursor JSON payload printed before exiting 0 while a descendant holds stdout.
            json!({"stdout":r#"{"subscriptionTier":"Pro"}"#}),
        ),
    ] {
        let lock = fleet.root.join(format!("{source}-descendant"));
        let release = fleet.root.join(format!("{source}-release-leader"));
        step["pipe_holding_descendant"] = json!(lock);
        step["wait_for"] = json!(release);
        fleet.script(cli, json!([step]));
        server.start_call(id, "usage", json!({"providers":[source]}));
        let until = Instant::now() + WAIT;
        while !lock.with_extension("ready").exists() {
            assert!(
                Instant::now() < until,
                "{source} descendant did not acquire its lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let witness = std::fs::File::options().write(true).open(&lock).unwrap();
        assert!(matches!(
            witness.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        std::fs::write(release, []).unwrap();
        let started = Instant::now();
        let report: Value = serde_json::from_str(text(&server.response(id))).unwrap();
        assert_eq!(
            report["providers"][source],
            json!({"status":"unavailable","reason":"timeout"})
        );
        assert!(started.elapsed() >= Duration::from_millis(1900));
        let until = Instant::now() + WAIT;
        loop {
            match witness.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        Instant::now() < until,
                        "{source} descendant survived its parent"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("could not verify the {source} descendant lock: {error}"),
            }
        }
    }
}

#[test]
fn an_unlaunchable_account_cli_does_not_disclose_os_diagnostics() {
    let fleet = Fleet::new(&[]);
    let mut server = fleet.server("cursor");
    for (source, cli, step) in [
        (
            "codex",
            "codex",
            // Synthetic Codex payload (never reached on an unlaunchable image).
            synthetic_rpc(json!({"id":2,"result":{"rateLimits":{}}})),
        ),
        (
            "cursor",
            "cursor-agent",
            // Synthetic Cursor payload (never reached on an unlaunchable image).
            json!({"stdout":r#"{"subscriptionTier":"Pro"}"#}),
        ),
    ] {
        fleet.script(cli, json!([step]));
        let path = fleet
            .root
            .join("bin")
            .join(format!("{cli}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(crate::provider_support::stub(), &path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        let _locked_image = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::File::options()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap()
        };
        assert!(path.is_file(), "CLI discovery should see the private copy");
        let reports = usage(&mut server, json!({"providers":[source]}));
        assert_eq!(
            reports[source],
            json!({"status":"unavailable","reason":"unavailable"})
        );
        assert!(
            fleet.records().is_empty(),
            "a locked or non-executable CLI started"
        );
        fleet.assert_steps_remaining(cli, 1);
    }
}

#[cfg(windows)]
#[test]
fn windows_unsupported_cursor_launcher_includes_path_free_hint() {
    let fleet = Fleet::new(&[]);
    let bin = fleet.root.join("bin");
    std::fs::write(
        bin.join("cursor-agent.cmd"),
        "@echo off\r\nif exist \"%~dp0versions\\promote-staged.cmd\" call \"%~dp0versions\\promote-staged.cmd\"\r\n",
    )
    .unwrap();
    let mut server = fleet.server("codex");
    let report = usage(&mut server, json!({"providers":["cursor"]}));
    let cursor = &report["cursor"];
    assert_eq!(cursor["status"], "unavailable");
    assert_eq!(cursor["reason"], "unsupported_launcher");
    let hint = cursor["hint"].as_str().expect("fixed hint");
    assert!(hint.contains("PATH"), "{cursor}");
    let root = fleet.root.to_string_lossy();
    assert!(!hint.contains(root.as_ref()), "leaked root path: {hint}");
    assert!(
        !hint.contains('\\') && !hint.contains('/'),
        "hint contained path separator: {hint}"
    );
    assert!(fleet.records().is_empty());
}
