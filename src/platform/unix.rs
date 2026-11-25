//! Linux and macOS: Proxenos leaves its terminal, and each child leads its own process group.
#![allow(unsafe_code)]

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long a child has between the termination signal and the kill signal.
const GRACE: Duration = Duration::from_secs(3);

/// Starts a new session for Proxenos, so that neither it nor any child it starts has a terminal
/// to prompt on. It fails when Proxenos already leads a process group, as when it is run from a
/// shell; it then keeps the terminal it has.
pub fn leave_terminal() -> bool {
    // SAFETY: setsid takes no arguments and changes only this process's session.
    unsafe { libc::setsid() != -1 }
}

/// Makes the child the leader of a new process group, which its descendants share.
pub fn prepare(command: &mut Command) {
    command.process_group(0);
}

/// The process group of one child.
pub struct Group {
    id: libc::pid_t,
}

impl Group {
    /// The child leads its own group, whose id is its process id.
    pub fn attach(child: &Child) -> io::Result<Self> {
        let pid = child.id().ok_or_else(|| io::Error::other("the child was already reaped"))?;
        let id = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        Ok(Self { id })
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: kill takes plain integers. A group that no longer exists is not an error here.
        let _ = unsafe { libc::kill(-self.id, signal) };
    }

    /// Asks the group to end, and kills it if the child has not exited within the grace time.
    pub async fn stop(&self, child: &mut Child) -> io::Result<ExitStatus> {
        self.signal(libc::SIGTERM);
        match tokio::time::timeout(GRACE, child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                self.signal(libc::SIGKILL);
                child.wait().await
            }
        }
    }

    /// Kills what is left of the group, such as a descendant still holding a pipe.
    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }
}

/// The signal that ended the process, if one did.
pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}
