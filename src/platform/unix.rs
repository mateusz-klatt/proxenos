//! A session per child, with its leader's PID reserved until the final group signal.
#![allow(unsafe_code)]

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child as NativeChild, Command, ExitStatus};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdout};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;

use super::session;
use crate::mcp::Activity;
use crate::supervise::cleanup::{TemporaryFile, remove_files_blocking};
use crate::supervise::{StartError, finish_reap};

const GRACE: Duration = Duration::from_secs(3);
static SPAWNING: Mutex<()> = Mutex::new(());

struct Leader {
    process: NativeChild,
    temporary_files: Vec<TemporaryFile>,
}

pub struct Child {
    pid: libc::pid_t,
    changes: Signal,
    leader: Option<Leader>,
    reap: mpsc::Sender<Leader>,
    status: oneshot::Receiver<io::Result<ExitStatus>>,
    cleaned: oneshot::Receiver<()>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

pub fn spawn(
    mut command: Command,
    files: &mut Vec<TemporaryFile>,
    activity: Activity,
    before_spawn: impl FnOnce() -> Result<(), StartError>,
) -> Result<Child, StartError> {
    // Install the handler before spawn. An inherited SIG_IGN for SIGCHLD would otherwise
    // let the kernel reap a short-lived child before waitid could observe its exit.
    let changes = signal(SignalKind::child())?;
    // Some clients block SIGCHLD in every thread. Unmask it on the blocking launcher so its
    // reaper can receive exit notifications and its child can use SIGCHLD for descendants.
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the storage, SIGCHLD is valid, and pthread_sigmask
    // reads the set for this call only. A null last argument discards the previous mask.
    let error = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGCHLD);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, set.as_ptr(), std::ptr::null_mut())
    };
    (error == 0)
        .then_some(())
        .ok_or(io::Error::from_raw_os_error(error))?;
    // The reaper exists before the child does. Runtime shutdown can drop the supervisor,
    // and a denied stop can outlive the request that reports it. In either case this thread
    // can still wait for the leader and remove its files as long as the server is alive.
    let (reap, released) = mpsc::channel::<Leader>();
    let (report_status, status) = oneshot::channel();
    let (report_cleaned, cleaned) = oneshot::channel();
    let reap_when_released = move || {
        if let Ok(mut leader) = released.recv() {
            let status = leader.process.wait();
            let files_to_remove = status.is_ok().then_some(leader.temporary_files);
            let _ = report_status.send(status);
            remove_files_blocking(files_to_remove.as_deref().unwrap_or_default());
        }
        let _ = report_cleaned.send(());
        drop(activity);
    };
    thread::Builder::new()
        .name("proxenos-reap".into())
        .spawn(reap_when_released)?;

    // Pipe creation and FD_CLOEXEC can be separate operations on macOS. Serialize both our
    // output pipes and std's exec-status pipe against every other fork in the supervisor.
    let spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    let (out, stdout_writer) = io::pipe()?;
    let (err, stderr_writer) = io::pipe()?;
    let stdout = ChildStdout::from_std(OwnedFd::from(out).into())?;
    let stderr = ChildStderr::from_std(OwnedFd::from(err).into())?;
    command.stdout(stdout_writer).stderr(stderr_writer);
    // SAFETY: create only calls setsid and reads errno, without allocating or taking locks.
    unsafe { command.pre_exec(session::create) };
    before_spawn()?;
    // All pipe registration has already succeeded. From this point onward setup is infallible.
    let process = command.spawn()?;
    drop(spawning);
    let pid = process.id().cast_signed();
    Ok(Child {
        pid,
        changes,
        leader: Some(Leader {
            process,
            temporary_files: std::mem::take(files),
        }),
        reap,
        status,
        cleaned,
        stdout: Some(stdout),
        stderr: Some(stderr),
    })
}

impl Child {
    /// Observe exit without reaping so that the final group signal still targets our group.
    pub async fn wait_for_exit(&mut self) -> io::Result<()> {
        while !self.exited()? {
            self.changes.recv().await;
        }
        Ok(())
    }

    fn exited(&self) -> io::Result<bool> {
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid receives writable storage and a positive PID for our own child.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid.cast_unsigned(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        (rc == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)?;
        // SAFETY: waitid succeeded and si_pid is the exit-report field. It is zero if the
        // child is still running, including on systems that leave the zeroed buffer alone.
        Ok(unsafe { info.assume_init_ref().si_pid() } != 0)
    }

    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: until the leader is handed to the reaper its PID reserves the group ID.
        let result = unsafe { libc::kill(-self.pid, signal) };
        let error = io::Error::last_os_error();
        // XNU returns EPERM when only the zombie remains. Keep the PID reserved while
        // checking the leader. An unsignalable descendant beside that zombie looks the same.
        #[cfg(target_os = "macos")]
        if result != 0 && error.raw_os_error() == Some(libc::EPERM) && self.exited()? {
            return Ok(());
        }
        (result == 0 || error.raw_os_error() == Some(libc::ESRCH))
            .then_some(())
            .ok_or(error)
    }

    pub async fn stop(&self) -> io::Result<()> {
        self.signal(libc::SIGTERM)?;
        // A descendant may need the grace even after the leader has exited and closed its pipes.
        tokio::time::sleep(GRACE).await;
        self.signal(libc::SIGKILL)
    }

    fn release(&mut self) {
        // Moving the sole native Child disarms the drop guard before wait can recycle its PID.
        let leader = self.leader.take().expect("the group still owns its leader");
        self.reap
            .send(leader)
            .expect("reaper waits for this leader");
    }

    pub async fn finish(mut self) -> io::Result<ExitStatus> {
        self.signal(libc::SIGKILL)?;
        self.release();
        finish_reap(&mut self.status, &mut self.cleaned).await
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.leader.is_some() {
            let _ = self.signal(libc::SIGKILL);
            self.release();
        }
    }
}

pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}
