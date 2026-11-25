//! What supervising a child needs from the operating system (D6). The only place, apart from
//! the test stub, where `unsafe` is allowed.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Group, exit_signal, leave_terminal, prepare};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Group, exit_signal, prepare};
