//! Subscription information obtained by asking installed CLIs for account metadata (D9).
mod codex;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::discovery::{self, Executable};
use crate::mcp::{Activity, ToolOutcome, ToolSpec};
use crate::supervise::{self, Ending, Launch};

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const REPORT_LIMIT: usize = 256 << 10;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Source {
    Codex,
    Cursor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    providers: Vec<Source>,
    #[serde(default)]
    full: bool,
}

pub fn tool() -> ToolSpec {
    let schema = json!({
        "type":"object",
        "properties":{
            "providers":{"type":"array","items":{"enum":["codex","cursor"]},
                "minItems":1,"maxItems":2,"uniqueItems":true},
            "full":{"type":"boolean","default":false,
                "description":"Include available account details. These two CLI reads do not supply model prices."}
        },
        "required":["providers"],"additionalProperties":false
    });
    ToolSpec {
        name: "usage".into(),
        description: "Read Codex subscription windows or the plan exposed by Cursor without running a model. Reset times are Unix seconds. Each requested provider has a separate status.".into(),
        input_schema: schema.as_object().unwrap().clone(),
    }
}

pub async fn call(
    args: Map<String, Value>,
    cancel: &CancellationToken,
    activity: Activity,
) -> ToolOutcome {
    let Ok(request) = serde_json::from_value::<Request>(Value::Object(args)) else {
        return crate::background::invalid_request(
            "Expected providers from codex and cursor, and an optional boolean full.",
        );
    };
    match request.providers.as_slice() {
        [_] => {}
        [first, second] if first != second => {}
        _ => {
            return crate::background::invalid_request(
                "Select one or two distinct usage providers.",
            );
        }
    }
    let mut reports = Map::new();
    for source in request.providers {
        let (name, cli, dirs) = match source {
            Source::Codex => {
                let spec = &crate::providers::CODEX;
                (spec.name, spec.cli, spec.install_dirs)
            }
            Source::Cursor => {
                #[cfg(unix)]
                const DIRS: &[(&str, &str)] = &[("HOME", ".local/bin")];
                #[cfg(windows)]
                const DIRS: &[(&str, &str)] = &[];
                ("cursor", "cursor-agent", DIRS)
            }
        };
        let deadline = Instant::now() + READ_TIMEOUT;
        let report = match discovery::locate_for_usage(cli, dirs, cancel, deadline).await {
            Err(error) => {
                let reason = match error.failure.kind {
                    crate::envelope::FailureKind::Cancellation => json!("cancelled"),
                    other => json!(other),
                };
                json!({"status":"unavailable","reason":reason})
            }
            Ok(exe) => {
                let read = match source {
                    Source::Codex => codex::read(exe, deadline, cancel, activity.clone())
                        .await
                        .and_then(|v| codex_report(v, request.full)),
                    Source::Cursor => read_cursor(exe, deadline, cancel, activity.clone()).await,
                };
                match read {
                    Ok(mut value) => {
                        if request.full {
                            value["model_prices"] = Value::Null;
                        }
                        value
                    }
                    Err(reason) => json!({"status":"unavailable","reason":reason}),
                }
            }
        };
        reports.insert(name.to_owned(), report);
    }
    ToolOutcome {
        is_error: false,
        text: json!({"schema_version":1,"providers":reports}).to_string(),
    }
}

async fn read_interruption(deadline: Instant, cancel: &CancellationToken) -> &'static str {
    tokio::select! {
        biased;
        () = cancel.cancelled() => "cancelled",
        () = tokio::time::sleep_until(deadline.into()) => "timeout",
    }
}

async fn read_cursor(
    mut exe: Executable,
    deadline: Instant,
    cancel: &CancellationToken,
    activity: Activity,
) -> Result<Value, &'static str> {
    exe.prefix
        .extend(["about".into(), "--format".into(), "json".into()]);
    let launch = Launch {
        program: exe.program,
        args: exe.prefix,
        cwd: None,
        env_set: Vec::new(),
        env_remove: Vec::new(),
        deadline,
        memory_limit_bytes: None,
        capture_limit: REPORT_LIMIT,
        temporary_files: Vec::new(),
    };
    let run = supervise::run_with_interruptible_spawn(
        launch,
        cancel,
        activity,
        read_interruption(deadline, cancel),
    )
    .await?
    .map_err(|_| "unavailable")?;
    match run.ending {
        Ending::TimedOut => return Err("timeout"),
        Ending::Cancelled => return Err("cancelled"),
        _ if run.error.is_some() || run.code != Some(0) => return Err("unavailable"),
        _ => {}
    }
    if run.stdout.total > run.stdout.bytes.len() as u64 {
        return Err("capture");
    }
    let about: Value = serde_json::from_slice(&run.stdout.bytes).map_err(|_| "invalid_report")?;
    match about.get("subscriptionTier") {
        Some(Value::String(plan)) if valid_label(plan) => {
            Ok(json!({"status":"plan_only","plan":plan,"windows":null}))
        }
        Some(Value::Null) => Err("plan_not_reported"),
        _ => Err("invalid_report"),
    }
}

#[derive(Deserialize, Serialize)]
struct Window {
    #[serde(rename(deserialize = "usedPercent"))]
    used_percent: u32,
    #[serde(rename(deserialize = "windowDurationMins"))]
    duration_minutes: Option<u64>,
    #[serde(rename(deserialize = "resetsAt"))]
    resets_at_unix_seconds: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    limit_id: Option<String>,
    plan_type: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    rate_limits: Snapshot,
    rate_limits_by_limit_id: Option<BTreeMap<String, Snapshot>>,
    ordinary_usage_allowed: Option<bool>,
}

fn valid_label(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'-' | b'.' | b'+'))
}

fn codex_report(value: Value, full: bool) -> Result<Value, &'static str> {
    let limits: RateLimits = serde_json::from_value(value).map_err(|_| "invalid_report")?;
    let plan = limits
        .rate_limits
        .plan_type
        .as_deref()
        .filter(|s| valid_label(s));
    let mut windows = Vec::new();
    let mut add = |id: Option<&str>, snapshot: &Snapshot| {
        let bucket = id.filter(|s| valid_label(s));
        for (period, window) in [
            ("primary", &snapshot.primary),
            ("secondary", &snapshot.secondary),
        ] {
            if let Some(window) = window {
                let mut entry = json!(window);
                entry["bucket"] = json!(bucket);
                entry["period"] = json!(period);
                windows.push(entry);
            }
        }
    };
    match &limits.rate_limits_by_limit_id {
        Some(buckets) if !buckets.is_empty() => {
            for (id, snapshot) in buckets {
                add(Some(id), snapshot);
            }
        }
        _ => add(limits.rate_limits.limit_id.as_deref(), &limits.rate_limits),
    }
    let status = if windows.is_empty() {
        "no_windows"
    } else {
        "available"
    };
    let mut report = json!({"status":status,"plan":plan,"windows":windows});
    if full {
        report["ordinary_usage_allowed"] = json!(limits.ordinary_usage_allowed);
    }
    Ok(report)
}
