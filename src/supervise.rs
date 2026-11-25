//! The delegation task owns the child, its output pipes and its temporary files (D3, D6).

pub(crate) mod cleanup;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::mcp::Activity;
use crate::platform;

/// How long a descendant may hold the pipes after the child exits or its group is stopped.
const DRAIN: Duration = Duration::from_secs(2);

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub deadline: Duration,
    /// How many bytes to keep from each stream.
    pub capture_limit: usize,
    /// Files created exclusively for this launch. Their ownership passes to the supervisor.
    pub temporary_files: Vec<cleanup::TemporaryFile>,
}

pub enum Ending {
    Exited,
    TimedOut,
    Cancelled,
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
        }
    }
}

#[derive(Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub total: u64,
    last_output: Option<Duration>,
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
    pub ending: Ending,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Captured,
    pub stderr: Captured,
    pub elapsed: Duration,
    /// Time of the most recent read on either stream, relative to the start of the launch.
    pub last_output: Option<Duration>,
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

pub async fn run(
    mut launch: Launch,
    cancel: &CancellationToken,
    activity: Activity,
) -> io::Result<Finished> {
    let started = Instant::now();
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

    // The platform makes this check immediately before native spawn, after preparing the job
    // or acquiring the Unix spawn lock. A successful spawn transfers its files to the reaper.
    let queued = cancel.child_token();
    let _cancel_abandoned_launch = queued.clone().drop_guard();
    let (publish, launched) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let child = platform::spawn(
            command,
            &mut launch.temporary_files,
            activity.clone(),
            || {
                if queued.is_cancelled() {
                    return Err(StartError::Skipped(Ending::Cancelled));
                }
                if started.elapsed() >= launch.deadline {
                    return Err(StartError::Skipped(Ending::TimedOut));
                }
                Ok(())
            },
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
                    ending,
                    code: None,
                    signal: None,
                    stdout: Captured::default(),
                    stderr: Captured::default(),
                    elapsed: started.elapsed(),
                    last_output: None,
                }),
            };
        }
    };
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
            let remaining = launch.deadline.saturating_sub(started.elapsed());
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
    // Close the group or job even when a reader or the stop failed. Preserve the first error.
    let status = child.finish().await;
    let ending = ending?;
    let status = status?;
    read_result?;
    Ok(Finished {
        ending,
        code: status.code(),
        signal: platform::exit_signal(&status),
        elapsed: started.elapsed(),
        last_output: stdout.last_output.max(stderr.last_output),
        stdout,
        stderr,
    })
}
