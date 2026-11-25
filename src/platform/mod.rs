//! Operating system support for D6.

#[cfg(unix)]
mod session;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{exit_signal, spawn};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{exit_signal, spawn};
