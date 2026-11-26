//! D2 wire result. A CLI can supply the fields in `reported` only.

mod content;

pub use content::{AnswerArchive, CAPTURE_LIMIT};

use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::delegation::Attempt;
use crate::mcp::ToolOutcome;
use crate::providers::{Provider, Report, Turn};
use crate::supervise::Accounting;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    NotFound,
    #[cfg(windows)]
    UnsupportedLauncher,
    Launch,
    Busy,
    Timeout,
    Cancellation,
    MemoryLimit,
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
    state: &'static str,
    observed: Value,
    pub report: Report,
    attempts: Vec<Value>,
    accounting: Option<Accounting>,
    not_claimed: &'static [&'static str],
    access_not_claimed: &'static [&'static str],
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
            state: "finished",
            report: Report::default(),
            attempts: Vec::new(),
            accounting: None,
            not_claimed: provider.not_claimed,
            access_not_claimed: &[],
            observed: json!({
                "provider": provider.name,
                "requested": requested,
                "state": "finished",
                "failure": null,
                "passed": null,
                "access_mechanism": null,
                "accounting": null
            }),
        }
    }

    pub fn started(&self) -> Instant {
        self.started
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
        if result.spawned {
            if self.observed["passed"].is_null() {
                self.observed["passed"] = passed;
            }
            let mechanism = provider
                .access
                .iter()
                .find(|(name, _)| *name == turn.settings.access)
                .map(|(_, text)| text);
            self.observed["access_mechanism"] = json!(mechanism);
            if turn.settings.access == "read-only" {
                self.access_not_claimed = provider.read_only_not_claimed;
            }
        }
        self.accounting = Some(merge_accounting(self.accounting.take(), &result.accounting));
        self.attempts.push(json!({
            "exit_code": result.code,
            "signal": result.signal,
            "elapsed_ms": result.elapsed.as_millis(),
            "stdout_bytes": result.stdout.total,
            "stderr_bytes": result.stderr.total,
            "operation": if reply { "reply" } else { "start" },
            "deadline_ms": attempt.deadline.as_millis(),
            "cli_timeout_ms": attempt.cli_timeout_ms,
            "provider_file_bytes": attempt.file_bytes,
            "provider_file_captured": attempt.file_captured,
            "stdout_captured": result.stdout.bytes.len(),
            "stderr_captured": result.stderr.bytes.len(),
            "last_output_ms": result.last_output.map(|at| at.as_millis()),
            "accounting": result.accounting,
        }));
    }

    pub fn snapshot(&mut self, when: &str, value: Value) {
        self.observed["git"][when] = value;
    }

    pub fn session_save_failure(&mut self, failure: &Failure) {
        self.observed["session_save_failure"] = json!(failure);
    }

    pub fn fail(&mut self, failure: Failure) {
        self.state = if failure.kind == FailureKind::Cancellation {
            "cancelled"
        } else {
            "failed"
        };
        self.observed["state"] = json!(self.state);
        self.observed["failure"] = json!(failure);
    }

    pub fn finalize(mut self) -> (&'static str, Value) {
        if self.report.failure.is_some() && self.state == "finished" {
            self.state = "failed";
            self.observed["state"] = json!(self.state);
        }
        self.observed["duration_ms"] = json!(self.elapsed().as_millis());
        self.observed["attempts"] = json!(self.attempts);
        self.observed["accounting"] = json!(self.accounting);
        let mut not_claimed = vec![
            "The provider followed its requested mode.",
            "The CLI loaded particular instruction files.",
            "A review established a verdict.",
            "A git snapshot identifies the author of a change or captures every edit.",
        ];
        not_claimed.extend_from_slice(self.not_claimed);
        not_claimed.extend_from_slice(self.access_not_claimed);
        let value = json!({
            "schema_version": 1,
            "observed": self.observed,
            "reported": self.report,
            "not_claimed": not_claimed
        });
        (self.state, value)
    }

    pub fn outcome(self) -> ToolOutcome {
        let (state, value) = self.finalize();
        ToolOutcome {
            is_error: state != "finished",
            text: value.to_string(),
        }
    }
}

fn combine_opt(left: Option<u64>, right: Option<u64>, join: fn(u64, u64) -> u64) -> Option<u64> {
    left.zip(right).map(|(a, b)| join(a, b)).or(left).or(right)
}

fn merge_accounting(existing: Option<Accounting>, next: &Accounting) -> Accounting {
    let Some(mut acc) = existing else {
        return next.clone();
    };
    acc.peak_memory_bytes = combine_opt(acc.peak_memory_bytes, next.peak_memory_bytes, u64::max);
    acc.cpu_time_ms = combine_opt(acc.cpu_time_ms, next.cpu_time_ms, u64::saturating_add);
    acc.samples = combine_opt(acc.samples, next.samples, u64::saturating_add);
    acc.memory_limit_exceeded |= next.memory_limit_exceeded;
    acc
}
