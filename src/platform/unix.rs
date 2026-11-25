//! Each child leads a new Unix session and process group.
#![allow(unsafe_code)]

use std::io;
use std::mem::MaybeUninit;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::session;

const GRACE: Duration = Duration::from_secs(3);

pub struct Group {
    id: libc::pid_t,
    changes: Signal,
}

pub fn spawn(command: &mut Command) -> io::Result<(Child, Group)> {
    // Install the handler before spawn. An inherited SIG_IGN for SIGCHLD would otherwise
    // let the kernel reap a short-lived child before waitid could observe its exit.
    let changes = signal(SignalKind::child())?;
    // Some clients block SIGCHLD in every thread. Let this serving thread receive exits and
    // let the child use SIGCHLD for its own descendants.
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
    // SAFETY: create only calls setsid and reads errno, without allocating or taking locks.
    unsafe { command.pre_exec(session::create) };
    let child = command.spawn()?;
    let pid = child.id().expect("a newly spawned child has its pid");
    Ok((
        child,
        Group {
            id: pid.cast_signed(),
            changes,
        },
    ))
}

impl Group {
    /// Observe exit without reaping. Until the final group signal, the leader's PID must stay
    /// reserved even if all its descendants exit or leave the group.
    pub async fn wait_for_exit(&mut self, _child: &mut Child) -> io::Result<()> {
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
                self.id.cast_unsigned(),
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

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: the leader has not been reaped. The negative PID selects only its group.
        let _ = unsafe { libc::kill(-self.id, signal) };
    }

    pub async fn stop(&self) {
        self.signal(libc::SIGTERM);
        // A descendant may need the grace even after the leader has exited and closed its pipes.
        tokio::time::sleep(GRACE).await;
        self.kill();
    }

    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }
}

pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}
