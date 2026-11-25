//! One foreground call, from preflight through reaping, interpretation and D19 continuity.

mod git;

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
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
                .and_then(|id| self.starts.lock().unwrap().get(p.name, id).cloned())
            {
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
        let call_deadline = result.started() + Duration::from_millis(turn.timeout_ms);
        prepare(p, &turn, call_deadline, cancel, activity.clone()).await?;
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
            if turn.thread_id.is_some()
                && let Err(error) = prepare(p, &turn, call_deadline, cancel, activity.clone()).await
            {
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
            if let Some(error) = attempt.error {
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
            if !reply
                && result.report.completed
                && result.report.failure.is_none()
                && let Some(id) = &result.report.thread_id
            {
                self.starts
                    .lock()
                    .unwrap()
                    .record(p.name, id.clone(), turn.settings.clone());
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

async fn await_bounded<T>(
    fut: impl Future<Output = Result<T, Failure>>,
    deadline: Instant,
    cancel: &CancellationToken,
    what: &str,
) -> Result<T, Failure> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Failure::new(
            FailureKind::Cancellation,
            format!("{what} was cancelled."),
        )),
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err(Failure::new(
            FailureKind::Timeout,
            format!("{what} exceeded the call's deadline."),
        )),
        result = fut => result,
    }
}

async fn prepare(
    p: &'static Provider,
    turn: &Turn,
    deadline: Instant,
    cancel: &CancellationToken,
    activity: Activity,
) -> Result<(), Failure> {
    let check = turn.clone();
    let task = tokio::task::spawn_blocking(move || {
        let result = (p.prepare)(&check);
        drop(activity);
        result
    });
    await_bounded(
        async {
            task.await
                .expect("preflight worker finished")
                .map_err(|reason| Failure::new(FailureKind::InvalidRequest, reason))
        },
        deadline,
        cancel,
        "Provider preflight",
    )
    .await
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

fn require_regular(meta: &std::fs::Metadata) -> io::Result<()> {
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provider output is not a regular file",
        ));
    }
    Ok(())
}

fn open_output_blocking(path: &Path) -> io::Result<(std::fs::File, u64)> {
    require_regular(&std::fs::symlink_metadata(path)?)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    );
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    require_regular(&metadata)?;
    Ok((file, metadata.len()))
}

async fn read_regular_file(path: PathBuf, activity: Activity) -> io::Result<(Vec<u8>, u64)> {
    let (file, total) = tokio::task::spawn_blocking(move || {
        let result = open_output_blocking(&path);
        drop(activity);
        result
    })
    .await
    .expect("output open worker finished")?;
    let mut bytes = Vec::new();
    tokio::fs::File::from_std(file)
        .take(CAPTURE_LIMIT as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok((bytes, total))
}

async fn read_output(
    path: PathBuf,
    deadline: Instant,
    cancel: &CancellationToken,
    activity: Activity,
) -> Result<(Vec<u8>, u64), Failure> {
    await_bounded(
        async {
            read_regular_file(path, activity).await.map_err(|err| {
                Failure::new(
                    FailureKind::Capture,
                    format!("Could not read the provider output file: {err}"),
                )
            })
        },
        deadline,
        cancel,
        "Reading the provider output file",
    )
    .await
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
    let mut done = supervise::run(launch, cancel, activity.clone()).await?;
    let read = if done.spawned && done.error.is_none() && matches!(done.ending, Ending::Exited) {
        read_output(output, deadline, cancel, activity).await
    } else {
        Ok((Vec::new(), 0))
    };
    scratch.finish().await;
    let supervision = done.error.take();
    let is_supervision = supervision.is_some();
    let (bytes, file_bytes, read_error) = match read {
        Ok((bytes, file_bytes)) => (bytes, file_bytes, None),
        Err(error) => (Vec::new(), 0, Some(error)),
    };
    let (read_kind, read_msg) = read_error.map_or((FailureKind::Capture, None), |failure| {
        (failure.kind, Some(failure.message))
    });
    let kind = [read_kind, FailureKind::Unknown][usize::from(is_supervision)];
    let prefix =
        ["", "Supervision failed after starting the provider: "][usize::from(is_supervision)];
    let error = supervision
        .as_ref()
        .map(ToString::to_string)
        .or(read_msg)
        .map(|msg| Failure::new(kind, format!("{prefix}{msg}")));
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
