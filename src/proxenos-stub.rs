//! A stand-in provider CLI. Arguments describe, in order, what the child does on a test run.

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "platform/session.rs"]
mod session;

const BURN_WALL_LIMIT_MS: u64 = 30_000;

fn main() -> Result<ExitCode, Box<dyn Error>> {
    if let Some(directory) = std::env::var_os("PROXENOS_STUB_DIR") {
        return replay(Path::new(&directory));
    }
    #[cfg(unix)]
    let mut held_memory = Vec::new();
    for step in std::env::args().skip(1) {
        let (name, value) = step.split_once(':').unwrap_or((&step, ""));
        match name {
            "out" => writeln!(io::stdout(), "{value}")?,
            "err" => writeln!(io::stderr(), "{value}")?,
            "flood" => io::stdout().write_all(&vec![0xff; value.parse()?])?,
            "flood-err" => io::stderr().write_all(&vec![0xfe; value.parse()?])?,
            "sleep" => thread::sleep(Duration::from_millis(value.parse()?)),
            "burn" => {
                let (cpu_ms, wall_ms) = match value.split_once(':') {
                    Some((cpu_ms, wall_ms)) => (cpu_ms, wall_ms.parse()?),
                    None => (value, BURN_WALL_LIMIT_MS),
                };
                burn_cpu(cpu_ms.parse()?, wall_ms)?;
            }
            #[cfg(unix)]
            "alloc" => held_memory.push(alloc_and_touch(value.parse()?)?),
            "close-output" => platform::close_output()?,
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
            "remove" => fs::remove_file(value)?,
            "mkdir" => fs::create_dir(value)?,
            "hold" => hold_lock(value)?,
            "grandchild" => spawn_and_wait_ready(&mut grandchild(value, true)?, value)?,
            "quiet-grandchild" => spawn_and_wait_ready(&mut grandchild(value, false)?, value)?,
            "until-released" => wait_for_file(&format!("{value}.release"))?,
            #[cfg(unix)]
            "ignore-term" => platform::ignore_term(),
            #[cfg(unix)]
            "session" => platform::report_session()?,
            #[cfg(unix)]
            "await-term" => platform::await_term(value)?,
            #[cfg(unix)]
            "delayed-grandchild" => {
                let (path, delay) = value.split_once(':').ok_or("missing delay")?;
                let mut copy = Command::new(std::env::current_exe()?);
                copy.arg("ignore-term")
                    .arg(format!("hold:{path}"))
                    .arg(format!("await-term:{path}.term"))
                    .arg(format!("sleep:{delay}"));
                spawn_and_wait_ready(&mut copy, path)?;
            }
            #[cfg(unix)]
            "escape" => {
                let mut copy = Command::new(std::env::current_exe()?);
                copy.arg("detach")
                    .arg(format!("hold:{value}"))
                    .arg(format!("until-released:{value}"));
                spawn_and_wait_ready(&mut copy, value)?;
            }
            #[cfg(unix)]
            "detach" => session::create()?,
            #[cfg(windows)]
            "breakaway" => writeln!(io::stdout(), "breakaway={}", platform::breakaway(value)?)?,
            #[cfg(windows)]
            "window" => writeln!(io::stdout(), "visible={}", platform::visible_console())?,
            _ => {
                let code = step.strip_prefix("exit:").ok_or("unknown stub argument")?;
                return Ok(ExitCode::from(code.parse::<u8>()?));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Record only test inputs. Provider credentials from the surrounding environment never enter
/// the log. Version checks have their own response and consume no turn.
fn replay(root: &Path) -> Result<ExitCode, Box<dyn Error>> {
    use serde_json::{Value, json};
    let invoked = std::env::args_os().next().unwrap();
    let cli = Path::new(&invoked).file_stem().unwrap().to_string_lossy();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let env: serde_json::Map<_, _> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            (key.ends_with("_DEPTH")
                || matches!(
                    key,
                    "IRIS_EXECUTION" | "CLAUDECODE" | "PROXENOS_STUB_MARKER"
                ))
            .then(|| (key.to_owned(), json!(value.to_string_lossy())))
        })
        .collect();
    let record = json!({"cli":cli, "args":args, "cwd":std::env::current_dir()?, "env":env, "pid":std::process::id()});
    let mut log = File::options()
        .create(true)
        .read(true)
        .append(true)
        .open(root.join("invocations"))?;
    log.lock()?;
    log.write_all(format!("{record}\n").as_bytes())?;
    let step = if args.last().is_some_and(|arg| arg == "--version") {
        let version = root.join(format!("{cli}.version"));
        if version.exists() {
            serde_json::from_slice(&fs::read(version)?)?
        } else {
            json!({"stdout": "1.2.3"})
        }
    } else {
        let script = root.join(format!("{cli}.steps"));
        let mut steps: Vec<Value> = serde_json::from_slice(&fs::read(&script)?)?;
        let step = steps.remove(0);
        fs::write(script, serde_json::to_vec(&steps)?)?;
        step
    };
    drop(log);
    if let Some(path) = step["hold_file"].as_str() {
        hold_lock(path)?;
    }
    let mut held_memory = Vec::<u8>::new();
    if let Some(bytes) = step["alloc_bytes"].as_u64() {
        held_memory = alloc_and_touch(bytes as usize)?;
    }
    if let Some(ms) = step["burn_cpu_ms"].as_u64() {
        burn_cpu(ms, BURN_WALL_LIMIT_MS)?;
    }
    if let Some(ms) = step["child_burn_cpu_ms"].as_u64() {
        let mut child = Command::new(std::env::current_exe()?);
        child
            .env_remove("PROXENOS_STUB_DIR")
            .arg(format!("burn:{ms}"))
            .arg(format!(
                "sleep:{}",
                step["child_sleep_ms"].as_u64().unwrap_or(0)
            ));
        let status = child.status()?;
        status.success().then_some(()).ok_or("child burn failed")?;
    }
    if let Some(message) = step["final_message"].as_str() {
        let pos = args
            .iter()
            .position(|arg| arg == "-o")
            .ok_or("missing -o")?;
        fs::write(&args[pos + 1], message)?;
    }
    if let Some(message) = step["replace_final_message"].as_str() {
        let pos = args
            .iter()
            .position(|arg| arg == "-o")
            .ok_or("missing -o")?;
        let target = Path::new(&args[pos + 1]);
        let staging = target.with_extension("tmp");
        fs::write(&staging, message)?;
        fs::rename(&staging, target)?;
    }
    if step["remove_output"] == true {
        let pos = args
            .iter()
            .position(|arg| arg == "-o" || arg == "--log-file")
            .ok_or("missing output flag")?;
        fs::remove_file(&args[pos + 1])?;
    }
    if step["replace_output_with_dir"] == true {
        let pos = args
            .iter()
            .position(|arg| arg == "-o" || arg == "--log-file")
            .ok_or("missing output flag")?;
        fs::remove_file(&args[pos + 1])?;
        fs::create_dir(&args[pos + 1])?;
    }
    #[cfg(unix)]
    if step["replace_output_with_fifo"] == true {
        let pos = args
            .iter()
            .position(|arg| arg == "-o" || arg == "--log-file")
            .ok_or("missing output flag")?;
        platform::replace_with_fifo(Path::new(&args[pos + 1]))?;
    }
    io::stdout().write_all(step["stdout"].as_str().unwrap_or("").as_bytes())?;
    io::stdout().flush()?;
    io::stderr().write_all(step["stderr"].as_str().unwrap_or("").as_bytes())?;
    io::stderr().flush()?;
    if let Some(ready) = step["ready"].as_str() {
        fs::write(ready, [])?;
    }
    if let Some(wait) = step["wait_for"].as_str() {
        wait_for_file(wait)?;
    }
    thread::sleep(Duration::from_millis(
        step["sleep_ms"].as_u64().unwrap_or(0),
    ));
    std::hint::black_box(&held_memory);
    Ok(ExitCode::from(step["code"].as_u64().unwrap_or(0) as u8))
}

fn alloc_and_touch(len: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut memory = Vec::new();
    memory.try_reserve_exact(len)?;
    memory.resize(len, 0);
    for chunk in memory.chunks_mut(4096) {
        chunk[0] = 1;
    }
    std::hint::black_box(&memory);
    Ok(memory)
}

fn burn_cpu(ms: u64, wall_ms: u64) -> Result<(), Box<dyn Error>> {
    let start = platform::cpu_time_ms();
    let deadline = Instant::now() + Duration::from_millis(wall_ms);
    let mut acc = 0_u64;
    while platform::cpu_time_ms().saturating_sub(start) < ms {
        (Instant::now() < deadline)
            .then_some(())
            .ok_or("burn_cpu exceeded its wall-clock ceiling")?;
        for _ in 0..10_000 {
            acc = std::hint::black_box(acc).wrapping_add(1);
        }
    }
    std::hint::black_box(acc);
    Ok(())
}

fn hold_lock(path: &str) -> io::Result<()> {
    // Statics outlive exit handlers. The OS releases these locks after the coverage
    // writer finishes, so each unlocked file witnesses the end of the process.
    static HELD_LOCKS: Mutex<Vec<File>> = Mutex::new(Vec::new());
    let lock = File::create(path)?;
    lock.lock()?;
    fs::write(format!("{path}.ready"), [])?;
    HELD_LOCKS.lock().expect("stub lock list").push(lock);
    Ok(())
}

/// Leave a live descendant holding the output pipes. The ready file ties the test observation
/// to this instance: its lock survives until this process exits, including death by signal.
fn grandchild(path: &str, announce: bool) -> io::Result<Command> {
    let mut copy = Command::new(std::env::current_exe()?);
    // Grandchildren are deliberately killed by signal and never exit cleanly; keep any
    // continuous-mode .profraw out of cargo-llvm-cov's target directory.
    copy.env("LLVM_PROFILE_FILE", format!("{path}.killed-%p.profraw"));
    #[cfg(unix)]
    copy.arg("ignore-term");
    if announce {
        copy.arg("out:grandchild ready");
    }
    copy.arg(format!("hold:{path}")).arg("sleep:60000");
    Ok(copy)
}

fn spawn_and_wait_ready(command: &mut Command, path: &str) -> io::Result<()> {
    drop(command.spawn()?);
    wait_for_file(&format!("{path}.ready"))
}

fn wait_for_file(path: &str) -> io::Result<()> {
    wait_until(|| Path::new(path).try_exists())
}

fn wait_until(mut ready: impl FnMut() -> io::Result<bool>) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready()? {
        (Instant::now() < deadline)
            .then_some(())
            .ok_or(io::ErrorKind::TimedOut)?;
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(unix)]
mod platform {
    #![allow(unsafe_code)]

    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    static RECEIVED_TERM: AtomicBool = AtomicBool::new(false);

    pub(super) fn close_output() -> io::Result<()> {
        let null = File::options().write(true).open("/dev/null")?;
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: this single-threaded stub owns its output descriptors. Replacing each
            // pipe with an open file keeps the descriptor valid for Rust's standard streams.
            let result = unsafe { libc::dup2(null.as_raw_fd(), fd) };
            (result >= 0)
                .then_some(())
                .ok_or_else(io::Error::last_os_error)?;
        }
        Ok(())
    }

    pub(super) fn ignore_term() {
        extern "C" fn received(_: libc::c_int) {
            RECEIVED_TERM.store(true, Ordering::Relaxed);
        }
        // SAFETY: the handler has the signal ABI and only writes a lock-free atomic flag.
        unsafe { libc::signal(libc::SIGTERM, received as *const () as libc::sighandler_t) };
    }

    pub(super) fn report_session() -> io::Result<()> {
        // SAFETY: both calls query the current process and take no pointers.
        let leader = unsafe { libc::getsid(0) == libc::getpid() };
        writeln!(io::stdout(), "session_leader={leader}")?;
        writeln!(io::stdout(), "terminal={}", File::open("/dev/tty").is_ok())
    }

    pub(super) fn await_term(ready: &str) -> io::Result<()> {
        fs::write(ready, [])?;
        super::wait_until(|| Ok(RECEIVED_TERM.load(Ordering::Relaxed)))
    }

    pub(super) fn cpu_time_ms() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: RUSAGE_SELF queries the current process and writes into valid storage.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed for the stub");
        // SAFETY: getrusage returned 0 and initialized `usage`.
        let usage = unsafe { usage.assume_init() };
        let to_ms = |tv: libc::timeval| {
            (tv.tv_sec.max(0) as u64)
                .saturating_mul(1000)
                .saturating_add((tv.tv_usec.max(0) as u64) / 1000)
        };
        to_ms(usage.ru_utime).saturating_add(to_ms(usage.ru_stime))
    }

    pub(super) fn replace_with_fifo(path: &std::path::Path) -> io::Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        fs::remove_file(path)?;
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: c_path is a valid NUL-terminated path created by this stub test.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        (rc == 0).then_some(()).ok_or_else(io::Error::last_os_error)
    }
}

#[cfg(windows)]
mod platform {
    #![allow(unsafe_code)]

    use std::io;
    use std::os::windows::process::CommandExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Console::{
        GetConsoleWindow, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandleEx,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, GetCurrentProcess, GetProcessTimes,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    pub(super) fn cpu_time_ms() -> u64 {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle and all four
        // out-pointers reference initialized local FILETIME structs.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &raw mut created,
                &raw mut exited,
                &raw mut kernel,
                &raw mut user,
            )
        };
        assert_ne!(ok, 0, "GetProcessTimes failed for the current stub process");
        let to_100ns =
            |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
        to_100ns(kernel).saturating_add(to_100ns(user)) / 10_000
    }

    pub(super) fn visible_console() -> bool {
        // SAFETY: GetConsoleWindow takes no arguments. IsWindowVisible accepts a window
        // handle and returns zero when there is no such window.
        unsafe { IsWindowVisible(GetConsoleWindow()) != 0 }
    }

    pub(super) fn close_output() -> io::Result<()> {
        for stream in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let mut previous = null_mut();
            // SAFETY: this single-threaded stub owns two distinct output pipes. The call
            // clears the selected standard handle and writes its previous value to our local.
            let replaced = unsafe { SetStdHandleEx(stream, null_mut(), &raw mut previous) };
            (replaced != 0)
                .then_some(())
                .ok_or_else(io::Error::last_os_error)?;
            // SAFETY: the handle was removed from the table above and is closed only here.
            let closed = unsafe { CloseHandle(previous) };
            (closed != 0)
                .then_some(())
                .ok_or_else(io::Error::last_os_error)?;
        }
        Ok(())
    }

    /// Ask Windows to create this grandchild outside the job. Report the OS reason for the
    /// refusal and retry the same grandchild in the job, where the test can witness its death.
    pub(super) fn breakaway(path: &str) -> io::Result<i32> {
        let mut copy = super::grandchild(path, true)?;
        let refusal = copy
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .expect_err("the job allowed a grandchild to escape");
        super::spawn_and_wait_ready(copy.creation_flags(0), path)?;
        Ok(refusal.raw_os_error().unwrap_or_default())
    }
}
