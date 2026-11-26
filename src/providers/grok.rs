//! Grok Build writes one JSON object for a headless turn.
use std::path::Path;

use serde_json::Value;

use super::{
    CommandLine, Failure, Provider, Report, Turn, is_canonical_uuid, is_valid_model, token_usage,
};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

const SETTINGS: &[&str] = &["model", "effort", "cwd", "access", "mode", "instructions"];

pub(super) const SPEC: Provider = Provider {
    name: "grok",
    cli: "grok",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin"), ("HOME", ".grok/bin")],
    #[cfg(windows)]
    install_dirs: &[],
    efforts: &["low", "medium", "high", "xhigh"],
    start_settings: SETTINGS,
    reply_settings: SETTINGS,
    access: &[(
        "full",
        "Grok auto-approves tool executions with --always-approve. We have not verified a read-only configuration across its tools.",
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
        return Err("Grok thread_id must be a canonical session UUID.".into());
    }
    Ok(())
}

fn argv(turn: &Turn, _output: &Path) -> CommandLine {
    let mut args = vec![
        "--output-format".into(),
        "json".into(),
        "--always-approve".into(),
    ];
    if let Some(id) = &turn.thread_id {
        args.extend(["--resume".into(), id.into()]);
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

// A model usage entry records model calls. Multiple entries do not identify one answer model.
fn sole_model(usage: &Value) -> Option<String> {
    let mut entries = usage.as_object()?.iter();
    let (model, counts) = entries.next()?;
    (entries.next().is_none()
        && is_valid_model(model)
        && counts["modelCalls"].as_u64().is_some_and(|n| n > 0))
    .then(|| model.clone())
}

fn parse(run: &Finished, _output: &str, expected: Option<&str>) -> Report {
    let mut report = Report::default();
    let Ok(value) = serde_json::from_slice::<Value>(&run.stdout.bytes) else {
        return report;
    };
    if value["type"] == "error" {
        if let Some(message) = value["message"].as_str().filter(|s| !s.trim().is_empty()) {
            let kind = if message.lines().next()
                == Some("Not signed in. To authenticate without a browser, run:")
            {
                FailureKind::Authentication
            } else {
                FailureKind::Unknown
            };
            report.failure = Some(Failure::new(kind, message));
        }
        return report;
    }
    if run.code != Some(0) {
        return report;
    }
    let id = value["sessionId"]
        .as_str()
        .filter(|id| is_canonical_uuid(id));
    if id.is_none() || expected.is_some() && expected != id {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Grok did not confirm the session UUID.",
        ));
        return report;
    }
    if value["stopReason"] != "end_turn" || value["num_turns"].as_u64().is_none_or(|n| n == 0) {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Grok did not report a completed turn.",
        ));
        return report;
    }
    let Some(text) = value["text"].as_str().filter(|s| !s.trim().is_empty()) else {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Grok completed the turn without an answer.",
        ));
        return report;
    };
    report.completed = true;
    report.thread_id = id.map(str::to_owned);
    report.content = Some(text.to_owned());
    report.model = sole_model(&value["modelUsage"]);
    report.usage = token_usage(
        &value["usage"],
        &[
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
        ],
    );
    report
}
