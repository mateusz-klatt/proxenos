//! A stand-in provider CLI. Arguments describe, in order, what the child does on a test run.

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
#[path = "platform/session.rs"]
mod session;

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let mut locks = Vec::new();
    for step in std::env::args().skip(1) {
        let (name, value) = step.split_once(':').unwrap_or((&step, ""));
        match name {
            "out" => writeln!(io::stdout(), "{value}")?,
            "err" => writeln!(io::stderr(), "{value}")?,
            "flood" => io::stdout().write_all(&vec![0xff; value.parse()?])?,
            "flood-err" => io::stderr().write_all(&vec![0xfe; value.parse()?])?,
            "sleep" => thread::sleep(Duration::from_millis(value.parse()?)),
            "stdin" => {
                let count = io::copy(&mut io::stdin().lock(), &mut io::sink())?;
                writeln!(io::stdout(), "stdin={count}")?;
            }
            "env" => match std::env::var_os(value) {
                Some(found) => writeln!(io::stdout(), "{value}={}", found.display())?,
                None => writeln!(io::stdout(), "{value} unset")?,
            },
            "cwd" => writeln!(io::stdout(), "{}", std::env::current_dir()?.display())?,
            "exists" => writeln!(io::stdout(), "exists={}", Path::new(value).try_exists()?)?,
            "hold" => {
                let lock = File::create(value)?;
                lock.lock()?;
                fs::write(format!("{value}.ready"), [])?;
                locks.push(lock);
            }
            "grandchild" => drop(grandchild(value)?.spawn()?),
            #[cfg(unix)]
            "ignore-term" => platform::ignore_term(),
            #[cfg(unix)]
            "session" => platform::report_session()?,
            #[cfg(unix)]
            "escape" => {
                let mut copy = Command::new(std::env::current_exe()?);
                copy.arg("detach")
                    .arg(format!("hold:{value}"))
                    .arg(format!("until-released:{value}"));
                drop(copy.spawn()?);
            }
            #[cfg(unix)]
            "detach" => session::create()?,
            #[cfg(unix)]
            "until-released" => platform::until_released(value),
            #[cfg(windows)]
            "breakaway" => platform::breakaway(value)?,
            #[cfg(windows)]
            "terminal" => writeln!(io::stdout(), "terminal={}", File::open("CONIN$").is_ok())?,
            _ => {
                let code = step.strip_prefix("exit:").ok_or("unknown stub argument")?;
                return Ok(ExitCode::from(code.parse::<u8>()?));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Leave a live descendant holding the output pipes. The ready file ties the test observation
/// to this instance: its lock survives until this process exits, including death by signal.
fn grandchild(path: &str) -> io::Result<Command> {
    let mut copy = Command::new(std::env::current_exe()?);
    #[cfg(unix)]
    copy.arg("ignore-term");
    copy.arg(format!("hold:{path}"))
        .arg("out:grandchild ready")
        .arg("sleep:60000");
    Ok(copy)
}

#[cfg(unix)]
mod platform {
    #![allow(unsafe_code)]

    use std::fs::File;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    pub(super) fn ignore_term() {
        // SAFETY: SIG_IGN installs no handler and SIGTERM is a valid, catchable signal.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }

    pub(super) fn report_session() -> io::Result<()> {
        // SAFETY: both calls query the current process and take no pointers.
        let leader = unsafe { libc::getsid(0) == libc::getpid() };
        writeln!(io::stdout(), "session_leader={leader}")?;
        writeln!(io::stdout(), "terminal={}", File::open("/dev/tty").is_ok())
    }

    pub(super) fn until_released(path: &str) {
        let release = PathBuf::from(format!("{path}.release"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !release.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io::{self, Write};
    use std::os::windows::process::CommandExt;

    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;

    /// Ask Windows to create this grandchild outside the job. Report the OS reason for the
    /// refusal and retry the same grandchild in the job, where the test can witness its death.
    pub(super) fn breakaway(path: &str) -> io::Result<()> {
        let mut copy = super::grandchild(path)?;
        let refusal = copy
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .expect_err("the job allowed a grandchild to escape");
        writeln!(
            io::stdout(),
            "breakaway={}",
            refusal.raw_os_error().unwrap_or_default()
        )?;
        drop(copy.creation_flags(0).spawn()?);
        Ok(())
    }
}
