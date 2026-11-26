//! Grok Build writes one JSON object for a headless turn.

use serde_json::Value;

use super::{
    CommandLine, Failure, Provider, Report, is_canonical_uuid, is_valid_model, token_usage,
};
use crate::envelope::FailureKind;
use crate::supervise::Finished;

pub(super) const SPEC: Provider = Provider::full_access_print(
    "grok",
    &[("HOME", ".local/bin"), ("HOME", ".grok/bin")],
    &["low", "medium", "high", "xhigh"],
    &[(
        "full",
        "Grok auto-approves tool executions with --always-approve. We have not verified a read-only configuration across its tools.",
    )],
    |turn| turn.require_session_uuid("Grok"),
    |turn, _| {
        let resume = turn
            .thread_id
            .iter()
            .flat_map(|id| ["--resume".into(), id.into()]);
        CommandLine::for_print_turn(turn, &["--always-approve"], resume)
    },
    parse,
);

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
    if run.stdout.total > run.stdout.bytes.len() as u64 {
        return report;
    }
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
