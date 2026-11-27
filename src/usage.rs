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
                "description":"Request the full report with per-model pricing when available. These two CLI reads currently return model_prices: null."}
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
                #[cfg_attr(not(windows), allow(unused_mut))]
                let mut body = json!({"status":"unavailable","reason":reason});
                #[cfg(windows)]
                if error.failure.kind == crate::envelope::FailureKind::UnsupportedLauncher {
                    body["hint"] = json!(
                        "If a native executable exists, point PATH at its directory ahead of the shim (or set PROXENOS_NODE_BIN for a JavaScript launcher)."
                    );
                }
                body
            }
            Ok(exe) => {
                let read = match source {
                    Source::Codex => codex::read(exe, deadline, cancel, activity.clone())
                        .await
                        .and_then(codex_report),
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
    if !run.stdout.eof {
        return Err("timeout");
    }
    if run.stdout.total > run.stdout.bytes.len() as u64 {
        return Err("capture");
    }
    let about: Value = serde_json::from_slice(&run.stdout.bytes).map_err(|_| "invalid_report")?;
    match about.get("subscriptionTier") {
        Some(Value::String(plan)) => {
            let plan = cursor_tier(plan).ok_or("invalid_report")?;
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
    rate_limit_reached_type: Option<String>,
    spend_control_reached: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    rate_limits: Snapshot,
    rate_limits_by_limit_id: Option<BTreeMap<String, Snapshot>>,
    ordinary_usage_allowed: Option<bool>,
}

fn cursor_tier(raw: &str) -> Option<&'static str> {
    const TIERS: &[(&str, &str)] = &[
        ("free", "Free"),
        ("hobby", "Hobby"),
        ("freetrial", "Free Trial"),
        ("plus", "Plus"),
        ("pro", "Pro"),
        ("pro+", "Pro Plus"),
        ("proplus", "Pro Plus"),
        ("premium", "Premium"),
        ("super", "Super"),
        ("ultra", "Ultra"),
        ("team", "Teams"),
        ("teams", "Teams"),
        ("business", "Business"),
        ("enterprise", "Enterprise"),
    ];
    if raw.trim().is_empty() {
        return None;
    }
    let key: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    Some(
        TIERS
            .iter()
            .find(|(alias, _)| *alias == key)
            .map_or("other", |(_, tier)| *tier),
    )
}

fn map_known(raw: &str, known: &[&'static str]) -> Option<&'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        known
            .iter()
            .copied()
            .find(|entry| entry.eq_ignore_ascii_case(trimmed))
            .unwrap_or("other"),
    )
}

fn codex_plan(raw: &str) -> Option<&'static str> {
    const PLANS: &[&str] = &[
        "free",
        "go",
        "plus",
        "pro",
        "prolite",
        "promax",
        "team",
        "self_serve_business_prolite",
        "self_serve_business_usage_based",
        "business",
        "ent26",
        "enterprise_cbp_automation",
        "enterprise_cbp_usage_based",
        "enterprise",
        "edu",
        "edu_plus",
        "edu_pro",
        "sci",
        "sci_plus",
        "unknown",
    ];
    map_known(raw, PLANS)
}

fn codex_limit_reached_type(raw: &str) -> Option<&'static str> {
    const TYPES: &[&str] = &[
        "rate_limit_reached",
        "workspace_owner_credits_depleted",
        "workspace_member_credits_depleted",
        "workspace_owner_usage_limit_reached",
        "workspace_member_usage_limit_reached",
    ];
    map_known(raw, TYPES)
}

const CODEX_BUCKETS: &[&str] = &["codex", "codex_bengalfox", "codex_other", "codex_secondary"];

fn known_codex_bucket(raw: &str) -> Option<&'static str> {
    CODEX_BUCKETS.iter().copied().find(|&known| known == raw)
}

struct BucketLabeler {
    multiple_unknown: bool,
    next_unknown: usize,
}

impl BucketLabeler {
    fn new<'a>(keys: impl Iterator<Item = &'a String>) -> Self {
        let unknown_count = keys
            .filter(|id| !id.is_empty() && known_codex_bucket(id).is_none())
            .count();
        Self {
            multiple_unknown: unknown_count > 1,
            next_unknown: 0,
        }
    }

    fn label(&mut self, id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        if let Some(known) = known_codex_bucket(id) {
            return Some(known.to_owned());
        }
        if self.multiple_unknown {
            self.next_unknown += 1;
            Some(format!("other_{}", self.next_unknown))
        } else {
            Some(String::from("other"))
        }
    }
}

fn codex_report(value: Value) -> Result<Value, &'static str> {
    let limits: RateLimits = serde_json::from_value(value).map_err(|_| "invalid_report")?;
    let plan = limits.rate_limits.plan_type.as_deref().and_then(codex_plan);
    let top_reached = limits
        .rate_limits
        .rate_limit_reached_type
        .as_deref()
        .and_then(codex_limit_reached_type);
    let top_spend = limits.rate_limits.spend_control_reached;
    let mut windows = Vec::new();
    let mut bucket_limits = Vec::new();
    let mut push_windows = |bucket: Option<&str>, snapshot: &Snapshot| {
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
            let mut labeler = BucketLabeler::new(buckets.keys());
            for (id, snapshot) in buckets {
                let bucket = labeler.label(id);
                push_windows(bucket.as_deref(), snapshot);
                let reached = snapshot
                    .rate_limit_reached_type
                    .as_deref()
                    .and_then(codex_limit_reached_type);
                if reached.is_some() || snapshot.spend_control_reached.is_some() {
                    bucket_limits.push(json!({
                        "bucket": bucket,
                        "rate_limit_reached_type": reached,
                        "spend_control_reached": snapshot.spend_control_reached,
                    }));
                }
            }
        }
        _ => {
            let mut labeler = BucketLabeler {
                multiple_unknown: false,
                next_unknown: 0,
            };
            let bucket = limits
                .rate_limits
                .limit_id
                .as_deref()
                .and_then(|id| labeler.label(id));
            push_windows(bucket.as_deref(), &limits.rate_limits);
        }
    }
    let any_bucket_exhausted = bucket_limits.iter().any(|entry| {
        !entry["rate_limit_reached_type"].is_null()
            || entry["spend_control_reached"] == Value::Bool(true)
    });
    let exhausted = top_reached.is_some()
        || top_spend == Some(true)
        || limits.ordinary_usage_allowed == Some(false)
        || any_bucket_exhausted;
    let status = if !windows.is_empty() {
        "available"
    } else if exhausted {
        "limit_reached"
    } else {
        "no_windows"
    };
    let mut report = json!({
        "status": status,
        "plan": plan,
        "windows": windows,
        "ordinary_usage_allowed": limits.ordinary_usage_allowed,
        "rate_limit_reached_type": top_reached,
        "spend_control_reached": top_spend,
    });
    if !bucket_limits.is_empty() {
        report["bucket_limits"] = Value::Array(bucket_limits);
    }
    Ok(report)
}
