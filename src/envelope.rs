//! D2 wire result. A CLI can supply the fields in `reported` only.

mod content;

pub use content::{AnswerArchive, CAPTURE_LIMIT};

use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::delegation::Attempt;
use crate::mcp::ToolOutcome;
use crate::providers::{Provider, Report, Turn};
use crate::supervise::Ending;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    NotFound,
    #[cfg(windows)]
    UnsupportedLauncher,
    Launch,
    Timeout,
    Cancellation,
    AbnormalExit,
    InvalidRequest,
    DepthLimit,
    Capture,
    Authentication,
    Refusal,
    Quota,
    Unknown,
}

#[derive(Clone, Serialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

impl Failure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            reset_at: None,
        }
    }
}

pub struct Envelope {
    started: Instant,
    observed: Value,
    pub report: Report,
    attempts: Vec<Value>,
}

impl Envelope {
    pub fn new(provider: &Provider, args: &Map<String, Value>) -> Self {
        let requested: Map<_, _> = args
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "prompt" | "coordination"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            started: Instant::now(),
            report: Report::default(),
            attempts: Vec::new(),
            observed: json!({"provider":provider.name,"requested":requested,"state":"finished","failure":null,"passed":null,"access_mechanism":null}),
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    pub fn observe(&mut self, attempt: &Attempt, provider: &Provider, turn: &Turn) {
        let result = &attempt.finished;
        let reply = turn.thread_id.is_some();
        let mut passed = json!(turn.settings);
        let supported = provider.supported_settings(reply);
        passed
            .as_object_mut()
            .unwrap()
            .retain(|key, _| supported.contains(&key.as_str()));
        if reply && provider.fixed_instructions {
            passed.as_object_mut().unwrap().remove("mode");
            passed.as_object_mut().unwrap().remove("instructions");
        }
        if !matches!(result.ending, Ending::TimedOut | Ending::Cancelled)
            || result.code.is_some()
            || result.signal.is_some()
        {
            if self.observed["passed"].is_null() {
                self.observed["passed"] = passed;
            }
            let mechanism = provider
                .access
                .iter()
                .find(|(name, _)| *name == turn.settings.access)
                .map(|(_, text)| text);
            self.observed["access_mechanism"] = json!(mechanism);
        }
        self.attempts.push(json!({
            "exit_code":result.code, "signal":result.signal,"elapsed_ms":result.elapsed.as_millis(),
            "stdout_bytes":result.stdout.total,"stderr_bytes":result.stderr.total,
            "operation": if reply {"reply"} else {"start"}, "deadline_ms":attempt.deadline.as_millis(),
            "cli_timeout_ms":attempt.cli_timeout_ms, "provider_file_bytes":attempt.file_bytes,
            "provider_file_captured":attempt.file_captured,
            "stdout_captured":result.stdout.bytes.len(),"stderr_captured":result.stderr.bytes.len(),
            "last_output_ms":result.last_output.map(|at|at.as_millis())
        }));
    }

    pub fn snapshot(&mut self, when: &str, value: Value) {
        self.observed["git"][when] = value;
    }

    pub fn fail(&mut self, failure: Failure) {
        self.observed["state"] = json!(if failure.kind == FailureKind::Cancellation {
            "cancelled"
        } else {
            "failed"
        });
        self.observed["failure"] = json!(failure);
    }

    pub fn outcome(mut self) -> ToolOutcome {
        if self.report.failure.is_some() && self.observed["state"] == "finished" {
            self.observed["state"] = json!("failed");
        }
        self.observed["duration_ms"] = json!(self.elapsed().as_millis());
        self.observed["attempts"] = json!(self.attempts);
        ToolOutcome {
            is_error:self.observed["state"]!="finished",
            text:json!({"schema_version":1,"observed": self.observed,"reported":self.report,
                "not_claimed":["The provider followed its requested mode.","The CLI loaded particular instruction files.","A review established a verdict.","A git snapshot identifies the author of a change or captures every edit."]}).to_string(),
        }
    }
}
