//! Operating system support for D6 and D18.

#[cfg(unix)]
mod session;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{empty_accounting, exit_signal, spawn};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{empty_accounting, exit_signal, spawn};
