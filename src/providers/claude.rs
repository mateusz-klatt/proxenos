//! Claude Code runs one turn with `--print --output-format json`.
//! Replies resume an explicit session UUID with `--resume`.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use super::{CommandLine, Failure, Provider, Report, Turn, is_valid_model};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "cwd", "access", "mode", "instructions", "effort"];

pub(super) const SPEC: Provider = Provider {
    name: "claude",
    cli: "claude",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin")],
    #[cfg(windows)]
    install_dirs: &[("USERPROFILE", ".local/bin"), ("APPDATA", "npm")],
    efforts: &["low", "medium", "high", "xhigh", "max"],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[
        (
            "full",
            "Claude Code runs with --permission-mode bypassPermissions, approving tools and commands without interactive prompts. With --print, workspace trust is skipped and project settings in cwd (hooks, env, MCP servers) still load.",
        ),
        (
            "read-only",
            "Claude Code runs with --permission-mode plan and --setting-sources user. Plan mode is a provider-side policy, not an OS sandbox; --setting-sources user excludes project and local settings files, while user/managed settings and allow-lists still apply.",
        ),
    ],
    fixed_instructions: true,
    reply_model: false,
    not_claimed: &[
        "Project settings in the working directory (hooks, env, MCP servers) were not applied; with --print Claude Code skips workspace trust.",
    ],
    read_only_not_claimed: &[
        "Claude Code's plan mode enforced an OS sandbox or prevented user and managed settings or permission allow-lists from interacting with it.",
    ],
    prepare,
    argv,
    parse,
};

fn is_canonical_uuid(id: &str) -> bool {
    uuid::Uuid::try_parse(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

fn prepare(turn: &Turn) -> Result<(), String> {
    if let Some(id) = &turn.thread_id
        && !is_canonical_uuid(id)
    {
        return Err("Claude thread_id must be a canonical UUID.".into());
    }
    Ok(())
}

fn argv(turn: &Turn, _output: &Path) -> CommandLine {
    let read_only = turn.settings.access == "read-only";
    let permission_mode = if read_only {
        "plan"
    } else {
        "bypassPermissions"
    };
    let mut args: Vec<OsString> = vec![
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--permission-mode".into(),
        permission_mode.into(),
        "--disable-slash-commands".into(),
    ];
    if read_only {
        args.extend(["--setting-sources".into(), "user".into()]);
    }
    if let Some(id) = &turn.thread_id {
        args.extend(["--resume".into(), id.into()]);
    } else {
        args.extend(["--append-system-prompt".into(), (&turn.guidance).into()]);
    }
    if let Some(model) = &turn.settings.model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = &turn.settings.effort {
        args.extend(["--effort".into(), effort.into()]);
    }
    args.extend(["--".into(), (&turn.prompt).into()]);
    CommandLine {
        args,
        cli_timeout_ms: None,
    }
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().and_then(nonempty)
}

fn result_object(bytes: &[u8]) -> Option<Value> {
    fn object(bytes: &[u8]) -> Option<Value> {
        serde_json::from_slice::<Value>(bytes)
            .ok()
            .filter(|value| value["type"] == "result")
    }
    object(bytes).or_else(|| bytes.rsplit(|byte| *byte == b'\n').find_map(object))
}

fn parse_model(value: &Value) -> Option<String> {
    let models = value.get("modelUsage")?.as_object()?;
    let mut keys = models.keys();
    let first = keys.next()?;
    if keys.next().is_some() {
        return None;
    }
    let trimmed = first.trim();
    is_valid_model(trimmed).then(|| trimmed.to_owned())
}

fn parse_usage(value: Option<&Value>) -> Option<Value> {
    let object = value?.as_object()?;
    let mut filtered: serde_json::Map<String, Value> = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .into_iter()
    .filter_map(|key| {
        let count = object.get(key)?.as_u64()?;
        Some((key.to_owned(), Value::from(count)))
    })
    .collect();
    if let Some(thinking) = object
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        filtered.insert("thinking_tokens".into(), Value::from(thinking));
    }
    (!filtered.is_empty()).then_some(Value::Object(filtered))
}

fn parse_cost(value: Option<&Value>) -> Option<f64> {
    value?
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

// Only CLI error channels reach this classifier, never successful model answers.
fn classify(message: &str, api_status: Option<u64>, api_type: Option<&str>) -> Option<Failure> {
    let lower = message.to_ascii_lowercase();
    let kind = if api_status == Some(401)
        || api_type == Some("authentication_error")
        || [
            "not logged in",
            "please run /login",
            "invalid api key",
            "invalid x-api-key",
            "authentication_error",
            "authentication failed",
            "401 unauthorized",
            "oauth token has expired",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        FailureKind::Authentication
    } else if api_type == Some("rate_limit_error")
        || [
            "usage limit",
            "hit your limit",
            "rate limit",
            "credit balance is too low",
            "out of extra usage",
            "exceeded your current quota",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        FailureKind::Quota
    } else if [
        "content filtering policy",
        "output blocked by content filtering",
        "usage policy",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        FailureKind::Refusal
    } else {
        return None;
    };
    let mut failure = Failure::new(kind, message);
    if kind == FailureKind::Quota {
        failure.reset_at = ["resets at ", "reset at ", "resets ", "try again at "]
            .into_iter()
            .find_map(|marker| {
                let start = lower.find(marker)? + marker.len();
                let when = message[start..].trim().trim_end_matches('.');
                nonempty(when).map(str::to_owned)
            });
    }
    Some(failure)
}

fn error_reason(value: &Value, stderr: &str, subtype: Option<&str>) -> String {
    if let Some(joined) = value["errors"].as_array().and_then(|items| {
        let messages: Vec<&str> = items
            .iter()
            .filter_map(Value::as_str)
            .filter_map(nonempty)
            .collect();
        (!messages.is_empty()).then(|| messages.join("; "))
    }) {
        return joined;
    }
    if let Some(message) = text(value, "result") {
        return message.to_owned();
    }
    if let Some(detail) = nonempty(stderr) {
        return detail.to_owned();
    }
    match subtype {
        Some(kind) => format!("Claude stopped with subtype {kind}."),
        None => "Claude returned an unsuccessful result.".into(),
    }
}

fn parse(finished: &Finished, _output: &str, expected: Option<&str>) -> Report {
    let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
    let Some(value) = result_object(&finished.stdout.bytes) else {
        return Report {
            failure: classify(stderr.trim(), None, None),
            ..Report::default()
        };
    };
    let raw_id = text(&value, "session_id").map(str::trim);
    let mut report = Report {
        thread_id: raw_id.filter(|id| is_canonical_uuid(id)).map(str::to_owned),
        ..Report::default()
    };
    let is_error = value["is_error"] == true;
    let subtype = text(&value, "subtype");
    let ran_turn = value["num_turns"].as_u64().is_some_and(|turns| turns > 0);
    let success = !is_error && subtype == Some("success") && ran_turn;
    let id_matches = expected.is_none() || expected == report.thread_id.as_deref();
    let foreign_id = expected.is_some() && raw_id.is_some() && expected != raw_id;
    if foreign_id || (success && !id_matches) {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Claude did not confirm the requested session ID.",
        ));
        return report;
    }
    if id_matches {
        report.model = parse_model(&value);
        report.usage = parse_usage(value.get("usage"));
        report.session_cost_usd = parse_cost(value.get("total_cost_usd"));
    }
    if !is_error && subtype == Some("success") && !ran_turn {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Claude exited without running a turn.",
        ));
    } else if !success {
        let reason = error_reason(&value, stderr.trim(), subtype);
        let api_status = value["api_error_status"].as_u64();
        let api_type = text(&value, "api_error_type");
        report.failure = Some(
            classify(&reason, api_status, api_type)
                .unwrap_or_else(|| Failure::new(FailureKind::Unknown, reason)),
        );
    } else {
        report.content = text(&value, "result").map(str::to_owned);
        if report.content.is_none() {
            let denied = value["permission_denials"]
                .as_array()
                .is_some_and(|items| !items.is_empty());
            report.failure = Some(if denied {
                Failure::new(
                    FailureKind::Refusal,
                    "Claude produced no answer after tool permission denials.",
                )
            } else {
                classify(stderr.trim(), None, None).unwrap_or_else(|| {
                    Failure::new(FailureKind::Unknown, "Claude returned an empty result.")
                })
            });
        } else {
            report.completed = true;
        }
    }
    report
}
