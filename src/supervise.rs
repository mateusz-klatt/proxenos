//! The delegation task owns the child, its output pipes and its temporary files (D3, D6).

pub(crate) mod cleanup;

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::mcp::Activity;
use crate::platform;

/// How long a descendant may hold the pipes after the child exits or its group is stopped.
const DRAIN: Duration = Duration::from_secs(2);

/// Resource usage and limit status observed for one supervised launch (D18).
#[derive(Clone, Debug, Serialize)]
pub struct Accounting {
    pub method: &'static str,
    pub caveats: &'static [&'static str],
    pub memory_limit_bytes: Option<u64>,
    pub peak_memory_bytes: Option<u64>,
    pub cpu_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<u64>,
    pub memory_limit_exceeded: bool,
}

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    /// Absolute deadline checked immediately before native spawn and enforced while running.
    pub deadline: Instant,
    /// Optional memory limit in bytes (D18). None by default.
    pub memory_limit_bytes: Option<u64>,
    /// How many bytes to keep from each stream.
    pub capture_limit: usize,
    /// Files created exclusively for this launch. Their ownership passes to the supervisor.
    pub temporary_files: Vec<cleanup::TemporaryFile>,
}

#[derive(Clone, Copy)]
pub enum Ending {
    Exited,
    TimedOut,
    Cancelled,
    #[cfg(unix)]
    MemoryLimit,
}

pub(crate) enum StartError {
    Io(io::Error),
    Skipped(Ending),
}

impl From<io::Error> for StartError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(feature = "test-tools")]
impl Ending {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            #[cfg(unix)]
            Self::MemoryLimit => "memory_limit",
        }
    }
}

#[derive(Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub total: u64,
    pub eof: bool,
    last_output: Option<Duration>,
}

impl Captured {
    pub fn truncated(&self) -> bool {
        self.total > self.bytes.len() as u64
    }
}

impl Captured {
    async fn read(
        &mut self,
        mut pipe: impl AsyncRead + Unpin,
        limit: usize,
        started: Instant,
    ) -> io::Result<()> {
        let mut chunk = [0; 8192];
        loop {
            let count = pipe.read(&mut chunk).await?;
            if count == 0 {
                self.eof = true;
                return Ok(());
            }
            self.last_output = Some(started.elapsed());
            self.total += count as u64;
            let keep = count.min(limit - self.bytes.len());
            self.bytes.extend_from_slice(&chunk[..keep]);
        }
    }
}

pub struct Finished {
    pub spawned: bool,
    pub ending: Ending,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Captured,
    pub stderr: Captured,
    pub elapsed: Duration,
    /// Time of the most recent read on either stream, relative to the start of the launch.
    pub last_output: Option<Duration>,
    /// Error raised while supervising a child that had already started.
    pub error: Option<io::Error>,
    /// Resource usage and optional limit observations (D18).
    pub accounting: Accounting,
}

/// The worker publishes the status as soon as it reaps the leader. Give file cleanup the
/// remainder of the same budget. The worker retains the files and its shutdown handle.
pub(crate) async fn finish_reap(
    status: &mut oneshot::Receiver<io::Result<ExitStatus>>,
    cleaned: &mut oneshot::Receiver<()>,
) -> io::Result<ExitStatus> {
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    let status = tokio::time::timeout_at(until, status)
        .await?
        .map_err(io::Error::other)??;
    let _ = tokio::time::timeout_at(until, cleaned).await;
    Ok(status)
}

pub(crate) fn check_start(cancel: &CancellationToken, deadline: Instant) -> Result<(), StartError> {
    if cancel.is_cancelled() {
        return Err(StartError::Skipped(Ending::Cancelled));
    }
    if Instant::now() >= deadline {
        return Err(StartError::Skipped(Ending::TimedOut));
    }
    Ok(())
}

pub(crate) async fn write_stdin(
    writer: &mut Option<io::PipeWriter>,
    payload: String,
) -> io::Result<()> {
    let mut pipe = writer.take().expect("stdin is open");
    let (pipe, result) = tokio::task::spawn_blocking(move || {
        let result = pipe
            .write_all(payload.as_bytes())
            .and_then(|()| pipe.flush());
        (pipe, result)
    })
    .await
    .expect("child stdin writer finished");
    *writer = Some(pipe);
    result
}

pub(crate) async fn until_interrupted<T, E>(
    operation: impl Future<Output = T>,
    interrupt: impl Future<Output = E>,
) -> Result<T, E> {
    tokio::select! {
        biased;
        reason = interrupt => Err(reason),
        value = operation => Ok(value),
    }
}

/// An interrupted wait cannot say whether native process creation already started.
pub(crate) async fn run_with_interruptible_spawn<E>(
    launch: Launch,
    cancel: &CancellationToken,
    activity: Activity,
    interrupt: impl Future<Output = E>,
) -> Result<io::Result<Finished>, E> {
    let (accepted, received) = oneshot::channel();
    let mut running = pin!(run_inner(launch, cancel, activity, Some(accepted)));
    tokio::select! {
        biased;
        // Once the supervisor owns the child, wait for its complete D6 result.
        _ = received => Ok(running.await),
        result = until_interrupted(&mut running, interrupt) => result,
    }
}

pub async fn run(
    launch: Launch,
    cancel: &CancellationToken,
    activity: Activity,
) -> io::Result<Finished> {
    run_inner(launch, cancel, activity, None).await
}

async fn run_inner(
    mut launch: Launch,
    cancel: &CancellationToken,
    activity: Activity,
    accepted: Option<oneshot::Sender<()>>,
) -> io::Result<Finished> {
    let started = Instant::now();
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &launch.cwd {
        command.current_dir(cwd);
    }
    for name in &launch.env_remove {
        command.env_remove(name);
    }
    command.envs(launch.env_set.iter().map(|(name, value)| (name, value)));

    // The platform makes this check immediately before native spawn, after preparing the job
    // or acquiring the Unix spawn lock. A successful spawn transfers its files to the reaper.
    let memory_limit_bytes = launch.memory_limit_bytes;
    let queued = cancel.child_token();
    let _cancel_abandoned_launch = queued.clone().drop_guard();
    let (publish, launched) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let child = platform::spawn(
            command,
            platform::Input::Closed,
            &mut launch.temporary_files,
            memory_limit_bytes,
            activity.clone(),
            || check_start(&queued, launch.deadline),
        );
        let _ = publish.send(child);
        // Only failed or skipped launches have files left here. This worker holds its
        // shutdown handle while removing them, even if a diagnostic blocks on stderr.
        cleanup::remove_files_blocking(launch.temporary_files);
    });
    let mut child = match launched.await.map_err(io::Error::other)? {
        Ok(child) => child,
        Err(reason) => {
            return match reason {
                StartError::Io(error) => Err(error),
                StartError::Skipped(ending) => Ok(Finished {
                    spawned: false,
                    ending,
                    code: None,
                    signal: None,
                    stdout: Captured::default(),
                    stderr: Captured::default(),
                    elapsed: started.elapsed(),
                    last_output: None,
                    error: None,
                    accounting: platform::empty_accounting(memory_limit_bytes),
                }),
            };
        }
    };
    if let Some(accepted) = accepted {
        let _ = accepted.send(());
    }
    let out = child.stdout.take().expect("stdout was piped");
    let err = child.stderr.take().expect("stderr was piped");
    let mut stdout = Captured::default();
    let mut stderr = Captured::default();
    let (ending, read_result) = {
        let read = async {
            let (out, err) = tokio::join!(
                stdout.read(out, launch.capture_limit, started),
                stderr.read(err, launch.capture_limit, started)
            );
            out.and(err)
        };
        let watch = async {
            let remaining = launch.deadline.saturating_duration_since(Instant::now());
            #[cfg(unix)]
            let ending = tokio::select! {
                biased;
                result = child.wait_for_exit() => match result? {
                    Ending::Exited => return Ok(Ending::Exited),
                    ending => ending,
                },
                () = cancel.cancelled() => Ending::Cancelled,
                () = tokio::time::sleep(remaining) => Ending::TimedOut,
            };
            #[cfg(windows)]
            let ending = tokio::select! {
                biased;
                result = child.wait_for_exit() => return result.map(|()| Ending::Exited),
                () = cancel.cancelled() => Ending::Cancelled,
                () = tokio::time::sleep(remaining) => Ending::TimedOut,
            };
            child.stop().await?;
            Ok(ending)
        };
        let mut read = pin!(read);
        let mut watch = pin!(watch);
        tokio::select! {
            result = &mut read => (watch.await, result),
            ending = &mut watch => (ending, tokio::time::timeout(DRAIN, read).await.unwrap_or(Ok(()))),
        }
    };
    // Close the group or job even when a reader or the stop failed. Preserve the first error
    // alongside the dispatch evidence and accounting that were already observed.
    let (status, accounting) = child.finish().await;
    let code = status.as_ref().ok().and_then(ExitStatus::code);
    let signal = status.as_ref().ok().and_then(platform::exit_signal);
    let ending_kind = ending.as_ref().copied().unwrap_or(Ending::Exited);
    let error = ending.err().or(status.err()).or(read_result.err());
    Ok(Finished {
        spawned: true,
        ending: ending_kind,
        code,
        signal,
        elapsed: started.elapsed(),
        last_output: stdout.last_output.max(stderr.last_output),
        stdout,
        stderr,
        error,
        accounting,
    })
}
