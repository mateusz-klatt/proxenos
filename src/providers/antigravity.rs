//! Antigravity's print command returns one JSON result for the turn.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{CommandLine, Failure, Provider, Report, Turn, is_valid_model};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "cwd", "access", "mode", "instructions"];

pub(super) const SPEC: Provider = Provider {
    name: "antigravity",
    cli: "agy",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin")],
    #[cfg(windows)]
    install_dirs: &[("LOCALAPPDATA", "agy/bin")],
    efforts: &[],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[
        (
            "full",
            "Antigravity approves tools without an interactive prompt.",
        ),
        (
            "read-only",
            "Antigravity denies shell permission in print mode. It can still write files, search the web and fetch URLs.",
        ),
    ],
    fixed_instructions: false,
    reply_model: true,
    not_claimed: &[],
    read_only_not_claimed: &[],
    prepare,
    argv,
    parse,
};

fn conversations() -> Option<PathBuf> {
    [
        "PROXENOS_AGY_CONVERSATIONS_DIR",
        "CLAUDE_DELEGATOR_AGY_CONVERSATIONS_DIR",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(|raw| PathBuf::from(raw.to_str().map(str::trim).map(OsStr::new).unwrap_or(&raw)))
    .find(|path| !path.as_os_str().is_empty())
    .or_else(|| std::env::home_dir().map(|home| home.join(".gemini/antigravity-cli/conversations")))
}

const MAX_CONVERSATION_ID_LEN: usize = 128;

fn is_valid_id(id: &str) -> bool {
    (1..=MAX_CONVERSATION_ID_LEN).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn prepare(turn: &mut Turn) -> Result<(), String> {
    let Some(id) = &turn.thread_id else {
        return Ok(());
    };
    // agy can deliver a prompt addressed to an unknown ID to a different conversation.
    // Check the spelling from the directory listing even on case-insensitive filesystems.
    if !is_valid_id(id) {
        return Err("thread_id must be an Antigravity conversation identifier".into());
    }
    let directory =
        conversations().ok_or("Set PROXENOS_AGY_CONVERSATIONS_DIR to the conversation store")?;
    let mut entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("Cannot inspect {}: {error}", directory.display()))?;
    let names = [
        OsString::from(format!("{id}.db")),
        OsString::from(format!("{id}.pb")),
    ];
    let held = entries.any(|entry| {
        entry.is_ok_and(|entry| {
            names.contains(&entry.file_name()) && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
    });
    if !held {
        return Err(format!(
            "No exact regular conversation file for {id} in {}",
            directory.display()
        ));
    }
    Ok(())
}

fn argv(turn: &Turn, log: &Path) -> CommandLine {
    // agy starts its own timer after it sends the turn. Leave room for startup under
    // the supervisor's deadline so the CLI can report a resumable print timeout.
    let print_ms = turn.timeout_ms.saturating_sub(30_000).max(5_000);
    let mut args = vec![
        "--output-format".into(),
        "json".into(),
        "--disable-slash-commands".into(),
        "--print-timeout".into(),
        format!("{print_ms}ms").into(),
        "--log-file".into(),
        log.into(),
    ];
    if let Some(id) = &turn.thread_id {
        args.extend(["--conversation".into(), id.into()]);
    }
    if let Some(model) = &turn.settings.model {
        args.extend(["--model".into(), model.into()]);
    }
    if turn.settings.access == "full" {
        args.push("--dangerously-skip-permissions".into());
    }
    args.extend([
        "--print".into(),
        format!("{}\n\n{}", turn.guidance, turn.prompt).into(),
    ]);
    CommandLine {
        args,
        cli_timeout_ms: Some(print_ms),
    }
}

fn response(bytes: &[u8]) -> Option<Value> {
    fn object(bytes: &[u8]) -> Option<Value> {
        serde_json::from_slice::<Value>(bytes).ok().filter(|v| {
            v["status"].is_string() && (v.get("response").is_some() || v.get("error").is_some())
        })
    }
    object(bytes).or_else(|| bytes.rsplit(|b| *b == b'\n').find_map(object))
}

// Only CLI error channels (top-level JSON `error` on non-SUCCESS status and stderr) reach
// this classifier, never agent-authored `response` text.
fn classify(message: &str) -> Option<Failure> {
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("quota")
        && ["exhaust", "exceed", "quota limit reached"]
            .iter()
            .any(|s| lower.contains(s))
    {
        FailureKind::Quota
    } else if [
        "unauthenticated",
        "authentication required",
        "authentication failed",
        "not authenticated",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        FailureKind::Authentication
    } else if lower.contains("timeout waiting for response") {
        FailureKind::Timeout
    } else if lower.contains("a tool required the \"command\" permission") {
        FailureKind::Refusal
    } else {
        return None;
    };
    let mut failure = Failure::new(kind, message);
    if kind == FailureKind::Quota {
        failure.reset_at = ["resets at ", "reset at "].into_iter().find_map(|marker| {
            let start = lower.find(marker)? + marker.len();
            Some(message[start..].trim().trim_end_matches('.').to_owned())
        });
    }
    Some(failure)
}

const DIAGNOSTIC_LIMIT: usize = 2048;
const OMITTED_URL: &str = "[sign-in URL omitted]";

fn redact_urls_in_line(line: &str) -> (String, bool) {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut found = false;
    while let Some(offset) = ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| lower[cursor..].find(scheme))
        .min()
    {
        let start = cursor + offset;
        let end = line[start..]
            .find(char::is_whitespace)
            .map_or(line.len(), |len| start + len);
        out.push_str(&line[cursor..start]);
        out.push_str(OMITTED_URL);
        cursor = end;
        found = true;
    }
    out.push_str(&line[cursor..]);
    (out, found)
}

fn is_url_continuation(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'.'
                        | b'_'
                        | b'~'
                        | b':'
                        | b'/'
                        | b'?'
                        | b'#'
                        | b'['
                        | b']'
                        | b'@'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                        | b'%'
                )
        })
}

fn omit_sign_in_urls(text: &str) -> String {
    let mut in_url_continuation = false;
    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let eol = &chunk[line.len()..];
        let (redacted, had_url) = redact_urls_in_line(line);
        if had_url {
            out.push_str(&redacted);
            out.push_str(eol);
            in_url_continuation = redacted.trim_end().ends_with(OMITTED_URL);
        } else if in_url_continuation && is_url_continuation(line) {
            continue;
        } else {
            out.push_str(line);
            out.push_str(eol);
            in_url_continuation = false;
        }
    }
    out
}

fn stderr_auth_fallback(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("authentication required. please visit the url to log in")
        || lower.contains("authentication failed")
}

fn finish_report(mut report: Report, stderr: &str) -> Report {
    if let Some(failure) = &mut report.failure
        && failure.kind == FailureKind::Authentication
    {
        failure.message = omit_sign_in_urls(&failure.message);
        if !stderr.is_empty() {
            let redacted = omit_sign_in_urls(stderr);
            let end = redacted.floor_char_boundary(DIAGNOSTIC_LIMIT);
            report.diagnostic = Some(redacted[..end].to_owned());
        }
    }
    report
}

fn parse(finished: &Finished, _log: &str, expected: Option<&str>) -> Report {
    let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
    let Some(value) = response(&finished.stdout.bytes) else {
        return finish_report(
            Report {
                failure: classify(stderr.trim()),
                ..Report::default()
            },
            &stderr,
        );
    };
    let text = |key: &str| value[key].as_str().filter(|s| !s.trim().is_empty());
    let raw_id = text("conversation_id");
    let mut report = Report {
        thread_id: raw_id.filter(|id| is_valid_id(id)).map(String::from),
        ..Report::default()
    };
    let success = value["status"] == "SUCCESS";
    let id_matches = expected.is_none() || expected == report.thread_id.as_deref();
    let foreign_id = expected.is_some() && raw_id.is_some() && expected != raw_id;
    if foreign_id || (success && !id_matches) {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Antigravity did not confirm the requested conversation ID",
        ));
        return report;
    }
    if id_matches {
        report.content = text("response").map(String::from);
        report.model = text("model")
            .filter(|s| is_valid_model(s))
            .map(|s| s.trim().to_owned());
    }
    if !success {
        let error = text("error");
        let stderr = stderr.trim();
        let reason = error.unwrap_or(stderr);
        let reason = if reason.is_empty() {
            "Antigravity returned an unsuccessful status"
        } else {
            reason
        };
        let failure = classify(reason).or_else(|| {
            error
                .filter(|_| stderr_auth_fallback(stderr))
                .map(|_| Failure::new(FailureKind::Authentication, reason))
        });
        report.failure =
            Some(failure.unwrap_or_else(|| Failure::new(FailureKind::Unknown, reason)));
    } else if report.content.is_none() {
        report.failure = Some(classify(stderr.trim()).unwrap_or_else(|| {
            Failure::new(
                FailureKind::Unknown,
                "Antigravity returned an empty response",
            )
        }));
    } else {
        report.completed = true;
    }
    finish_report(report, &stderr)
}
