//! The delegation task owns the child, its output pipes and its temporary files (D3, D6).

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

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
    pub temporary_files: Vec<PathBuf>,
}

pub enum Ending {
    Exited,
    TimedOut,
    Cancelled,
}

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

/// Owns the files through reap, including when the MCP request is cancelled. A failed spawn
/// also releases the files. Attempt every removal even if one fails.
pub async fn run(launch: Launch, cancel: &CancellationToken) -> io::Result<Finished> {
    let finished = run_child(&launch, cancel).await;
    let mut removed = Ok(());
    for path in launch.temporary_files {
        removed = removed.and(std::fs::remove_file(path));
    }
    let finished = finished?;
    removed?;
    Ok(finished)
}

async fn run_child(launch: &Launch, cancel: &CancellationToken) -> io::Result<Finished> {
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

    let started = Instant::now();
    let (mut child, mut group) = platform::spawn(&mut command)?;
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
                result = group.wait_for_exit(&mut child) => return result.map(|()| Ending::Exited),
                () = cancel.cancelled() => Ending::Cancelled,
                () = tokio::time::sleep(remaining) => Ending::TimedOut,
            };
            group.stop().await;
            Ok(ending)
        };
        let mut read = pin!(read);
        let mut watch = pin!(watch);
        tokio::select! {
            result = &mut read => (watch.await, result),
            ending = &mut watch => (ending, tokio::time::timeout(DRAIN, read).await.unwrap_or(Ok(()))),
        }
    };
    // A descendant may have closed its pipes and kept running. On Unix the leader still owns
    // its PID here, so the final signal cannot be delivered to a recycled process-group ID.
    group.kill();
    let status = child.wait().await?;
    read_result?;
    Ok(Finished {
        ending: ending?,
        code: status.code(),
        signal: platform::exit_signal(&status),
        elapsed: started.elapsed(),
        last_output: stdout.last_output.max(stderr.last_output),
        stdout,
        stderr,
    })
}
