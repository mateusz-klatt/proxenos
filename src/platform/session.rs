//! Session creation shared by the supervisor and the stub descendant that leaves its group.
#![allow(unsafe_code)]

use std::io;

pub fn create() -> io::Result<()> {
    // SAFETY: setsid takes no arguments and is async-signal-safe between fork and exec.
    let id = unsafe { libc::setsid() };
    (id != -1)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}
