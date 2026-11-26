//! Cursor print mode emits one JSON object on a successful turn.
use std::path::Path;

use serde::Deserialize;

use super::{CommandLine, Failure, Provider, Report, Turn, is_canonical_uuid};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

pub(super) const SPEC: Provider = Provider {
    name: "cursor",
    cli: "cursor-agent",
    #[cfg(unix)]
    install_dirs: &[("HOME", ".local/bin")],
    #[cfg(windows)]
    install_dirs: &[],
    efforts: &[],
    start_settings: &["model", "cwd", "access", "mode", "instructions"],
    reply_settings: &["cwd", "access", "mode", "instructions"],
    access: &[(
        "full",
        "Cursor receives --force to allow commands except those explicitly denied. Proxenos leaves workspace trust, MCP approval and sandbox settings to Cursor.",
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
        return Err("Cursor thread_id must be a canonical session UUID.".into());
    }
    Ok(())
}

fn argv(turn: &Turn, _output: &Path) -> CommandLine {
    let mut args = vec![
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--force".into(),
    ];
    if let Some(id) = &turn.thread_id {
        args.push(format!("--resume={id}").into());
    } else if let Some(model) = &turn.settings.model {
        args.extend(["--model".into(), model.into()]);
    }
    args.extend([
        "--".into(),
        format!("{}\n\n{}", turn.guidance, turn.prompt).into(),
    ]);
    CommandLine {
        args,
        cli_timeout_ms: None,
    }
}

#[derive(Deserialize)]
struct Terminal {
    #[serde(rename = "type")]
    kind: String,
    subtype: String,
    is_error: bool,
    result: String,
    session_id: String,
}

fn parse(run: &Finished, _output: &str, expected: Option<&str>) -> Report {
    const AUTH: &str = "Error: Authentication required. Please run 'agent login' first, or set CURSOR_API_KEY environment variable.";
    let mut report = Report::default();
    if run.code != Some(0) {
        if run.code == Some(1)
            && run.stdout.total == 0
            && run.stderr.total == run.stderr.bytes.len() as u64
            && run.stderr.bytes.trim_ascii_end() == AUTH.as_bytes()
        {
            report.failure = Some(Failure::new(FailureKind::Authentication, AUTH));
        }
        return report;
    }
    // Serde also permits a positional array when deserializing a struct. Cursor specifies an object.
    if run.stdout.total != run.stdout.bytes.len() as u64
        || !run.stdout.bytes.trim_ascii_start().starts_with(b"{")
    {
        return report;
    }
    let Ok(terminal) = serde_json::from_slice::<Terminal>(&run.stdout.bytes) else {
        return report;
    };
    if terminal.kind != "result"
        || terminal.subtype != "success"
        || terminal.is_error
        || terminal.result.trim().is_empty()
    {
        return report;
    }
    if !is_canonical_uuid(&terminal.session_id)
        || expected.is_some_and(|id| id != terminal.session_id)
    {
        report.failure = Some(Failure::new(
            FailureKind::Unknown,
            "Cursor did not confirm the session UUID.",
        ));
        return report;
    }
    report.completed = true;
    report.thread_id = Some(terminal.session_id);
    report.content = Some(terminal.result);
    report
}
