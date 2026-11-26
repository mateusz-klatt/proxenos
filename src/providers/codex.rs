//! Codex runs one turn with `exec`. Both start and resume accept config overrides.
use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use super::{CommandLine, Failure, Provider, Report, Turn};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "cwd", "access", "mode", "instructions", "effort"];

pub(super) const SPEC: Provider = Provider {
    name: "codex",
    cli: "codex",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin")],
    #[cfg(windows)]
    install_dirs: &[
        ("LOCALAPPDATA", "Programs/OpenAI/Codex/bin"),
        ("APPDATA", "npm"),
    ],
    efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[
        (
            "full",
            "Codex disables sandbox restrictions and approval prompts.",
        ),
        (
            "read-only",
            "Codex requests its OS read-only sandbox. Writes, including build caches, can fail.",
        ),
    ],
    fixed_instructions: true,
    reply_model: false,
    not_claimed: &[],
    prepare,
    argv,
    parse,
};

fn is_canonical_uuid(id: &str) -> bool {
    uuid::Uuid::try_parse(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

fn prepare(turn: &Turn) -> Result<(), String> {
    if turn.prompt == "-" {
        return Err("Codex interprets a lone dash as a request to read stdin.".into());
    }
    if let Some(id) = &turn.thread_id
        && !is_canonical_uuid(id)
    {
        return Err("Codex thread_id must be a canonical UUID.".into());
    }
    Ok(())
}

// Codex's -c takes a TOML basic string. JSON's escapes are valid here and DEL needs one too.
fn quoted(value: &str) -> String {
    Value::from(value).to_string().replace('\u{7f}', "\\u007f")
}

fn argv(turn: &Turn, output: &Path) -> CommandLine {
    let mut args = vec!["exec".into()];
    if turn.thread_id.is_some() {
        args.push("resume".into());
    }
    let sandbox = if turn.settings.access == "read-only" {
        "read-only"
    } else {
        "danger-full-access"
    };
    args.extend(["--json".into(), "--skip-git-repo-check".into(), "-c".into()]);
    args.push(format!("sandbox_mode=\"{sandbox}\"").into());
    args.extend(["-c".into(), "approval_policy=\"never\"".into()]);
    if let Some(model) = &turn.settings.model {
        args.extend(["-m".into(), model.into()]);
    }
    if let Some(effort) = &turn.settings.effort {
        args.extend([
            "-c".into(),
            format!("model_reasoning_effort={}", quoted(effort)).into(),
        ]);
    }
    if turn.thread_id.is_none() {
        args.extend([
            "-c".into(),
            format!("developer_instructions={}", quoted(&turn.guidance)).into(),
        ]);
    }
    args.extend(["-o".into(), output.into(), "--".into()]);
    args.extend(turn.thread_id.iter().map(OsString::from));
    args.push((&turn.prompt).into());
    CommandLine {
        args,
        cli_timeout_ms: None,
    }
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

// The backend sometimes serializes its error object into the event's message.
// Only CLI diagnostic channels reach this classifier, never agent text or tool results.
fn diagnostic(raw: &str) -> Failure {
    let backend: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let error = &backend["error"];
    let message = error["message"].as_str().unwrap_or(raw);
    let lower = message.to_ascii_lowercase();
    let code = error["code"].as_str().or_else(|| error["type"].as_str());
    let kind = match code {
        Some("insufficient_quota" | "usage_limit_reached") => FailureKind::Quota,
        Some("invalid_api_key" | "authentication_error") => FailureKind::Authentication,
        Some("content_filter" | "content_policy_violation") => FailureKind::Refusal,
        _ if backend["status"] == 401 || lower.contains("401 unauthorized") => {
            FailureKind::Authentication
        }
        _ if lower.contains("you've hit your usage limit") => FailureKind::Quota,
        _ if lower.contains("flagged for possible cybersecurity risk") => FailureKind::Refusal,
        _ => FailureKind::Unknown,
    };
    let mut failure = Failure::new(kind, message);
    if kind == FailureKind::Quota {
        let reset = match &error["resets_at"] {
            Value::Number(timestamp) => Some(timestamp.to_string()),
            value => value.as_str().map(str::to_owned),
        };
        failure.reset_at = reset.or_else(|| {
            let (_, when) = message.rsplit_once("try again at ")?;
            nonempty(when.trim_end_matches('.')).map(str::to_owned)
        });
    }
    failure
}

fn parse_usage(value: Option<&Value>) -> Option<Value> {
    let object = value?.as_object()?;
    let filtered: serde_json::Map<String, Value> = [
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ]
    .into_iter()
    .filter_map(|key| {
        let count = object.get(key)?.as_u64()?;
        Some((key.to_owned(), Value::from(count)))
    })
    .collect();
    (!filtered.is_empty()).then_some(Value::Object(filtered))
}

fn parse(run: &Finished, final_message: &str, expected: Option<&str>) -> Report {
    let mut report = Report::default();
    let mut streamed = None;
    let mut last_error = None;
    for event in run
        .stdout
        .bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
    {
        match event["type"].as_str() {
            Some("thread.started") => {
                report.thread_id = event["thread_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|id| is_canonical_uuid(id))
                    .map(str::to_owned);
            }
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                streamed = event["item"]["text"]
                    .as_str()
                    .and_then(nonempty)
                    .map(str::to_owned);
            }
            Some("turn.completed") => {
                report.completed = true;
                report.usage = parse_usage(event.get("usage"));
            }
            Some("error") => last_error = event["message"].as_str().map(diagnostic),
            Some("turn.failed") => {
                let terminal = event["error"]["message"]
                    .as_str()
                    .and_then(nonempty)
                    .map(diagnostic);
                let failure = match (terminal, last_error.take()) {
                    (Some(summary), Some(detail))
                        if summary.kind == FailureKind::Unknown
                            && detail.kind != FailureKind::Unknown =>
                    {
                        detail
                    }
                    (summary, detail) => summary.or(detail).unwrap_or_else(|| {
                        Failure::new(FailureKind::Unknown, "Codex reported an unsuccessful turn.")
                    }),
                };
                // An unsuccessful turn can mention a missing answer while reporting another
                // cause. Only an explicit missing-answer diagnostic permits a follow-up.
                report.retry_missing_final = failure.kind == FailureKind::Unknown
                    && matches!(
                        failure
                            .message
                            .trim()
                            .trim_end_matches('.')
                            .to_ascii_lowercase()
                            .as_str(),
                        "turn ended without a final message"
                            | "codex completed the turn without a final message"
                    );
                report.failure = Some(failure);
            }
            _ => {}
        }
    }
    if expected.is_some() && expected != report.thread_id.as_deref() && report.completed {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Codex did not confirm the requested thread.",
        ));
        report.retry_missing_final = false;
    } else if report.completed && report.failure.is_none() {
        report.content = nonempty(final_message).map(str::to_owned).or(streamed);
        if report.content.is_none() {
            report.failure = Some(Failure::new(
                FailureKind::Unknown,
                "Codex completed the turn without a final message.",
            ));
            report.retry_missing_final = true;
        }
    } else if report.failure.is_none() {
        let stderr = String::from_utf8_lossy(&run.stderr.bytes);
        report.failure = last_error.or_else(|| {
            let parsed = diagnostic(&stderr);
            (parsed.kind != FailureKind::Unknown).then_some(parsed)
        });
    }
    report
}
