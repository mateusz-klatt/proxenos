//! Runs one child process to its end (D3, D6).
//!
//! The child gets no standard input. Its output is captured as bytes up to a limit, and read to
//! the end beyond it, so that a full pipe never blocks it. It is stopped on its deadline or when
//! the caller cancels. After it has exited, a descendant that still holds one of its pipes is
//! given a short time, and then whatever is left of its group or job is killed.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::platform::{self, Group};

/// How long the pipes are read after the child has exited, before its descendants are killed,
/// and again after that.
const DRAIN: Duration = Duration::from_secs(2);

/// What to start.
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// Variables to set in the child's environment, over the inherited ones.
    pub env_set: Vec<(OsString, OsString)>,
    /// Inherited variables to remove.
    pub env_remove: Vec<OsString>,
    pub deadline: Duration,
    /// How many bytes of each stream are kept.
    pub capture_limit: usize,
}

/// How the child came to an end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Exited,
    TimedOut,
    Cancelled,
}

impl Ending {
    pub fn name(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One output stream, as captured.
#[derive(Default)]
pub struct Captured {
    /// At most the capture limit.
    pub bytes: Vec<u8>,
    /// Everything the stream carried, kept or not.
    pub total: u64,
}

/// A child that has been reaped.
pub struct Finished {
    pub ending: Ending,
    /// The exit code, when the child exited with one.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
    pub stdout: Captured,
    pub stderr: Captured,
    pub elapsed: Duration,
    /// When the last output arrived, counted from the start.
    pub last_output: Option<Duration>,
}

/// The child could not be started or put under supervision.
pub struct LaunchError(pub io::Error);

/// Starts the child, waits for its end, and returns what it left.
pub async fn run(launch: Launch, cancel: CancellationToken) -> Result<Finished, LaunchError> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &launch.cwd {
        command.current_dir(cwd);
    }
    for name in &launch.env_remove {
        command.env_remove(name);
    }
    command.envs(launch.env_set.iter().map(|(name, value)| (name, value)));
    platform::prepare(&mut command);

    let started = Instant::now();
    let mut child = command.spawn().map_err(LaunchError)?;
    // A child that cannot be put under supervision is not left running.
    let group = Group::attach(&child)
        .inspect_err(|_| drop(child.start_kill()))
        .map_err(LaunchError)?;

    let last_output = Arc::new(Mutex::new(None));
    let stdout = Arc::new(Mutex::new(Captured::default()));
    let stderr = Arc::new(Mutex::new(Captured::default()));
    let mut readers = JoinSet::new();
    for (pipe, sink) in [
        (child.stdout.take().map(boxed), &stdout),
        (child.stderr.take().map(boxed), &stderr),
    ] {
        readers.spawn(read(
            pipe,
            Arc::clone(sink),
            Arc::clone(&last_output),
            started,
            launch.capture_limit,
        ));
    }

    let ending = tokio::select! {
        _ = child.wait() => Ending::Exited,
        () = tokio::time::sleep(launch.deadline) => Ending::TimedOut,
        () = cancel.cancelled() => Ending::Cancelled,
    };
    let status = match ending {
        Ending::Exited => child.wait().await,
        Ending::TimedOut | Ending::Cancelled => group.stop(&mut child).await,
    };
    let elapsed = started.elapsed();

    // A descendant may still hold a pipe: give it a little time, then end the group, then stop
    // reading whatever an escaped process still holds.
    let _ = tokio::time::timeout(DRAIN, drain(&mut readers)).await;
    group.kill();
    let _ = tokio::time::timeout(DRAIN, drain(&mut readers)).await;
    readers.abort_all();

    let (code, signal) = exit_of(status.ok());
    Ok(Finished {
        ending,
        code,
        signal,
        stdout: take(&stdout),
        stderr: take(&stderr),
        elapsed,
        last_output: *lock(&last_output),
    })
}

type Pipe = Box<dyn AsyncRead + Send + Unpin>;

fn boxed(pipe: impl AsyncRead + Send + Unpin + 'static) -> Pipe {
    Box::new(pipe)
}

async fn read(
    pipe: Option<Pipe>,
    sink: Arc<Mutex<Captured>>,
    last_output: Arc<Mutex<Option<Duration>>>,
    started: Instant,
    limit: usize,
) {
    let Some(mut pipe) = pipe else { return };
    let mut chunk = vec![0_u8; 8192];
    while let Ok(read @ 1..) = pipe.read(&mut chunk).await {
        *lock(&last_output) = Some(started.elapsed());
        let mut captured = lock(&sink);
        let room = limit.saturating_sub(captured.bytes.len()).min(read);
        captured.bytes.extend_from_slice(&chunk[..room]);
        captured.total += read as u64;
    }
}

async fn drain(readers: &mut JoinSet<()>) {
    while readers.join_next().await.is_some() {}
}

fn exit_of(status: Option<ExitStatus>) -> (Option<i32>, Option<i32>) {
    status.map_or((None, None), |status| {
        (status.code(), platform::exit_signal(&status))
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn take(captured: &Mutex<Captured>) -> Captured {
    std::mem::take(&mut *lock(captured))
}
