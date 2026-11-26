//! A session per child, with its leader's PID reserved until the final group signal,
//! and periodic process-group sampling for D18 accounting and optional memory thresholds.
#![allow(unsafe_code)]

use std::collections::{BTreeSet, HashMap};
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
const MAX_TRACKED_PIDS: usize = 4096;
static SPAWNING: Mutex<()> = Mutex::new(());

#[cfg(target_os = "linux")]
const METHOD: &str = "linux_proc_group_sample";
#[cfg(target_os = "macos")]
const METHOD: &str = "macos_proc_group_sample";

#[cfg(target_os = "linux")]
const CAVEATS: &[&str] = &[
    "Values come from periodic samples of the supervised process group; sampling can miss brief peaks, short-lived children, and descendants that leave the process group.",
    "At most 4096 members of the process group are tracked per sample and in retained per-PID CPU state; at capacity the stalest entry is folded and evicted, so an evicted live PID that reappears can double-count CPU time. PID reuse within the same start tick can undercount CPU time.",
    "Summed RSS across group members double-counts pages shared between processes.",
    "A memory limit is a sampled threshold that triggers a D6 stop, not a hard allocation ceiling.",
];

#[cfg(target_os = "macos")]
const CAVEATS: &[&str] = &[
    "Values come from periodic samples of the supervised process group; sampling can miss brief peaks, short-lived children, and descendants that leave the process group.",
    "Only the first 4096 members of the process group are sampled, and retained per-PID CPU state is bounded at 4096 entries; at capacity the stalest entry is folded and evicted, so an evicted live PID that reappears can double-count CPU time. PID reuse with an identical start timestamp can undercount CPU time.",
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity(u64, u64);

#[derive(Clone, Copy)]
struct SampleRow {
    pid: libc::pid_t,
    identity: ProcessIdentity,
    memory_bytes: u64,
    cpu_ms: u64,
}

#[derive(Clone, Copy)]
struct TrackedProcess {
    identity: ProcessIdentity,
    cpu_ms: u64,
    last_seen_sample: u64,
}

struct UnixAccounting {
    memory_limit_bytes: Option<u64>,
    samples: u64,
    peak_memory_bytes: Option<u64>,
    exited_cpu_ms: u64,
    tracked: HashMap<libc::pid_t, TrackedProcess>,
    eviction_order: BTreeSet<(u64, libc::pid_t)>,
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
            tracked: HashMap::new(),
            eviction_order: BTreeSet::new(),
            seen_cpu: false,
            memory_limit_exceeded: false,
        }
    }

    fn update_tracked(&mut self, row: SampleRow) -> bool {
        let Some(existing) = self.tracked.get_mut(&row.pid) else {
            return false;
        };
        self.eviction_order
            .remove(&(existing.last_seen_sample, row.pid));
        if existing.identity != row.identity {
            self.exited_cpu_ms = self.exited_cpu_ms.saturating_add(existing.cpu_ms);
            existing.identity = row.identity;
            existing.cpu_ms = row.cpu_ms;
        } else {
            existing.cpu_ms = existing.cpu_ms.max(row.cpu_ms);
        }
        existing.last_seen_sample = self.samples;
        self.eviction_order.insert((self.samples, row.pid));
        true
    }

    fn insert_tracked(&mut self, row: SampleRow) {
        if !self.update_tracked(row) {
            if self.tracked.len() >= MAX_TRACKED_PIDS
                && let Some((_, victim_pid)) = self.eviction_order.pop_first()
                && let Some(victim) = self.tracked.remove(&victim_pid)
            {
                self.exited_cpu_ms = self.exited_cpu_ms.saturating_add(victim.cpu_ms);
            }
            self.eviction_order.insert((self.samples, row.pid));
            self.tracked.insert(
                row.pid,
                TrackedProcess {
                    identity: row.identity,
                    cpu_ms: row.cpu_ms,
                    last_seen_sample: self.samples,
                },
            );
        }
    }

    fn record(&mut self, rows: &[SampleRow], check_limit: bool) -> bool {
        self.samples += 1;
        let sample = &rows[..rows.len().min(MAX_TRACKED_PIDS)];
        let mut rss = 0_u64;
        let mut pending = Vec::new();
        for &row in sample {
            rss = rss.saturating_add(row.memory_bytes);
            if !self.update_tracked(row) {
                pending.push(row);
            }
        }
        for row in pending {
            self.insert_tracked(row);
        }
        self.seen_cpu |= !sample.is_empty();
        self.peak_memory_bytes = (!sample.is_empty())
            .then(|| self.peak_memory_bytes.unwrap_or(0).max(rss))
            .or(self.peak_memory_bytes);
        self.memory_limit_exceeded |=
            check_limit && self.memory_limit_bytes.is_some_and(|limit| rss > limit);
        self.memory_limit_exceeded
    }

    fn snapshot(&self) -> Accounting {
        let cpu_time_ms = self.seen_cpu.then(|| {
            self.tracked
                .values()
                .fold(self.exited_cpu_ms, |acc, p| acc.saturating_add(p.cpu_ms))
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

pub struct Child {
    pid: libc::pid_t,
    changes: Signal,
    leader: Option<Leader>,
    reap: mpsc::Sender<Leader>,
    status: oneshot::Receiver<io::Result<ExitStatus>>,
    cleaned: oneshot::Receiver<()>,
    accounting: Arc<Mutex<UnixAccounting>>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

pub fn spawn(
    mut command: Command,
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
    // output pipes and std's exec-status pipe against every other fork in the supervisor.
    let spawning = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
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
        stdout: Some(stdout),
        stderr: Some(stderr),
    })
}

impl Child {
    /// Observe exit without reaping so that the final group signal still targets our group,
    /// while periodically sampling the supervised process group for D18.
    pub async fn wait_for_exit(&mut self) -> io::Result<Ending> {
        let pid = self.pid;
        let accounting = Arc::clone(&self.accounting);
        let mut exit = pin!(async {
            while !self.exited()? {
                self.changes.recv().await;
            }
            Ok::<Ending, io::Error>(Ending::Exited)
        });
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
            result = &mut exit => result,
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

    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: until the leader is handed to the reaper its PID reserves the group ID.
        let result = unsafe { libc::kill(-self.pid, signal) };
        let error = io::Error::last_os_error();
        // XNU returns EPERM when only the zombie remains. Keep the PID reserved while
        // checking the leader. An unsignalable descendant beside that zombie looks the same.
        #[cfg(target_os = "macos")]
        if result != 0 && error.raw_os_error() == Some(libc::EPERM) && self.exited()? {
            return Ok(());
        }
        (result == 0 || error.raw_os_error() == Some(libc::ESRCH))
            .then_some(())
            .ok_or(error)
    }

    pub async fn stop(&self) -> io::Result<()> {
        self.signal(libc::SIGTERM)?;
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
        self.signal(libc::SIGKILL)
    }

    fn release(&mut self) {
        // Moving the sole native Child disarms the drop guard before wait can recycle its PID.
        let leader = self.leader.take().expect("the group still owns its leader");
        self.reap
            .send(leader)
            .expect("reaper waits for this leader");
    }

    async fn reap(&mut self) -> io::Result<ExitStatus> {
        self.signal(libc::SIGKILL)?;
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
            let _ = self.signal(libc::SIGKILL);
            self.release();
        }
    }
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
        .record(&rows, check_limit)
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
fn parse_linux_accounting(mut header: LinuxStat<'_>) -> Option<(u64, u64, u64)> {
    let utime: u64 = header.rest.nth(8)?.parse().ok()?;
    let stime: u64 = header.rest.next()?.parse().ok()?;
    let starttime: u64 = header.rest.nth(6)?.parse().ok()?;
    let rss_pages: u64 = header.rest.nth(1)?.parse().ok()?;
    Some((rss_pages, utime.saturating_add(stime), starttime))
}

#[cfg(target_os = "linux")]
fn sample_pgrp(pgrp: libc::pid_t) -> Vec<SampleRow> {
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
                .map(|(rss_pages, ticks, starttime)| SampleRow {
                    pid,
                    identity: ProcessIdentity(starttime, 0),
                    memory_bytes: rss_pages.saturating_mul(page_size),
                    cpu_ms: ticks.saturating_mul(1000) / ticks_per_sec,
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
fn sample_pgrp(pgrp: libc::pid_t) -> Vec<SampleRow> {
    let mut timebase = MachTimebaseInfo { numer: 1, denom: 1 };
    // SAFETY: timebase points to valid writable storage matching mach_timebase_info_data_t.
    let _ = unsafe { mach_timebase_info(&raw mut timebase) };
    let numer = u128::from(timebase.numer.max(1));
    let denom = u128::from(timebase.denom.max(1));
    let (pids, _) = pgrp_pids(pgrp);
    pids.into_iter()
        .filter_map(|pid| {
            let mut info = MaybeUninit::<libc::proc_taskallinfo>::zeroed();
            let size = size_of::<libc::proc_taskallinfo>() as libc::c_int;
            // SAFETY: info is writable storage of `size` bytes for PROC_PIDTASKALLINFO.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTASKALLINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            (got == size).then(|| {
                // SAFETY: proc_pidinfo wrote a complete proc_taskallinfo struct.
                let info = unsafe { info.assume_init() };
                let start = info.pbsd;
                let task = info.ptinfo;
                let cpu_ticks = u128::from(task.pti_total_user) + u128::from(task.pti_total_system);
                let cpu_ms = ((cpu_ticks * numer / denom) / 1_000_000) as u64;
                SampleRow {
                    pid,
                    identity: ProcessIdentity(start.pbi_start_tvsec, start.pbi_start_tvusec),
                    memory_bytes: task.pti_resident_size,
                    cpu_ms,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: libc::pid_t, start: u64, memory_bytes: u64, cpu_ms: u64) -> SampleRow {
        SampleRow {
            pid,
            identity: ProcessIdentity(start, 0),
            memory_bytes,
            cpu_ms,
        }
    }

    fn rows_range(pids: std::ops::RangeInclusive<libc::pid_t>) -> Vec<SampleRow> {
        pids.map(|pid| row(pid, 1, 100, 10)).collect()
    }

    #[test]
    fn omission_preserves_previous_cpu_without_double_counting() {
        let mut accounting = UnixAccounting::new(None);

        assert!(!accounting.record(&[], false));
        let initial = accounting.snapshot();
        assert_eq!(initial.samples, Some(1));
        assert_eq!(initial.cpu_time_ms, None);
        assert_eq!(initial.peak_memory_bytes, None);

        assert!(!accounting.record(&[row(100, 1, 4096, 100)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(100));
        assert_eq!(accounting.snapshot().peak_memory_bytes, Some(4096));

        // Temporary omissions keep the retained entry and must not fold it into exited_cpu_ms.
        assert!(!accounting.record(&[], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(100));
        assert_eq!(accounting.snapshot().peak_memory_bytes, Some(4096));

        assert!(!accounting.record(&[], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(100));

        // Reappearing with the same identity updates to 120 ms instead of 220 ms.
        assert!(!accounting.record(&[row(100, 1, 8192, 120)], false));
        let after = accounting.snapshot();
        assert_eq!(after.samples, Some(5));
        assert_eq!(after.cpu_time_ms, Some(120));
        assert_eq!(after.peak_memory_bytes, Some(8192));

        // Minor non-monotonic counter jitter with the same identity neither drops nor folds.
        assert!(!accounting.record(&[row(100, 1, 8192, 115)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(120));
        assert!(!accounting.record(&[row(100, 1, 8192, 125)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(125));

        // A duplicate PID within a single sample updates without creating a second tracked slot.
        let dup = [row(101, 2, 512, 10), row(101, 2, 512, 25)];
        assert!(!accounting.record(&dup, false));
        assert_eq!(accounting.tracked.len(), 2);
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(150));
    }

    #[test]
    fn same_pid_new_identity_folds_prior_cpu_for_higher_equal_and_lower_counters() {
        let mut accounting = UnixAccounting::new(None);

        assert!(!accounting.record(&[row(200, 10, 1024, 100)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(100));

        // Reused PID with a higher counter still folds the prior process's 100 ms.
        assert!(!accounting.record(&[row(200, 20, 2048, 150)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(250));

        // Reused PID after an omission with a lower counter folds 150 ms and tracks 40 ms.
        assert!(!accounting.record(&[], false));
        assert!(!accounting.record(&[row(200, 30, 1024, 40)], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(290));

        // Reused PID differing only in sub-second start identity with an equal counter folds 40 ms.
        let reused_subsec = SampleRow {
            pid: 200,
            identity: ProcessIdentity(30, 1),
            memory_bytes: 512,
            cpu_ms: 40,
        };
        assert!(!accounting.record(&[reused_subsec], false));
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(330));
        assert_eq!(accounting.tracked.len(), 1);
    }

    #[test]
    fn capacity_bounds_state_and_evicts_stalest_unseen_pid() {
        let limit = (MAX_TRACKED_PIDS as u64) * 100 - 1;
        let mut accounting = UnixAccounting::new(Some(limit));
        let max_pid = MAX_TRACKED_PIDS as libc::pid_t;

        let initial = rows_range(1..=max_pid);
        assert!(!accounting.record(&initial, false));
        assert_eq!(accounting.tracked.len(), MAX_TRACKED_PIDS);
        assert_eq!(accounting.eviction_order.len(), MAX_TRACKED_PIDS);
        let expected = (MAX_TRACKED_PIDS as u64) * 10;
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(expected));

        // Refresh PID 1 and PIDs 3..=4096 while omitting PID 2, making PID 2 the stalest entry.
        let mut refresh_except_2 = vec![row(1, 1, 100, 10)];
        refresh_except_2.extend(rows_range(3..=max_pid));
        assert!(!accounting.record(&refresh_except_2, false));
        assert_eq!(accounting.tracked.len(), MAX_TRACKED_PIDS);

        // Put a brand-new PID (4097) first, plus a 4098th row beyond MAX_TRACKED_PIDS.
        // Two-pass recording updates all currently present PIDs first so PID 2 (absent) is
        // evicted rather than PID 1, and RSS remains capped at MAX_TRACKED_PIDS rows.
        let added_pid = max_pid + 1;
        let dropped_pid = max_pid + 2;
        let mut with_new = vec![row(added_pid, 1, 100, 15), row(1, 1, 100, 10)];
        with_new.extend(rows_range(3..=max_pid));
        with_new.push(row(dropped_pid, 1, 100, 999));
        assert!(accounting.record(&with_new, true));
        assert_eq!(accounting.tracked.len(), MAX_TRACKED_PIDS);
        assert_eq!(accounting.eviction_order.len(), MAX_TRACKED_PIDS);
        assert!(!accounting.tracked.contains_key(&2));
        assert!(accounting.tracked.contains_key(&1));
        assert!(accounting.tracked.contains_key(&added_pid));
        assert!(!accounting.tracked.contains_key(&dropped_pid));
        let expected_evicted = expected + 15;
        let snapshot = accounting.snapshot();
        assert_eq!(snapshot.cpu_time_ms, Some(expected_evicted));
        assert_eq!(snapshot.peak_memory_bytes, Some(limit + 1));
        assert!(snapshot.memory_limit_exceeded);

        // Honest limitation at capacity: if evicted live PID 2 later reappears while PID 4097 is
        // omitted and evicted, PID 2's earlier 10 ms remains folded and is counted again.
        let mut reappear_2 = vec![row(2, 1, 100, 12), row(1, 1, 100, 10)];
        reappear_2.extend(rows_range(3..=max_pid));
        assert!(accounting.record(&reappear_2, true));
        assert_eq!(accounting.tracked.len(), MAX_TRACKED_PIDS);
        let reappeared = expected_evicted + 12;
        assert_eq!(accounting.snapshot().cpu_time_ms, Some(reappeared));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_extracts_pgrp_cpu_starttime_and_rss() {
        let stat = "4242 (stub ) worker) S 4200 4242 4242 0 -1 4194304 10 0 0 0 125 35 0 0 20 0 1 0 987654321 1048576 256 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        let header = parse_linux_header(stat).unwrap();
        assert!(!header.is_zombie);
        assert_eq!(header.pgrp, 4242);
        let (rss_pages, ticks, starttime) = parse_linux_accounting(header).unwrap();
        assert_eq!(ticks, 160);
        assert_eq!(starttime, 987_654_321);
        assert_eq!(rss_pages, 256);
    }
}
