//! One foreground call, from preflight through reaping, interpretation and D19 continuity.

mod git;

use std::ffi::OsString;
use std::io;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::discovery::Executable;
use crate::envelope::{CAPTURE_LIMIT, Envelope, Failure, FailureKind};
use crate::mcp::{Activity, ToolOutcome};
use crate::providers::{Provider, Turn};
use crate::supervise::cleanup::{PrivateDirectory, TemporaryFile};
use crate::supervise::{self, Ending, Finished, Launch};
use crate::tools::Registry;

const DEPTH: &[&str] = &[
    "PROXENOS_DEPTH",
    "CLAUDE_DELEGATOR_CODEX_DEPTH",
    "CLAUDE_DELEGATOR_AGY_DEPTH",
    "CLAUDE_DELEGATOR_CLAUDE_DEPTH",
    "CLAUDE_DELEGATOR_KIMI_DEPTH",
    "CLAUDE_DELEGATOR_GROK_DEPTH",
    "CLAUDE_DELEGATOR_CURSOR_DEPTH",
    "CLAUDE_DELEGATOR_COPILOT_DEPTH",
];

impl Registry {
    pub async fn delegate(
        &self,
        p: &Provider,
        reply: bool,
        args: Map<String, Value>,
        cancel: CancellationToken,
        activity: Activity,
    ) -> ToolOutcome {
        let mut result = Envelope::new(p, &args);
        let outcome = self
            .run(p, reply, &args, &cancel, activity, &mut result)
            .await;
        if let Err(error) = outcome {
            result.fail(error);
        }
        result.outcome()
    }

    async fn run(
        &self,
        p: &Provider,
        reply: bool,
        args: &Map<String, Value>,
        cancel: &CancellationToken,
        activity: Activity,
        result: &mut Envelope,
    ) -> Result<(), Failure> {
        if DEPTH.iter().any(|key| {
            std::env::var(key).is_ok_and(|s| s.trim().parse::<u64>().is_ok_and(|n| n > 0))
        }) {
            return Err(Failure::new(
                FailureKind::DepthLimit,
                "A delegated CLI cannot start another Proxenos delegation.",
            ));
        }
        let start = args
            .get("thread_id")
            .and_then(Value::as_str)
            .and_then(|id| {
                self.starts
                    .lock()
                    .unwrap()
                    .get(&(p.name, id.into()))
                    .cloned()
            });
        let mut turn = p
            .turn(args, reply, start)
            .map_err(|message| Failure::new(FailureKind::InvalidRequest, message))?;
        let exe = self
            .discovery
            .resolve(p.name, cancel, activity.clone())
            .await?;
        let timeout = Duration::from_millis(turn.settings.timeout_ms);
        if let Some(snapshot) = git::snapshot(&turn.settings.cwd, cancel, activity.clone()).await {
            result.snapshot("before", snapshot);
        }

        let mut retry = false;
        let outcome = loop {
            // Discovery and workspace inspection can outlast a conversation store entry.
            if let Err(reason) = (p.prepare)(&turn) {
                break Err(Failure::new(FailureKind::InvalidRequest, reason));
            }
            let remaining = timeout.saturating_sub(result.elapsed());
            let attempt = launch(&exe, p, &turn, remaining, cancel, activity.clone()).await;
            let attempt = match attempt {
                Ok(done) => done,
                Err(error) => break Err(Failure::new(FailureKind::Launch, error.to_string())),
            };
            result.observe(&attempt, p, &turn);
            let finished = &attempt.finished;
            result.report = (p.parse)(finished, &attempt.output, turn.thread_id.as_deref());
            if (finished.code != Some(0) || !result.report.completed)
                && !finished.stderr.bytes.is_empty()
            {
                let diagnostic = &finished.stderr.bytes[..finished.stderr.bytes.len().min(2048)];
                result.report.diagnostic = Some(String::from_utf8_lossy(diagnostic).into_owned());
            }
            if !reply
                && !retry
                && let Some(id) = &result.report.thread_id
            {
                self.starts
                    .lock()
                    .unwrap()
                    .entry((p.name, id.clone()))
                    .or_insert_with(|| turn.settings.clone());
            }
            match finished.ending {
                Ending::TimedOut => {
                    break Err(Failure::new(
                        FailureKind::Timeout,
                        "The provider exceeded the call's deadline.",
                    ));
                }
                Ending::Cancelled => {
                    break Err(Failure::new(
                        FailureKind::Cancellation,
                        "The provider call was cancelled.",
                    ));
                }
                Ending::Exited => (),
            }
            if !retry
                && result.report.retry_missing_final
                && matches!(finished.code, Some(0 | 1))
                && let Some(id) = &result.report.thread_id
                && turn
                    .thread_id
                    .as_ref()
                    .is_none_or(|expected| expected == id)
            {
                turn.thread_id = Some(id.clone());
                turn.prompt =
                    "Return the final answer for the preceding turn using the work already done."
                        .into();
                retry = true;
                continue;
            }
            if finished.code != Some(0) {
                break Err(Failure::new(
                    FailureKind::AbnormalExit,
                    "The provider process exited unsuccessfully.",
                ));
            }
            if !result.report.completed && result.report.failure.is_none() {
                if finished.stdout.total > finished.stdout.bytes.len() as u64 {
                    break Err(Failure::new(
                        FailureKind::Capture,
                        "The captured prefix of stdout contains no terminal result. The CLI wrote beyond the capture limit.",
                    ));
                }
                break Err(Failure::new(
                    FailureKind::Unknown,
                    "The CLI exited without evidence of a completed turn.",
                ));
            }
            break Ok(());
        };
        if let Some(snapshot) = git::snapshot(&turn.settings.cwd, cancel, activity).await {
            result.snapshot("after", snapshot);
        }
        result.excerpt(&self.answers).await;
        outcome
    }
}

pub struct Attempt {
    pub finished: Finished,
    pub output: String,
    pub file_bytes: u64,
    pub file_captured: usize,
    pub deadline: Duration,
    pub cli_timeout_ms: Option<u64>,
}

async fn launch(
    exe: &Executable,
    p: &Provider,
    turn: &Turn,
    deadline: Duration,
    cancel: &CancellationToken,
    activity: Activity,
) -> io::Result<Attempt> {
    let scratch = PrivateDirectory::create()?;
    let output = scratch.path.join("output");
    let reader = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&output)
        .await?;
    let mut args = exe.prefix.clone();
    let command = (p.argv)(turn, &output);
    args.extend(command.args);
    let launch = Launch {
        program: exe.program.clone(),
        args,
        cwd: Some(turn.settings.cwd.clone()),
        env_set: env_set(),
        env_remove: env_remove(),
        deadline,
        capture_limit: CAPTURE_LIMIT,
        temporary_files: vec![TemporaryFile::Directory(scratch.clone())],
    };
    let done = supervise::run(launch, cancel, activity).await?;
    let file_bytes = reader.metadata().await?.len();
    let mut bytes = Vec::new();
    reader
        .take(CAPTURE_LIMIT as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(Attempt {
        finished: done,
        output: String::from_utf8_lossy(&bytes).into_owned(),
        file_bytes,
        file_captured: bytes.len(),
        deadline,
        cli_timeout_ms: command.cli_timeout_ms,
    })
}

pub(crate) fn env_remove() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            matches!(
                key.to_string_lossy().to_ascii_uppercase().as_str(),
                "CLAUDECODE" | "IRIS_EXECUTION" | "CODEX_DELEGATOR_CODEX_BIN"
            )
        })
        .collect()
}

pub(crate) fn env_set() -> Vec<(OsString, OsString)> {
    DEPTH
        .iter()
        .map(|key| (OsString::from(key), OsString::from("1")))
        .collect()
}
