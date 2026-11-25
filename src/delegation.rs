//! One foreground call, from preflight through reaping, interpretation and D19 continuity.

mod git;

use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::discovery::{Executable, exit_description};
use crate::envelope::{CAPTURE_LIMIT, Envelope, Failure, FailureKind};
use crate::mcp::{Activity, ToolOutcome};
use crate::providers::{Provider, Report, Turn};
use crate::supervise::cleanup::DirectoryLease;
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

const POST_SPAWN: [(FailureKind, &str); 2] = [
    (
        FailureKind::Capture,
        "Could not read the provider output file",
    ),
    (
        FailureKind::Unknown,
        "Supervision failed after starting the provider",
    ),
];

impl Registry {
    pub async fn delegate(
        &self,
        p: &'static Provider,
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
        p: &'static Provider,
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
        let start = if reply {
            match args
                .get("thread_id")
                .and_then(Value::as_str)
                .and_then(|id| {
                    self.starts
                        .lock()
                        .unwrap()
                        .get(&(p.name, id.into()))
                        .cloned()
                }) {
                Some(None) => {
                    return Err(Failure::new(
                        FailureKind::InvalidRequest,
                        "Multiple starts reported this thread_id with conflicting settings. Start a new session instead.",
                    ));
                }
                Some(Some(settings)) => Some(settings),
                None => None,
            }
        } else {
            None
        };
        let mut turn = p
            .turn(args, reply, start)
            .map_err(|message| Failure::new(FailureKind::InvalidRequest, message))?;
        prepare(p, &turn).await?;
        let call_deadline = result.started() + Duration::from_millis(turn.timeout_ms);
        let exe = match self
            .discovery
            .resolve(p.name, cancel, call_deadline, activity.clone())
            .await
        {
            Ok(exe) => exe,
            Err(error) => {
                result.report.diagnostic = error.diagnostic;
                return Err(error.failure);
            }
        };
        if let Some(snapshot) = git::snapshot(
            &turn.settings.cwd,
            cancel,
            Some(call_deadline),
            activity.clone(),
        )
        .await
        {
            result.snapshot("before", snapshot);
        }

        let mut retry = false;
        let outcome = loop {
            // Discovery and workspace inspection can outlast a conversation store entry.
            if let Err(error) = prepare(p, &turn).await {
                break Err(error);
            }
            let attempt = launch(&exe, p, &turn, call_deadline, cancel, activity.clone()).await;
            let attempt = match attempt {
                Ok(done) => done,
                Err(error) => {
                    result.report = Report {
                        thread_id: result.report.thread_id.take(),
                        ..Report::default()
                    };
                    break Err(Failure::new(FailureKind::Launch, error.to_string()));
                }
            };
            result.observe(&attempt, p, &turn);
            let finished = &attempt.finished;
            if matches!(finished.ending, Ending::Exited)
                && let Some(error) = attempt.error
            {
                result.report = Report {
                    thread_id: result.report.thread_id.take(),
                    ..Report::default()
                };
                break Err(error);
            }
            let prior_thread = retry.then(|| result.report.thread_id.take()).flatten();
            result.report = (p.parse)(finished, &attempt.output, turn.thread_id.as_deref());
            if result.report.thread_id.is_none() {
                result.report.thread_id = prior_thread;
            }
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
                match self.starts.lock().unwrap().entry((p.name, id.clone())) {
                    Entry::Vacant(slot) => {
                        slot.insert(Some(turn.settings.clone()));
                    }
                    Entry::Occupied(mut slot) => {
                        if slot.get().as_ref() != Some(&turn.settings) {
                            slot.insert(None);
                        }
                    }
                }
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
                let status = exit_description(finished.code, finished.signal);
                break Err(Failure::new(
                    FailureKind::AbnormalExit,
                    format!("The provider process exited unsuccessfully ({status})."),
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
        if let Some(snapshot) = git::snapshot(&turn.settings.cwd, cancel, None, activity).await {
            result.snapshot("after", snapshot);
        }
        result.excerpt(&self.answers).await;
        outcome
    }
}

async fn prepare(p: &'static Provider, turn: &Turn) -> Result<(), Failure> {
    let check = turn.clone();
    tokio::task::spawn_blocking(move || (p.prepare)(&check))
        .await
        .expect("preflight worker finished")
        .map_err(|reason| Failure::new(FailureKind::InvalidRequest, reason))
}

pub struct Attempt {
    pub finished: Finished,
    pub output: String,
    pub error: Option<Failure>,
    pub file_bytes: u64,
    pub file_captured: usize,
    pub deadline: Duration,
    pub cli_timeout_ms: Option<u64>,
}

async fn read_output(path: &Path) -> io::Result<(Vec<u8>, u64)> {
    let file = tokio::fs::File::open(path).await?;
    let total = file.metadata().await?.len();
    let mut bytes = Vec::new();
    file.take(CAPTURE_LIMIT as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok((bytes, total))
}

async fn launch(
    exe: &Executable,
    p: &Provider,
    turn: &Turn,
    deadline: Instant,
    cancel: &CancellationToken,
    activity: Activity,
) -> io::Result<Attempt> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let setup_activity = activity.clone();
    let (scratch, output) = tokio::task::spawn_blocking(move || {
        let scratch = DirectoryLease::create_blocking(setup_activity)?;
        let output = scratch.path().join("output");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&output)?;
        Ok::<_, io::Error>((scratch, output))
    })
    .await
    .expect("scratch setup worker finished")?;
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
        temporary_files: vec![scratch.for_child()],
    };
    let mut done = supervise::run(launch, cancel, activity).await?;
    let read = if done.spawned && done.error.is_none() {
        read_output(&output).await
    } else {
        Ok((Vec::new(), 0))
    };
    scratch.finish().await;
    let (kind, detail) = POST_SPAWN[usize::from(done.error.is_some())];
    let (bytes, file_bytes, read_error) = match read {
        Ok((bytes, file_bytes)) => (bytes, file_bytes, None),
        Err(error) => (Vec::new(), 0, Some(error)),
    };
    let error = done
        .error
        .take()
        .or(read_error)
        .map(|err| Failure::new(kind, format!("{detail}: {err}")));
    Ok(Attempt {
        finished: done,
        output: String::from_utf8_lossy(&bytes).into_owned(),
        error,
        file_bytes,
        file_captured: bytes.len(),
        deadline: remaining,
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
