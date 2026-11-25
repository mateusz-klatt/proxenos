//! Resource accounting and optional memory limits across platforms (D18).

use serde_json::{Value, json};

use crate::provider_support::{Fleet, call, turn};

#[test]
fn accounting_reports_platform_method_caveats_memory_and_cpu() {
    let fleet = Fleet::new(&["agy"]);
    let mut step = turn("measured");
    step["alloc_bytes"] = json!(16_u64 << 20);
    step["burn_cpu_ms"] = json!(80);
    step["sleep_ms"] = json!(150);
    fleet.script("agy", json!([step]));

    let mut server = fleet.server("claude");

    // Parameter validation for wait and memory_limit_bytes.
    for bad in [
        json!({"prompt": "x", "wait": "false"}),
        json!({"prompt": "x", "memory_limit_bytes": 0}),
        json!({"prompt": "x", "memory_limit_bytes": -10}),
        json!({"prompt": "x", "memory_limit_bytes": "1024"}),
    ] {
        let rejected = call(&mut server, "antigravity", bad);
        assert_eq!(rejected["observed"]["failure"]["kind"], "invalid_request");
        assert!(rejected["observed"]["accounting"].is_null());
    }

    let result = call(&mut server, "antigravity", json!({"prompt": "measure"}));
    assert_eq!(result["observed"]["state"], "finished", "{result}");

    for accounting in [
        &result["observed"]["accounting"],
        &result["observed"]["attempts"][0]["accounting"],
    ] {
        #[cfg(target_os = "linux")]
        {
            assert_eq!(accounting["method"], "linux_proc_group_sample");
            assert!(accounting["samples"].as_u64().unwrap() >= 1);
            let caveats = accounting["caveats"].to_string().to_lowercase();
            assert!(caveats.contains("miss"), "{caveats}");
            assert!(caveats.contains("short-lived"), "{caveats}");
            assert!(caveats.contains("leave the process group"), "{caveats}");
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(accounting["method"], "macos_proc_group_sample");
            assert!(accounting["samples"].as_u64().unwrap() >= 1);
            let caveats = accounting["caveats"].to_string().to_lowercase();
            assert!(caveats.contains("miss"), "{caveats}");
            assert!(caveats.contains("short-lived"), "{caveats}");
            assert!(caveats.contains("leave the process group"), "{caveats}");
        }
        #[cfg(windows)]
        {
            assert_eq!(accounting["method"], "windows_job_totals");
            assert!(accounting.get("samples").is_none());
            let caveats = accounting["caveats"].to_string().to_lowercase();
            assert!(caveats.contains("committed"), "{caveats}");
            assert!(caveats.contains("refuse"), "{caveats}");
        }
        assert!(accounting["memory_limit_bytes"].is_null());
        assert_eq!(accounting["memory_limit_exceeded"], false);
        assert!(
            accounting["peak_memory_bytes"].as_u64().unwrap() >= (16_u64 << 20),
            "{accounting}"
        );
        assert!(
            accounting["cpu_time_ms"].as_u64().unwrap() > 0,
            "{accounting}"
        );
    }
}

#[test]
fn memory_limit_is_honoured_when_under_and_triggers_memory_limit_failure_when_exceeded() {
    let fleet = Fleet::new(&["agy"]);
    let mut baseline_step = turn("baseline");
    baseline_step["sleep_ms"] = json!(150);
    let mut under_step = turn("within limit");
    under_step["sleep_ms"] = json!(150);
    let mut over_step = turn("over limit");
    over_step["alloc_bytes"] = json!(64_u64 << 20);
    over_step["sleep_ms"] = json!(10_000);
    fleet.script(
        "agy",
        Value::Array(vec![baseline_step, under_step, over_step]),
    );

    let mut server = fleet.server("claude");

    let baseline = call(&mut server, "antigravity", json!({"prompt": "baseline"}));
    assert_eq!(baseline["observed"]["state"], "finished", "{baseline}");
    let base_bytes = baseline["observed"]["accounting"]["peak_memory_bytes"]
        .as_u64()
        .unwrap();

    // Limit well above baseline: delegation finishes cleanly.
    let generous = base_bytes + (64 << 20);
    let under = call(
        &mut server,
        "antigravity",
        json!({"prompt": "under", "memory_limit_bytes": generous}),
    );
    assert_eq!(under["observed"]["state"], "finished", "{under}");
    assert_eq!(
        under["observed"]["accounting"]["memory_limit_bytes"],
        generous
    );
    assert_eq!(
        under["observed"]["accounting"]["memory_limit_exceeded"],
        false
    );

    // Limit above baseline but below the stub's 64 MiB allocation: deterministic memory_limit failure.
    let tight = base_bytes + (16 << 20);
    let over = call(
        &mut server,
        "antigravity",
        json!({"prompt": "over", "memory_limit_bytes": tight}),
    );
    assert_eq!(over["observed"]["state"], "failed", "{over}");
    assert_eq!(
        over["observed"]["failure"]["kind"], "memory_limit",
        "{over}"
    );
    assert_eq!(over["observed"]["accounting"]["memory_limit_bytes"], tight);
    assert_eq!(
        over["observed"]["accounting"]["memory_limit_exceeded"],
        true
    );
    #[cfg(unix)]
    assert!(
        over["observed"]["accounting"]["peak_memory_bytes"]
            .as_u64()
            .unwrap()
            > tight,
        "{over}"
    );
}
