//! Account reads go through the compiled MCP server and local providers with scripted stdout.
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

fn rpc(result: Value) -> Value {
    json!({"rpc":[
        {"expect":"initialize","send":format!("{}\n",json!({"id":1,"result":{"userAgent":"fixture","codexHome":"private-path"}}))},
        {"expect":"initialized"},
        {"expect":"account/rateLimits/read","send":format!("{}\n{result}\n", json!({"id":2,"method":"account/updated","params":{"private":"unrelated"}}))}
    ]})
}

#[test]
fn shared_usage_reports_selected_windows_and_plan_without_private_fields() {
    let fleet = Fleet::new(&["codex", "cursor-agent"]);
    let snapshot = json!({
        "rateLimits":{"limitId":"base","planType":"plus","primary":{"usedPercent":99}},
        "rateLimitsByLimitId":{
            "base":{"primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1700000000}},
            "extra":{"primary":null,"secondary":{"usedPercent":62,"windowDurationMins":null,"resetsAt":null}}
        },
        "ordinaryUsageAllowed":false,"accountId":"private-account",
        "rateLimitResetCredits":{"credits":[{"description":"private-credit"}]}
    });
    let result = json!({"id":2,"result":snapshot});
    fleet.script("codex", json!([rpc(result.clone()), rpc(result)]));
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
        {"bucket":"base","period":"primary","used_percent":25,"duration_minutes":300,"resets_at_unix_seconds":1700000000},
        {"bucket":"extra","period":"secondary","used_percent":62,"duration_minutes":null,"resets_at_unix_seconds":null}
    ]);
    let summary = usage(&mut server, json!({"providers":["codex","cursor"]}));
    assert_eq!(
        summary,
        json!({
            "codex":{"status":"available","plan":"plus","windows":expected_windows},
            "cursor":{"status":"plan_only","plan":"Pro Plus","windows":null}
        })
    );
    let full = usage(
        &mut server,
        json!({"providers":["cursor","codex"],"full":true}),
    );
    let mut expected = summary;
    expected["codex"]["ordinary_usage_allowed"] = json!(false);
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
    let prefix = r#"{"subscriptionTier":"Pro"}"#;
    fleet.script("cursor-agent",json!([
        {"stdout":"not JSON"},{"stdout":"{}"},{"stdout":r#"{"subscriptionTier":"user@example.invalid"}"#},
        {"stdout":format!("{prefix}{}\n{{}}"," ".repeat(256<<10))},{"code":1,"stdout":prefix,"stderr":"private-account"}
    ]));
    let mut server = fleet.server("codex");
    for reason in [
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
fn codex_falls_back_to_the_single_bucket_and_preserves_unavailable_windows() {
    let fleet = Fleet::new(&["codex"]);
    fleet.script("codex",json!([
        rpc(json!({"id":2,"result":{"rateLimits":{"limitId":"codex","secondary":{"usedPercent":0}}}})),
        rpc(json!({"id":2,"result":{"rateLimits":{},"rateLimitsByLimitId":{}}})),
        rpc(json!({"id":2,"result":{"rateLimits":{"primary":{"usedPercent":"50"}}}}))
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
        json!({"status":"no_windows","plan":null,"windows":[],"ordinary_usage_allowed":null,"model_prices":null})
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
    let mut nonzero = rpc(json!({"id":2,"result":{"rateLimits":{}}}));
    nonzero["code"] = json!(7);
    let mut tail = rpc(json!({"id":2,"result":{"rateLimits":{}}}));
    tail["stdout"] = json!(" ".repeat((256 << 10) + 1));
    fleet.script(
        "codex",
        json!([
            rpc(err(2,-32600,"codex account authentication required to read rate limits")),
            rpc(err(2,-32600,"unrelated private failure")),
            {"rpc":[{"expect":"initialize","send":"not JSON\n"}]},
            {"rpc":[{"expect":"initialize","send":"{\"id\":999,\"result\":{}}\n"}]},
            {"rpc":[{"expect":"initialize","send":"{\"id\":1}\n"}]},
            {"rpc":[{"expect":"initialize","send":"x".repeat((256<<10)+1)}]},
            {"stderr":"x".repeat((256<<10)+1),"sleep_ms":1000}, nonzero, tail
        ]),
    );
    let mut server = fleet.server("cursor");
    for reason in [
        "authentication",
        "unavailable",
        "invalid_report",
        "invalid_report",
        "invalid_report",
        "capture",
        "capture",
        "unavailable",
        "capture",
    ] {
        assert_eq!(
            usage(&mut server, json!({"providers":["codex"]})),
            json!({"codex":{"status":"unavailable","reason":reason}})
        );
    }
}

#[test]
fn a_reported_account_error_allows_the_app_server_to_exit_normally() {
    let fleet = Fleet::new(&["codex"]);
    let lock = fleet.root.join("rpc-error");
    let release = fleet.root.join("allow-exit");
    let mut step = rpc(
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
    let fleet = Fleet::new(&["codex"]);
    let lock = fleet.root.join("descendant");
    let release = fleet.root.join("release-leader");
    let mut step = rpc(json!({"id":2,"result":{"rateLimits":{}}}));
    step["pipe_holding_descendant"] = json!(lock);
    step["wait_for"] = json!(release);
    fleet.script("codex", json!([step]));
    let mut server = fleet.server("codex");
    server.start_call(13, "usage", json!({"providers":["codex"]}));
    let until = Instant::now() + WAIT;
    while !lock.with_extension("ready").exists() {
        assert!(
            Instant::now() < until,
            "descendant did not acquire its lock"
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
    let report: Value = serde_json::from_str(text(&server.response(13))).unwrap();
    assert_eq!(
        report["providers"]["codex"],
        json!({"status":"unavailable","reason":"timeout"})
    );
    assert!(started.elapsed() >= Duration::from_millis(1900));
    let until = Instant::now() + WAIT;
    loop {
        match witness.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(Instant::now() < until, "the descendant survived its parent");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("could not verify the descendant lock: {error}"),
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
            rpc(json!({"id":2,"result":{"rateLimits":{}}})),
        ),
        (
            "cursor",
            "cursor-agent",
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
