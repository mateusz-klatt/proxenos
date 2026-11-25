//! A stand-in for an agent CLI, used by the tests (D12). Built only with `test-tools`.
//!
//! Each argument is one step, run in order:
//!
//! - `out:TEXT`, `err:TEXT`: write a line to standard output or standard error
//! - `flood:N`: write N bytes to standard output
//! - `sleep:MS`: sleep
//! - `exit:N`: exit with code N
//! - `env:NAME`: write `NAME=value`, or `NAME unset`
//! - `cwd`: write the working directory
//! - `pidfile:PATH`: write this process's id to PATH
//! - `grandchild:PATH`: start a copy that writes its id to PATH and sleeps for a minute, holding
//!   this process's output pipes, and do not wait for it
//! - `alive:PID`: exit 0 if that process is running, 3 if not
//! - `ignore-term` (Unix): ignore the termination signal
//! - `breakaway` (Windows): try to start a copy outside the job, and say whether that was refused

use std::io::Write;
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    for step in std::env::args().skip(1) {
        let (name, value) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match name {
            "out" => println!("{value}"),
            "err" => eprintln!("{value}"),
            "flood" => {
                let size = value.parse().unwrap_or(0);
                let _ = std::io::stdout().write_all(&vec![b'x'; size]);
            }
            "sleep" => std::thread::sleep(Duration::from_millis(value.parse().unwrap_or(0))),
            "exit" => return ExitCode::from(value.parse::<u8>().unwrap_or(1)),
            "env" => match std::env::var(value) {
                Ok(found) => println!("{value}={found}"),
                Err(_) => println!("{value} unset"),
            },
            "cwd" => println!("{}", std::env::current_dir().unwrap_or_default().display()),
            "pidfile" => {
                let _ = std::fs::write(value, std::process::id().to_string());
            }
            "grandchild" => {
                let me = std::env::current_exe().unwrap_or_default();
                let started = Command::new(me)
                    .args([format!("pidfile:{value}"), "sleep:60000".to_owned()])
                    .spawn();
                eprintln!("grandchild started: {}", started.is_ok());
            }
            "alive" => {
                let alive = value.parse().is_ok_and(platform::alive);
                return ExitCode::from(if alive { 0 } else { 3 });
            }
            other => {
                if !platform::step(other) {
                    eprintln!("unknown step {other:?}");
                    return ExitCode::from(64);
                }
            }
        }
    }
    let _ = std::io::stdout().flush();
    ExitCode::SUCCESS
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod platform {
    pub fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    pub fn step(name: &str) -> bool {
        if name != "ignore-term" {
            return false;
        }
        // SAFETY: ignoring a signal installs no handler code.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        true
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, GetExitCodeProcess, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub fn alive(pid: u32) -> bool {
        // SAFETY: plain arguments; a null handle means the process is gone.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        let mut code = 0_u32;
        // SAFETY: the handle is checked for null first and closed after use.
        let running = !handle.is_null()
            && unsafe { GetExitCodeProcess(handle, &raw mut code) } != 0
            && code == STILL_ACTIVE.cast_unsigned();
        // SAFETY: closing a null handle fails harmlessly.
        let _ = unsafe { CloseHandle(handle) };
        running
    }

    pub fn step(name: &str) -> bool {
        if name != "breakaway" {
            return false;
        }
        let me = std::env::current_exe().unwrap_or_default();
        let escaped = Command::new(me)
            .arg("exit:0")
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .status();
        println!("breakaway {}", if escaped.is_ok() { "allowed" } else { "refused" });
        true
    }
}
