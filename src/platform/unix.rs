//! A session per child, with its leader's PID reserved until the final group signal,
//! and periodic process-group sampling for D18 accounting and optional memory thresholds.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::pin::pin;
use std::process::{Child as NativeChild, Command, ExitStatus};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdout};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;

use super::session;
use crate::mcp::Activity;
use crate::supervise::cleanup::{TemporaryFile, remove_files_blocking};
use crate::supervise::{Accounting, Ending, StartError, finish_reap};

const GRACE: Duration = Duration::from_secs(3);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(target_os = "macos")]
// Bound the SIGCHLD wait when XNU returns EPERM during leader teardown: long enough for the
// exiting leader to become observable to waitid, short enough to return real EPERM quickly.
const MACOS_EPERM_GRACE: Duration = Duration::from_millis(100);
const MAX_TRACKED_PIDS: usize = 4096;
static SPAWNING: Mutex<()> = Mutex::new(());

#[cfg(target_os = "linux")]
const METHOD: &str = "linux_proc_group_sample";
#[cfg(target_os = "macos")]
const METHOD: &str = "macos_proc_group_sample";

#[cfg(target_os = "linux")]
const CAVEATS: &[&str] = &[
    "Values come from periodic samples of the supervised process group; sampling can miss brief peaks, short-lived children, and descendants that leave the process group.",
    "At most 4096 members of the process group are tracked per sample.",
    "Summed RSS across group members double-counts pages shared between processes.",
    "A memory limit is a sampled threshold that triggers a D6 stop, not a hard allocation ceiling.",
];

#[cfg(target_os = "macos")]
const CAVEATS: &[&str] = &[
    "Values come from periodic samples of the supervised process group; sampling can miss brief peaks, short-lived children, and descendants that leave the process group.",
    "Only the first 4096 members of the process group are sampled.",
    "Summed RSS across group members double-counts pages shared between processes.",
    "A memory limit is a sampled threshold that triggers a D6 stop, not a hard allocation ceiling.",
];

pub fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and always succeeds.
    unsafe { libc::geteuid() }
}

pub fn empty_accounting(memory_limit_bytes: Option<u64>) -> Accounting {
    Accounting {
        method: METHOD,
        caveats: CAVEATS,
        memory_limit_bytes,
        peak_memory_bytes: None,
        cpu_time_ms: None,
        samples: Some(0),
        memory_limit_exceeded: false,
    }
}

struct UnixAccounting {
    memory_limit_bytes: Option<u64>,
    samples: u64,
    peak_memory_bytes: Option<u64>,
    exited_cpu_ms: u64,
    active_cpu_ms: HashMap<libc::pid_t, u64>,
    seen_cpu: bool,
    memory_limit_exceeded: bool,
}

impl UnixAccounting {
    fn new(memory_limit_bytes: Option<u64>) -> Self {
        Self {
            memory_limit_bytes,
            samples: 0,
            peak_memory_bytes: None,
            exited_cpu_ms: 0,
            active_cpu_ms: HashMap::new(),
            seen_cpu: false,
            memory_limit_exceeded: false,
        }
    }

    fn record(&mut self, rows: Vec<(libc::pid_t, u64, u64)>, check_limit: bool) -> bool {
        self.samples += 1;
        let mut rss = 0_u64;
        let mut next_active = HashMap::with_capacity(rows.len().min(MAX_TRACKED_PIDS));
        for &(pid, mem, cpu_ms) in rows.iter().take(MAX_TRACKED_PIDS) {
            rss = rss.saturating_add(mem);
            if let Some(previous) = self.active_cpu_ms.remove(&pid) {
                // If a PID was recycled between consecutive samples, fold the prior process's CPU.
                self.exited_cpu_ms = self
                    .exited_cpu_ms
                    .saturating_add(previous * u64::from(cpu_ms < previous));
            }
            next_active.insert(pid, cpu_ms);
        }
        for previous in self.active_cpu_ms.values() {
            self.exited_cpu_ms = self.exited_cpu_ms.saturating_add(*previous);
        }
        self.active_cpu_ms = next_active;
        self.seen_cpu |= !rows.is_empty();
        self.peak_memory_bytes = (!rows.is_empty())
            .then(|| self.peak_memory_bytes.unwrap_or(0).max(rss))
            .or(self.peak_memory_bytes);
        self.memory_limit_exceeded |=
            check_limit && self.memory_limit_bytes.is_some_and(|limit| rss > limit);
        self.memory_limit_exceeded
    }

    fn snapshot(&self) -> Accounting {
        let cpu_time_ms = self.seen_cpu.then(|| {
            self.active_cpu_ms
                .values()
                .fold(self.exited_cpu_ms, |acc, &ms| acc.saturating_add(ms))
        });
        Accounting {
            method: METHOD,
            caveats: CAVEATS,
            memory_limit_bytes: self.memory_limit_bytes,
            peak_memory_bytes: self.peak_memory_bytes,
            cpu_time_ms,
            samples: Some(self.samples),
            memory_limit_exceeded: self.memory_limit_exceeded,
        }
    }
}

struct Leader {
    process: NativeChild,
    temporary_files: Vec<TemporaryFile>,
}

#[cfg(feature = "test-tools")]
struct EpermInjection {
    deliver: bool,
    remaining: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "test-tools")]
impl EpermInjection {
    fn load() -> Option<Self> {
        let path = std::env::var_os("PROXENOS_TEST_KILL_GROUP_EPERM")?;
        let spec = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(path);
        let (mode, count) = spec.trim().split_once(':')?;
        let remaining = count.parse::<usize>().ok()?;
        Some(Self {
            deliver: mode == "deliver",
            remaining: std::sync::atomic::AtomicUsize::new(remaining),
        })
    }

    fn take(&self) -> bool {
        self.remaining
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |n| n.checked_sub(1),
            )
            .is_ok()
    }
}

pub struct Child {
    pid: libc::pid_t,
    changes: Signal,
    leader: Option<Leader>,
    reap: mpsc::Sender<Leader>,
    status: oneshot::Receiver<io::Result<ExitStatus>>,
    cleaned: oneshot::Receiver<()>,
    accounting: Arc<Mutex<UnixAccounting>>,
    #[cfg(feature = "test-tools")]
    eperm_injection: Option<EpermInjection>,
    pub stdin: Option<io::PipeWriter>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

pub fn spawn(
    mut command: Command,
    input: super::Input,
    files: &mut Vec<TemporaryFile>,
    memory_limit_bytes: Option<u64>,
    activity: Activity,
    before_spawn: impl FnOnce() -> Result<(), StartError>,
) -> Result<Child, StartError> {
    // Install the handler before spawn. An inherited SIG_IGN for SIGCHLD would otherwise
    // let the kernel reap a short-lived child before waitid could observe its exit.
    let changes = signal(SignalKind::child())?;
    // Some clients block SIGCHLD in every thread. Unmask it on the blocking launcher so its
    // reaper can receive exit notifications and its child can use SIGCHLD for descendants.
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
    // The reaper exists before the child does. Runtime shutdown can drop the supervisor,
    // and a denied stop can outlive the request that reports it. In either case this thread
    // can still wait for the leader and remove its files as long as the server is alive.
    let (reap, released) = mpsc::channel::<Leader>();
    let (report_status, status) = oneshot::channel();
    let (report_cleaned, cleaned) = oneshot::channel();
    let reap_when_released = move || {
        if let Ok(mut leader) = released.recv() {
            let status = leader.process.wait();
            let files_to_remove = status.is_ok().then_some(leader.temporary_files);
            let _ = report_status.send(status);
            remove_files_blocking(files_to_remove.unwrap_or_default());
        }
        let _ = report_cleaned.send(());
        drop(activity);
    };
    thread::Builder::new()
        .name("proxenos-reap".into())
        .spawn(reap_when_released)?;

    // Pipe creation and FD_CLOEXEC can be separate operations on macOS. Serialize both our
    // data pipes and std's exec-status pipe against every other fork in the supervisor.
    let spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    let stdin = input.attach(&mut command)?;
    let (out, stdout_writer) = io::pipe()?;
    let (err, stderr_writer) = io::pipe()?;
    let stdout = ChildStdout::from_std(OwnedFd::from(out).into())?;
    let stderr = ChildStderr::from_std(OwnedFd::from(err).into())?;
    command.stdout(stdout_writer).stderr(stderr_writer);
    // SAFETY: create only calls setsid and reads errno, without allocating or taking locks.
    unsafe { command.pre_exec(session::create) };
    before_spawn()?;
    // All pipe registration has already succeeded. From this point onward setup is infallible.
    let process = command.spawn()?;
    drop(spawning);
    let pid = process.id().cast_signed();
    Ok(Child {
        pid,
        changes,
        leader: Some(Leader {
            process,
            temporary_files: std::mem::take(files),
        }),
        reap,
        status,
        cleaned,
        accounting: Arc::new(Mutex::new(UnixAccounting::new(memory_limit_bytes))),
        #[cfg(feature = "test-tools")]
        eperm_injection: EpermInjection::load(),
        stdin,
        stdout: Some(stdout),
        stderr: Some(stderr),
    })
}

impl Child {
    async fn wait_for_leader_exit(&mut self) -> io::Result<()> {
        while !self.exited()? {
            self.changes.recv().await;
        }
        Ok(())
    }

    /// Observe exit without reaping so that the final group signal still targets our group,
    /// while periodically sampling the supervised process group for D18.
    pub async fn wait_for_exit(&mut self) -> io::Result<Ending> {
        let pid = self.pid;
        let accounting = Arc::clone(&self.accounting);
        let mut exit = pin!(self.wait_for_leader_exit());
        let mut monitor = pin!(async move {
            loop {
                if sample_once(pid, &accounting, true).await {
                    return Ending::MemoryLimit;
                }
                tokio::time::sleep(SAMPLE_INTERVAL).await;
            }
        });
        tokio::select! {
            biased;
            result = &mut exit => result.map(|()| Ending::Exited),
            ending = &mut monitor => Ok(ending),
        }
    }

    fn exited(&self) -> io::Result<bool> {
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid receives writable storage and a positive PID for our own child.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid.cast_unsigned(),
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

    fn kill_group(&self, signal: libc::c_int) -> (libc::c_int, io::Error) {
        #[cfg(feature = "test-tools")]
        if let Some(inject) = &self.eperm_injection
            && inject.take()
        {
            if inject.deliver {
                let _ = send_group_signal(self.pid, signal);
            }
            return (-1, io::Error::from_raw_os_error(libc::EPERM));
        }
        send_group_signal(self.pid, signal)
    }

    async fn signal(&mut self, signal: libc::c_int) -> io::Result<()> {
        let (result, error) = self.kill_group(signal);
        // XNU can return EPERM while the leader is still tearing down before waitid reports
        // the zombie, as well as once only the zombie remains. Wait briefly for SIGCHLD/waitid
        // while keeping the PID reserved; an unsignalable descendant beside that zombie looks
        // the same.
        #[cfg(target_os = "macos")]
        let eperm_exited = if result != 0 && error.raw_os_error() == Some(libc::EPERM) {
            if self.exited()? {
                true
            } else {
                #[cfg(feature = "test-tools")]
                if let Some(marker) = std::env::var_os("PROXENOS_TEST_EPERM_WAIT_MARKER") {
                    let _ = tokio::fs::write(marker, []).await;
                }
                // The outcome of the bounded wait is not needed: the exited() check below decides,
                // and if waitid failed during the wait, that check reports the error.
                let _ = tokio::time::timeout(MACOS_EPERM_GRACE, self.wait_for_leader_exit()).await;
                self.exited()?
            }
        } else {
            false
        };
        #[cfg(not(target_os = "macos"))]
        let eperm_exited = false;
        (result == 0 || error.raw_os_error() == Some(libc::ESRCH) || eperm_exited)
            .then_some(())
            .ok_or(error)
    }

    pub async fn stop(&mut self) -> io::Result<()> {
        self.signal(libc::SIGTERM).await?;
        // A descendant may need up to GRACE even after the leader exits, while a group whose
        // members have already exited should not pay the full 3 s wait. Because the zombie leader
        // keeps its PID reserved until the final signal and enumeration is not atomic, require two
        // consecutive confirmed-empty scans separated by a poll interval before ending early.
        let deadline = tokio::time::Instant::now() + GRACE;
        let mut empty_scans = 0_u8;
        loop {
            let empty = if self.exited()? {
                let pid = self.pid;
                tokio::task::spawn_blocking(move || group_confirmed_empty(pid))
                    .await
                    .expect("group check finished")
            } else {
                false
            };
            empty_scans = if empty { empty_scans + 1 } else { 0 };
            if empty_scans >= 2 {
                break;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(remaining.min(SAMPLE_INTERVAL)).await;
        }
        self.signal(libc::SIGKILL).await
    }

    fn release(&mut self) {
        // Moving the sole native Child disarms the drop guard before wait can recycle its PID.
        let leader = self.leader.take().expect("the group still owns its leader");
        self.reap
            .send(leader)
            .expect("reaper waits for this leader");
    }

    async fn reap(&mut self) -> io::Result<ExitStatus> {
        self.signal(libc::SIGKILL).await?;
        self.release();
        finish_reap(&mut self.status, &mut self.cleaned).await
    }

    pub async fn finish(mut self) -> (io::Result<ExitStatus>, Accounting) {
        let _ = sample_once(self.pid, &self.accounting, false).await;
        let status = self.reap().await;
        let accounting = self
            .accounting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        (status, accounting)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.leader.is_some() {
            let _ = self.kill_group(libc::SIGKILL);
            self.release();
        }
    }
}

fn send_group_signal(pid: libc::pid_t, signal: libc::c_int) -> (libc::c_int, io::Error) {
    // SAFETY: until the leader is handed to the reaper its PID reserves the group ID.
    let result = unsafe { libc::kill(-pid, signal) };
    (result, io::Error::last_os_error())
}

pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}

async fn sample_once(
    pid: libc::pid_t,
    state: &Arc<Mutex<UnixAccounting>>,
    check_limit: bool,
) -> bool {
    let rows = tokio::task::spawn_blocking(move || sample_pgrp(pid))
        .await
        .expect("accounting sampler finished");
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(rows, check_limit)
}

#[cfg(target_os = "linux")]
struct LinuxStat<'a> {
    is_zombie: bool,
    pgrp: libc::pid_t,
    rest: std::str::SplitAsciiWhitespace<'a>,
}

#[cfg(target_os = "linux")]
fn parse_linux_header(stat: &str) -> Option<LinuxStat<'_>> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    let mut fields = after_comm.split_ascii_whitespace();
    let is_zombie = matches!(fields.next()?, "Z" | "X" | "x");
    let pgrp: libc::pid_t = fields.nth(1)?.parse().ok()?;
    Some(LinuxStat {
        is_zombie,
        pgrp,
        rest: fields,
    })
}

#[cfg(target_os = "linux")]
fn parse_linux_accounting(mut header: LinuxStat<'_>) -> Option<(u64, u64)> {
    let utime: u64 = header.rest.nth(8)?.parse().ok()?;
    let stime: u64 = header.rest.next()?.parse().ok()?;
    let rss_pages: u64 = header.rest.nth(8)?.parse().ok()?;
    Some((rss_pages, utime.saturating_add(stime)))
}

#[cfg(target_os = "linux")]
fn sample_pgrp(pgrp: libc::pid_t) -> Vec<(libc::pid_t, u64, u64)> {
    // SAFETY: sysconf takes only a POSIX constant and has no memory safety requirements.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    // SAFETY: sysconf takes only a POSIX constant and has no memory safety requirements.
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse::<libc::pid_t>().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let header = parse_linux_header(&stat)?;
            (header.pgrp == pgrp)
                .then(|| parse_linux_accounting(header))
                .flatten()
                .map(|(rss_pages, ticks)| {
                    (
                        pid,
                        rss_pages.saturating_mul(page_size),
                        ticks.saturating_mul(1000) / ticks_per_sec,
                    )
                })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn scan_linux_proc_entry(
    entry: io::Result<std::fs::DirEntry>,
    pgrp: libc::pid_t,
) -> Result<bool, ()> {
    let entry = entry.ok().ok_or(())?;
    let Some(pid) = entry
        .file_name()
        .to_str()
        .and_then(|name| name.parse::<libc::pid_t>().ok())
    else {
        return Ok(false);
    };
    let read = std::fs::read_to_string(entry.path().join("stat"));
    // Fail safe on any stat read error other than NotFound (a process that vanished).
    let vanished = read.as_ref().err().map(io::Error::kind) == Some(io::ErrorKind::NotFound);
    let stat = read.ok();
    (stat.is_some() || vanished).then_some(()).ok_or(())?;
    // Fail safe if an existing stat file cannot be parsed for state and pgrp.
    let parsed = stat
        .as_deref()
        .map(|text| parse_linux_header(text).ok_or(()));
    let header = parsed.transpose()?;
    let member = header.filter(|h| h.pgrp == pgrp);
    let is_live = member.as_ref().is_some_and(|h| !h.is_zombie);
    (!is_live).then_some(()).ok_or(())?;
    Ok(member.is_some() && pid == pgrp)
}

#[cfg(target_os = "linux")]
fn group_confirmed_empty(pgrp: libc::pid_t) -> bool {
    // Positive proof only: /proc scan completed without errors, no non-zombie member matched
    // pgrp, and the unreaped zombie leader itself was observed in this scan.
    std::fs::read_dir("/proc")
        .ok()
        .and_then(|mut entries| {
            entries
                .try_fold(false, |seen_leader, entry| {
                    scan_linux_proc_entry(entry, pgrp).map(|is_leader| seen_leader || is_leader)
                })
                .ok()
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

/// How many members of a process group are sampled on macOS.
#[cfg(target_os = "macos")]
const PGRP_SAMPLE_PIDS: usize = 4096;

#[cfg(target_os = "macos")]
fn pgrp_pids(pgrp: libc::pid_t) -> (Vec<libc::pid_t>, bool) {
    // A fixed buffer: libproc's size query with a null buffer does not report the size of this
    // group. A group of more than PGRP_SAMPLE_PIDS processes is sampled only in part.
    let mut pids = vec![0 as libc::pid_t; PGRP_SAMPLE_PIDS];
    let buf_bytes = (pids.len() * size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: pids is writable storage of exactly buf_bytes bytes.
    let filled = unsafe { libc::proc_listpgrppids(pgrp, pids.as_mut_ptr().cast(), buf_bytes) };
    // libproc converts syscall failure into 0 with errno set, and returns a count of PIDs (measured
    // on macOS 26). A full buffer may be truncated; a 0 return fails the leader-presence check.
    let count = (filled as usize).min(pids.len());
    let complete = count < pids.len();
    pids.truncate(count);
    (pids, complete)
}

#[cfg(target_os = "macos")]
fn macos_pid_is_zombie(pid: libc::pid_t) -> bool {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // Pass arg=1 so XNU looks up zombies via proc_find_zombref(). Never treat ESRCH (or any other
    // lookup failure) as proof a member is gone: a member may have forked before exiting.
    // SAFETY: info points to valid writable storage of `size` bytes for PROC_PIDTBSDINFO.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: when got == size, proc_pidinfo wrote a complete proc_bsdinfo struct.
    got == size && unsafe { info.assume_init() }.pbi_status == libc::SZOMB
}

#[cfg(target_os = "macos")]
fn group_confirmed_empty(pgrp: libc::pid_t) -> bool {
    let (pids, complete) = pgrp_pids(pgrp);
    // Positive proof only: the buffer was not full (no truncation), the unreaped zombie leader
    // itself is present (ruling out a 0-return failure), and every listed member is confirmed to
    // be a zombie.
    complete && pids.contains(&pgrp) && pids.into_iter().all(macos_pid_is_zombie)
}

#[cfg(target_os = "macos")]
fn sample_pgrp(pgrp: libc::pid_t) -> Vec<(libc::pid_t, u64, u64)> {
    let mut timebase = MachTimebaseInfo { numer: 1, denom: 1 };
    // SAFETY: timebase points to valid writable storage matching mach_timebase_info_data_t.
    let _ = unsafe { mach_timebase_info(&raw mut timebase) };
    let numer = u128::from(timebase.numer.max(1));
    let denom = u128::from(timebase.denom.max(1));
    let (pids, _) = pgrp_pids(pgrp);
    pids.into_iter()
        .filter_map(|pid| {
            let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
            let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
            // SAFETY: info points to valid writable storage of `size` bytes for PROC_PIDTASKINFO.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTASKINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            (got == size).then(|| {
                // SAFETY: proc_pidinfo wrote a complete proc_taskinfo struct.
                let info = unsafe { info.assume_init() };
                let cpu_ticks = u128::from(info.pti_total_user) + u128::from(info.pti_total_system);
                let cpu_ms = ((cpu_ticks * numer / denom) / 1_000_000) as u64;
                (pid, info.pti_resident_size, cpu_ms)
            })
        })
        .collect()
}
