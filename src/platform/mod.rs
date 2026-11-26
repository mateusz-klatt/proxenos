//! Operating system support for D6 and D18.

use std::io;
use std::process::{Command, Stdio};

pub enum Input {
    Closed,
    Piped,
}

impl Input {
    fn attach(self, command: &mut Command) -> io::Result<Option<io::PipeWriter>> {
        match self {
            Self::Closed => {
                command.stdin(Stdio::null());
                Ok(None)
            }
            Self::Piped => {
                let (reader, writer) = io::pipe()?;
                command.stdin(reader);
                Ok(Some(writer))
            }
        }
    }
}

#[cfg(unix)]
mod session;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{current_uid, empty_accounting, exit_signal, spawn};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{empty_accounting, exit_signal, spawn};
