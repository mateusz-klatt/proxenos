//! Kimi Code runs one turn with `-p` and emits JSONL under `--output-format stream-json`.

use std::path::Path;

use serde_json::Value;

use super::{CommandLine, Failure, Provider, Report, Turn};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "cwd", "access", "mode", "instructions"];
const MAX_SESSION_ID_LEN: usize = 128;

pub(super) const SPEC: Provider = Provider {
    name: "kimi",
    cli: "kimi",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin"), ("HOME", ".kimi-code/bin")],
    #[cfg(windows)]
    install_dirs: &[("USERPROFILE", ".kimi-code/bin")],
    efforts: &[],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[(
        "full",
        "Kimi print mode has no permission tier (it rejects --plan, --yolo and --auto with -p) and runs tools unattended; read-only is unsupported. Kimi auto-loads AGENTS.md from the working directory with no disable switch.",
    )],
    not_claimed: &[
        "That the delegation was free of the working directory's AGENTS.md; Kimi Code auto-loads it and has no switch to disable it.",
    ],
    read_only_not_claimed: &[],
    fixed_instructions: false,
    reply_model: false,
    prepare,
    argv,
    parse,
};

fn is_valid_id(id: &str) -> bool {
    (1..=MAX_SESSION_ID_LEN).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn prepare(turn: &Turn) -> Result<(), String> {
    if let Some(id) = &turn.thread_id
        && !is_valid_id(id)
    {
        return Err("thread_id must be a Kimi session identifier.".into());
    }
    Ok(())
}

fn argv(turn: &Turn, _output: &Path) -> CommandLine {
    let mut args = vec!["--output-format".into(), "stream-json".into()];
    // Resume only by explicit session ID; never pass --continue / -c, which
    // resumes the previous session for the directory and cross-talks between calls.
    if let Some(id) = &turn.thread_id {
        args.extend(["-S".into(), id.into()]);
    }
    if let Some(model) = &turn.settings.model {
        args.extend(["-m".into(), model.into()]);
    }
    args.extend([
        "-p".into(),
        format!("{}\n\n{}", turn.guidance, turn.prompt).into(),
    ]);
    CommandLine {
        args,
        cli_timeout_ms: None,
    }
}

fn error_message(stderr: &str) -> &str {
    stderr
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("error: failed to run prompt:")
                .or_else(|| trimmed.strip_prefix("error:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| stderr.trim())
}

fn classify(stderr: &str) -> Option<Failure> {
    let message = error_message(stderr);
    if message.is_empty() {
        return None;
    }
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("is not configured in config.toml")
        || lower.contains("cannot combine --prompt with")
        || (lower.contains("session \"")
            && (lower.contains("\" not found")
                || lower.contains("\" was created under a different directory")))
    {
        FailureKind::InvalidRequest
    } else if [
        "/login to sign in",
        "401 unauthorized",
        "invalid_api_key",
        "authentication required",
        "not authenticated",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        FailureKind::Authentication
    } else if lower.contains("insufficient_quota")
        || lower.contains("usage limit")
        || (lower.contains("quota")
            && ["exhaust", "exceed", "limit"]
                .iter()
                .any(|needle| lower.contains(needle)))
    {
        FailureKind::Quota
    } else if [
        "content_filter",
        "content_policy_violation",
        "content policy",
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
        failure.reset_at = ["resets at ", "reset at ", "try again at "]
            .into_iter()
            .find_map(|marker| {
                let start = lower.find(marker)? + marker.len();
                let when = message[start..].trim().trim_end_matches('.');
                (!when.is_empty()).then(|| when.to_owned())
            });
    }
    Some(failure)
}

fn parse(finished: &Finished, _output: &str, expected: Option<&str>) -> Report {
    let mut report = Report::default();
    let mut messages = Vec::new();
    let mut raw_id: Option<String> = None;
    let mut conflicting_hints = false;
    let mut saw_resume_hint = false;

    for event in finished
        .stdout
        .bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
    {
        match (event["role"].as_str(), event["content"].as_str()) {
            (Some("assistant"), Some(text)) if !text.trim().is_empty() => {
                messages.push(text.to_owned());
            }
            (Some("meta"), _) if event["type"] == "session.resume_hint" => {
                let next_id = event["session_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                if saw_resume_hint && raw_id != next_id {
                    conflicting_hints = true;
                } else {
                    raw_id = next_id;
                }
                saw_resume_hint = true;
            }
            _ => {}
        }
    }

    if conflicting_hints {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Kimi emitted conflicting session IDs.",
        ));
        return report;
    }

    report.thread_id = raw_id
        .as_deref()
        .filter(|id| is_valid_id(id))
        .map(str::to_owned);

    let id_matches = expected.is_none() || expected == report.thread_id.as_deref();
    let foreign_id = expected.is_some() && raw_id.is_some() && expected != raw_id.as_deref();
    if foreign_id || (finished.code == Some(0) && saw_resume_hint && !id_matches) {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Kimi did not confirm the requested session ID.",
        ));
        return report;
    }

    let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
    if finished.code == Some(0) && saw_resume_hint {
        if messages.is_empty() {
            report.failure = Some(classify(&stderr).unwrap_or_else(|| {
                Failure::new(
                    FailureKind::Unknown,
                    "Kimi completed the turn without an assistant answer.",
                )
            }));
        } else {
            report.content = Some(messages.join("\n\n"));
            report.completed = true;
        }
    } else {
        report.failure = classify(&stderr);
    }
    report
}
