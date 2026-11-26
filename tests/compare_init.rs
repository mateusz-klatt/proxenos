#![cfg(feature = "test-tools")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use uuid::Uuid;

const BENCH: &str = env!("CARGO_BIN_EXE_compare_init");
const SERVER: &str = env!("CARGO_BIN_EXE_proxenos");
const STUB: &str = env!("CARGO_BIN_EXE_proxenos-stub");
const REVISION: &str = "2024-11-05";

struct Baseline(PathBuf);

impl Baseline {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("comparison fixture {}", Uuid::new_v4()));
        fs::create_dir_all(root.join("server/codex")).unwrap();
        fs::write(root.join("package.json"), r#"{"version":"3.5.0"}"#).unwrap();
        Self(root)
    }

    fn script(&self) -> PathBuf {
        self.0.join("server/codex/index.js")
    }

    fn set(&self, fixture: Value) {
        fs::write(self.script(), serde_json::to_vec(&fixture).unwrap()).unwrap();
    }

    fn log(&self) -> Vec<Value> {
        let bytes = fs::read_to_string(self.script().with_extension("log")).unwrap();
        bytes
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(BENCH);
        cmd.arg(SERVER).arg(STUB).arg(&self.0).arg(STUB).arg("2");
        cmd
    }

    fn report(&self, success: bool) -> Value {
        let out = self.command().output().unwrap();
        let result: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(out.status.success(), success, "{result}");
        result
    }
}

impl Drop for Baseline {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn response() -> Value {
    json!({"jsonrpc":"2.0","id":1,"result":{
        "protocolVersion":REVISION,"capabilities":{"tools":{}},
        "serverInfo":{"name":"claude-delegator-codex","version":"3.5.0"}
    }})
}

#[test]
fn paired_run_records_order_artifacts_and_the_initialize_only_conversation() {
    let baseline = Baseline::new();
    let notice = json!({"jsonrpc":"2.0","method":"notifications/message","params":{}});
    baseline
        .set(json!({"reply":format!("{notice}\n{}\n",response()), "after_eof_bytes":2*1024*1024}));
    let report = baseline.report(true);
    assert_eq!(report["protocol_revision"], REVISION);
    assert_eq!(report["d12_baseline_version_matches"], true);
    assert_eq!(report["inputs"]["pairs"], 2);
    assert_eq!(report["inputs"]["baseline_version"], "3.5.0");
    assert_eq!(
        report["inputs"]["package"]["sha256"],
        "c8f007fd9ef1200536fea1e3f6a209e5e692bcf62b57ee282f23c4e49d48abe2"
    );
    for artifact in ["proxenos", "node", "bridge", "stub", "package"] {
        let item = &report["inputs"][artifact];
        assert!(Path::new(item["path"].as_str().unwrap()).is_absolute());
        let hash = item["sha256"].as_str().unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    let trials = report["trials"].as_array().unwrap();
    assert_eq!(trials.len(), 4);
    for (n, target) in [
        "proxenos",
        "claude_delegator",
        "claude_delegator",
        "proxenos",
    ]
    .iter()
    .enumerate()
    {
        let trial = &trials[n];
        assert_eq!(trial["target"].as_str(), Some(*target));
        assert_eq!(trial["pair"], n / 2 + 1);
        assert_eq!(trial["position"], n % 2 + 1);
        assert!(trial["error"].is_null());
        let sample = &trial["observation"];
        let expected = if *target == "proxenos" {
            "proxenos"
        } else {
            "claude-delegator-codex"
        };
        assert_eq!(sample["server_name"], expected);
        assert!(sample["latency_ms"].as_f64().unwrap() >= 0.0);
    }
    for target in ["proxenos", "claude_delegator"] {
        let summary = &report["summary"][target];
        assert_eq!(summary["successful"], 2);
        assert_eq!(summary["failed"], 0);
        let samples: Vec<_> = trials
            .iter()
            .filter(|t| t["target"] == target)
            .map(|t| t["observation"]["latency_ms"].as_f64().unwrap())
            .collect();
        let expected = (samples[0] + samples[1]) / 2.0;
        let median = summary["median_ms"].as_f64().unwrap();
        assert!((median - expected).abs() <= 1e-12 * expected.max(1.0));
    }
    let log = baseline.log();
    assert_eq!(log.len(), 2);
    assert_ne!(log[0]["cwd"], log[1]["cwd"]);
    for record in log {
        let script = Path::new(record["script"].as_str().unwrap());
        let fingerprinted = Path::new(report["inputs"]["bridge"]["path"].as_str().unwrap());
        assert_eq!(script.canonicalize().unwrap(), fingerprinted);
        #[cfg(windows)]
        assert!(matches!(
            script.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::UNC(_, _))
        ));
        #[cfg(not(windows))]
        assert_eq!(script, fingerprinted);
        let cwd = Path::new(record["cwd"].as_str().unwrap());
        let home = Path::new(record["home"].as_str().unwrap());
        assert_eq!(cwd.file_name(), home.file_name());
        assert!(!cwd.exists(), "trial directory survived EOF");
        assert_eq!(record["codex"], true);
        let messages: Vec<Value> = record["input"]
            .as_str()
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            messages,
            [
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":REVISION,"capabilities":{},"clientInfo":{"name":"proxenos-comparison","version":"1"}
                }}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            ]
        );
    }
}

fn assert_bridge_failure(baseline: &Baseline, fixture: Value, reason: &str) {
    baseline.set(fixture);
    let report = baseline.report(false);
    assert_eq!(report["summary"]["proxenos"]["successful"], 2);
    assert_eq!(
        report["summary"]["claude_delegator"],
        json!({"successful":0,"failed":2,"median_ms":null})
    );
    for trial in report["trials"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|trial| trial["target"] == "claude_delegator")
    {
        assert!(trial["observation"].is_null());
        assert!(trial["error"].as_str().unwrap().contains(reason), "{trial}");
    }
}

#[test]
fn invalid_responses_and_failed_shutdown_do_not_enter_latency_summaries() {
    let baseline = Baseline::new();
    for (pointer, invalid, reason) in [
        ("/jsonrpc", json!("1.0"), "JSON-RPC version"),
        ("/id", json!(2), "invalid initialize response"),
        (
            "/result/protocolVersion",
            json!("unsupported"),
            "invalid initialize response",
        ),
        (
            "/result/capabilities/tools",
            Value::Null,
            "invalid initialize response",
        ),
        ("/result/serverInfo/name", json!(""), "missing server name"),
        (
            "/result/serverInfo/version",
            json!(""),
            "missing server version",
        ),
        (
            "/result/serverInfo/name",
            json!("another-bridge"),
            "selected server",
        ),
        (
            "/result/serverInfo/version",
            json!("0.1"),
            "selected server",
        ),
    ] {
        let mut message = response();
        *message.pointer_mut(pointer).unwrap() = invalid;
        assert_bridge_failure(&baseline, json!({"reply":format!("{message}\n")}), reason);
    }
    let mut err = response();
    err["error"] = json!({"code":-32603,"message":"failed"});
    assert_bridge_failure(
        &baseline,
        json!({"reply":format!("{err}\n")}),
        "invalid initialize response",
    );
    assert_bridge_failure(&baseline, json!({"reply":"invalid\n"}), "expected");
    assert_bridge_failure(
        &baseline,
        json!({"reply":"x".repeat(1024*1024)}),
        "exceeded 1 MiB",
    );
    assert_bridge_failure(
        &baseline,
        json!({"reply":format!("{}\n",response()),"exit_code":9}),
        "exited unsuccessfully",
    );
}

#[test]
fn incomplete_response_times_out_and_the_second_target_still_runs() {
    let baseline = Baseline::new();
    // The peer writes a prefix and waits for EOF. The client waits for the terminating LF.
    assert_bridge_failure(&baseline, json!({"reply":"{"}), "timed out");
}

fn assert_setup_failure(out: Output, reason: &str) {
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(reason),
        "{out:?}"
    );
}

#[test]
fn invalid_setup_is_reported_before_a_trial_starts() {
    let baseline = Baseline::new();
    assert_setup_failure(Command::new(BENCH).output().unwrap(), "usage:");
    for pairs in ["abc", "0", "3", "1002"] {
        let out = Command::new(BENCH)
            .arg(SERVER)
            .arg(STUB)
            .arg(&baseline.0)
            .arg(STUB)
            .arg(pairs)
            .output()
            .unwrap();
        assert_setup_failure(out, "usage:");
    }
    fs::create_dir(baseline.script()).unwrap();
    assert_setup_failure(baseline.command().output().unwrap(), "expected a file");
    fs::write(baseline.0.join("package.json"), "{}").unwrap();
    assert_setup_failure(baseline.command().output().unwrap(), "needs a version");
}

#[test]
fn fixture_peer_replays_wire_bytes_until_eof() {
    let baseline = Baseline::new();
    baseline.set(json!({"reply":"response\n","after_eof_bytes":3,"exit_code":7}));
    let mut child = Command::new(STUB)
        .arg(baseline.script())
        .env("HOME", &baseline.0)
        .current_dir(&baseline.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"request\nack\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"response\n   ");
    assert_eq!(baseline.log()[0]["input"], "request\nack\n");
}
