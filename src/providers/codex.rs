//! Codex runs one turn with `exec`. Both start and resume accept config overrides.
use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use super::{CommandLine, Failure, Provider, Report, Turn, is_canonical_uuid};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "cwd", "access", "mode", "instructions", "effort"];

pub(crate) const SPEC: Provider = Provider {
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
    read_only_not_claimed: &[],
    prepare,
    argv,
    parse,
};

fn prepare(turn: &mut Turn) -> Result<(), String> {
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

const MAX_RESET_TIMESTAMP_LEN: usize = 40;

fn two_digits(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [a @ b'0'..=b'9', b @ b'0'..=b'9'] => Some((a - b'0') * 10 + (b - b'0')),
        _ => None,
    }
}

fn is_valid_reset_timestamp(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_RESET_TIMESTAMP_LEN {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes.iter().all(u8::is_ascii_digit) {
        return true;
    }
    // Strict RFC 3339 datetime: YYYY-MM-DDTHH:MM:SS[.digits](Z|+HH:MM|-HH:MM).
    if bytes.len() < 20
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    let (Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        two_digits(&bytes[5..7]),
        two_digits(&bytes[8..10]),
        two_digits(&bytes[11..13]),
        two_digits(&bytes[14..16]),
        two_digits(&bytes[17..19]),
    ) else {
        return false;
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }
    let mut rest = &bytes[19..];
    if let Some(after_dot) = rest.strip_prefix(b".") {
        let frac_len = after_dot.iter().take_while(|b| b.is_ascii_digit()).count();
        if frac_len == 0 {
            return false;
        }
        rest = &after_dot[frac_len..];
    }
    match rest {
        [b'Z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            two_digits(&[*h1, *h2]).is_some_and(|h| h <= 23)
                && two_digits(&[*m1, *m2]).is_some_and(|m| m <= 59)
        }
        _ => false,
    }
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
        // Drop invalid `error.resets_at` strings unconditionally rather than keeping them as
        // text-derived resets, so a bare token there cannot survive even in an untainted report.
        let reset = match &error["resets_at"] {
            Value::Number(timestamp) => Some(timestamp.to_string()),
            Value::String(value) if is_valid_reset_timestamp(value) => Some(value.clone()),
            _ => None,
        };
        if let Some(reset) = reset {
            failure.reset_at = Some(reset);
        } else {
            failure.reset_at = message
                .rsplit_once("try again at ")
                .and_then(|(_, when)| nonempty(when.trim_end_matches('.')).map(str::to_owned));
            failure.reset_at_from_text = failure.reset_at.is_some();
        }
    }
    failure
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
                report.usage = super::token_usage(
                    &event["usage"],
                    &[
                        "input_tokens",
                        "cached_input_tokens",
                        "cache_write_input_tokens",
                        "output_tokens",
                        "reasoning_output_tokens",
                        "total_tokens",
                    ],
                );
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
