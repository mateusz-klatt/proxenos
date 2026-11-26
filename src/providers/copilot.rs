//! Copilot print mode emits events followed by a result for the session.
use std::path::Path;

use serde_json::{Value, json};

use super::{CommandLine, Failure, Provider, Report, Turn, is_canonical_uuid, is_valid_model};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "effort", "cwd", "access", "mode", "instructions"];

pub(super) const SPEC: Provider = Provider {
    name: "copilot",
    cli: "copilot",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin")],
    #[cfg(windows)]
    install_dirs: &[],
    efforts: &["none", "minimal", "low", "medium", "high", "xhigh", "max"],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[(
        "full",
        "Copilot runs with --allow-all. CLI deny rules and organization policy still apply. No read-only configuration has been verified across its tools.",
    )],
    fixed_instructions: false,
    reply_model: false,
    not_claimed: &[],
    read_only_not_claimed: &[],
    prepare,
    argv,
    parse,
};

fn prepare(turn: &mut Turn) -> Result<(), String> {
    if turn
        .thread_id
        .as_deref()
        .is_some_and(|id| !is_canonical_uuid(id))
    {
        return Err("Copilot thread_id must be a canonical session UUID.".into());
    }
    Ok(())
}

fn argv(turn: &Turn, _output: &Path) -> CommandLine {
    let mut args = vec![
        "--output-format".into(),
        "json".into(),
        "--stream".into(),
        "off".into(),
        "--no-custom-instructions".into(),
        "--allow-all".into(),
    ];
    if let Some(id) = &turn.thread_id {
        args.push(format!("--resume={id}").into());
    }
    if let Some(model) = &turn.settings.model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = &turn.settings.effort {
        args.extend(["--reasoning-effort".into(), effort.into()]);
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

fn cli_failure(stderr: &[u8], expected: Option<&str>) -> Option<Failure> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().next()?;
    let kind = if line == "Error: No authentication information found." {
        FailureKind::Authentication
    } else if expected
        .is_some_and(|id| line == format!("Error: No session, task, or name matched '{id}'."))
    {
        FailureKind::InvalidRequest
    } else {
        return None;
    };
    Some(Failure::new(kind, line))
}

fn parse(run: &Finished, _output: &str, expected: Option<&str>) -> Report {
    let mut report = Report::default();
    let mut lines = run
        .stdout
        .bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace));
    let terminal: Value = lines
        .next_back()
        .and_then(|line| serde_json::from_slice(line).ok())
        .unwrap_or(Value::Null);
    let result = (terminal["type"] == "result").then_some(&terminal);
    let status = result.and_then(|event| event["exitCode"].as_i64());
    if run.code != Some(0) || status != Some(0) {
        report.failure = cli_failure(&run.stderr.bytes, expected).or_else(|| {
            status.filter(|&code| code != 0).map(|code| {
                Failure::new(
                    FailureKind::Unknown,
                    format!("Copilot reported exit code {code}."),
                )
            })
        });
        return report;
    }
    let id = terminal["sessionId"]
        .as_str()
        .filter(|id| is_canonical_uuid(id));
    if id.is_none() || expected.is_some() && expected != id {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Copilot did not confirm the session UUID.",
        ));
        return report;
    }

    let mut turn_id = None;
    let mut answer = None;
    let mut ended = false;
    for event in lines.filter_map(|line| serde_json::from_slice::<Value>(line).ok()) {
        let data = &event["data"];
        if !event["agentId"].is_null() || !data["parentToolCallId"].is_null() {
            continue;
        }
        let next_id = data["turnId"].as_str();
        match event["type"].as_str() {
            Some("session.start") if data["sessionId"].as_str() != id => {
                report.failure = Some(Failure::new(
                    FailureKind::Unknown,
                    "Copilot reported conflicting session UUIDs.",
                ));
                return report;
            }
            Some("assistant.turn_start") => {
                turn_id = next_id.map(str::to_owned);
                answer = None;
                ended = false;
            }
            Some("assistant.message")
                if next_id.is_some() && next_id == turn_id.as_deref() && !ended =>
            {
                answer = data["toolRequests"]
                    .as_array()
                    .filter(|tools| tools.is_empty())
                    .and_then(|_| data["content"].as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        (
                            s.to_owned(),
                            data["model"]
                                .as_str()
                                .filter(|m| is_valid_model(m))
                                .map(str::to_owned),
                        )
                    });
            }
            Some("assistant.turn_end") if next_id.is_some() && next_id == turn_id.as_deref() => {
                ended = true
            }
            _ => {}
        }
    }
    if let Some((content, model)) = answer.filter(|_| ended) {
        report.completed = true;
        report.thread_id = id.map(str::to_owned);
        report.content = Some(content);
        report.model = model;
        let premium = &terminal["usage"]["premiumRequests"];
        report.usage = premium
            .as_f64()
            .filter(|n| *n >= 0.0)
            .map(|_| json!({"premium_requests":premium}));
    } else {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Copilot did not report a completed assistant answer.",
        ));
    }
    report
}
