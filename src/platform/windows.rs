//! Start suspended and resume the primary thread only after joining a non-breakaway job.
//! Report the D6 job's totals and apply optional job memory limits for D18.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::{ChildExt, CommandExt};
use std::process::{Child as StdChild, Command, ExitStatus};
use std::ptr::{NonNull, null, null_mut};

use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT;
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED, ResumeThread};

use crate::mcp::Activity;
use crate::supervise::cleanup::{self, TemporaryFile};
use crate::supervise::{Accounting, StartError, finish_reap};

const METHOD: &str = "windows_job_totals";
const CAVEATS: &[&str] = &[
    "Peak memory is the job's peak committed memory across all processes assigned to the job.",
    "A memory limit asks Windows to refuse allocations above the job limit; the CLI may fail on allocation failure or carry on.",
    "Job IOCP memory-limit notifications are best effort and may be missed under memory pressure, in which case a memory-limit failure may surface as abnormal_exit.",
];

pub fn empty_accounting(memory_limit_bytes: Option<u64>) -> Accounting {
    Accounting {
        method: METHOD,
        caveats: CAVEATS,
        memory_limit_bytes,
        peak_memory_bytes: None,
        cpu_time_ms: None,
        samples: None,
        memory_limit_exceeded: false,
    }
}

struct Group {
    job: OwnedHandle,
    completion: Option<OwnedHandle>,
    memory_limit_bytes: Option<u64>,
}

impl Group {
    fn new(memory_limit_bytes: Option<u64>) -> io::Result<Self> {
        // SAFETY: null security attributes create a non-inheritable handle; null name is valid.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        let raw = NonNull::new(raw).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: creation succeeded and transfers ownership of this new job handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw.as_ptr()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let mut completion = None;
        if let Some(limit) = memory_limit_bytes {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = usize::try_from(limit).unwrap_or(usize::MAX);
            // SAFETY: INVALID_HANDLE_VALUE creates a standalone completion port with no existing port.
            let raw_port =
                unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
            let raw_port = NonNull::new(raw_port).ok_or_else(io::Error::last_os_error)?;
            // SAFETY: creation succeeded and transfers ownership of this new completion port handle.
            let port = unsafe { OwnedHandle::from_raw_handle(raw_port.as_ptr()) };
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: null_mut(),
                CompletionPort: port.as_raw_handle(),
            };
            // SAFETY: job and completion port handles are open and association is live.
            let associated = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectAssociateCompletionPortInformation,
                    (&raw const association).cast(),
                    size_of_val(&association) as u32,
                )
            };
            (associated != 0)
                .then_some(())
                .ok_or_else(io::Error::last_os_error)?;
            completion = Some(port);
        }
        // SAFETY: this job handle is open and the pointer and size describe the live structure.
        let rc = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of_val(&limits) as u32,
            )
        };
        (rc != 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)?;
        Ok(Self {
            job,
            completion,
            memory_limit_bytes,
        })
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: this is our live job handle, with the right to terminate its members.
        let rc = unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
        (rc != 0).then_some(()).ok_or_else(io::Error::last_os_error)
    }

    fn accounting(&self) -> Accounting {
        let mut basic = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: this job handle is open and basic points to valid writable storage.
        let has_basic = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&raw mut basic).cast(),
                size_of_val(&basic) as u32,
                null_mut(),
            )
        } != 0;
        let cpu_time_ms = has_basic.then(|| {
            let total_100ns = (basic.TotalUserTime.max(0) as u64)
                .saturating_add(basic.TotalKernelTime.max(0) as u64);
            total_100ns / 10_000
        });

        let mut extended = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: this job handle is open and extended points to valid writable storage.
        let has_extended = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut extended).cast(),
                size_of_val(&extended) as u32,
                null_mut(),
            )
        } != 0;
        let peak_memory_bytes = has_extended.then_some(extended.PeakJobMemoryUsed as u64);

        let mut memory_limit_exceeded = false;
        if let Some(port) = &self.completion {
            let mut message = 0_u32;
            let mut key = 0_usize;
            let mut overlapped = null_mut();
            // SAFETY: port is our live completion port and out-pointers point to valid locals.
            while unsafe {
                GetQueuedCompletionStatus(
                    port.as_raw_handle(),
                    &raw mut message,
                    &raw mut key,
                    &raw mut overlapped,
                    0,
                )
            } != 0
            {
                if message == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                    memory_limit_exceeded = true;
                    break;
                }
            }
        }

        Accounting {
            method: METHOD,
            caveats: CAVEATS,
            memory_limit_bytes: self.memory_limit_bytes,
            peak_memory_bytes,
            cpu_time_ms,
            samples: None,
            memory_limit_exceeded,
        }
    }
}

/// Owns the process from the instant it is created. A failure to assign or resume it must also
/// terminate and wait for it, even when the job never owned it. Both operations on an exited
/// Windows process use its original handle and are idempotent.
struct Process(StdChild);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub struct Child {
    // Fields drop in declaration order. Close the job before releasing the file cleanup worker,
    // including when the supervisor's future is dropped.
    job: Group,
    release: oneshot::Sender<()>,
    exited: oneshot::Receiver<()>,
    status: oneshot::Receiver<io::Result<ExitStatus>>,
    cleaned: oneshot::Receiver<()>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    pub async fn wait_for_exit(&mut self) -> io::Result<()> {
        (&mut self.exited).await.map_err(io::Error::other)
    }

    pub async fn stop(&self) -> io::Result<()> {
        self.job.terminate()
    }

    pub async fn finish(mut self) -> (io::Result<ExitStatus>, Accounting) {
        let accounting = self.job.accounting();
        drop(self.job);
        drop(self.release);
        let status = finish_reap(&mut self.status, &mut self.cleaned).await;
        (status, accounting)
    }
}

pub fn spawn(
    mut command: Command,
    files: &mut Vec<TemporaryFile>,
    memory_limit_bytes: Option<u64>,
    activity: Activity,
    check_start: impl Fn() -> Result<(), StartError>,
) -> Result<Child, StartError> {
    let job = Group::new(memory_limit_bytes)?;
    // Rust exposes the primary thread handle on std's Child. Keep it through job assignment.
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    check_start()?;
    let mut process = Process(command.spawn()?);
    // SAFETY: the job and suspended process handles are open with assignment rights.
    let rc =
        unsafe { AssignProcessToJobObject(job.job.as_raw_handle(), process.0.as_raw_handle()) };
    (rc != 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)?;
    let stdout = process
        .0
        .stdout
        .take()
        .map(ChildStdout::from_std)
        .transpose()?;
    let stderr = process
        .0
        .stderr
        .take()
        .map(ChildStderr::from_std)
        .transpose()?;
    // Creation and pipe setup can outlast the deadline or overlap cancellation. A failed
    // check leaves the process guard to kill and reap the child while it is still suspended.
    check_start()?;
    // SAFETY: this is the primary thread of our suspended child, borrowed from its owner.
    let count = unsafe { ResumeThread(process.0.main_thread_handle().as_raw_handle()) };
    (count != u32::MAX)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)?;
    // Only one blocking wait owns the process. The notification can be cancelled without
    // consuming the status the supervisor collects after it has finished with the pipes.
    let (done, exited) = oneshot::channel();
    let (report_status, status) = oneshot::channel();
    let (report_cleaned, cleaned) = oneshot::channel();
    let (release, released) = oneshot::channel::<()>();
    let files = std::mem::take(files);
    tokio::task::spawn_blocking(move || {
        let status = process.0.wait();
        drop(process);
        let files_to_remove = status.is_ok().then_some(files);
        let _ = report_status.send(status);
        let _ = done.send(());
        let _ = released.blocking_recv();
        cleanup::remove_files_blocking(files_to_remove.unwrap_or_default());
        let _ = report_cleaned.send(());
        drop(activity);
    });
    Ok(Child {
        job,
        release,
        exited,
        status,
        cleaned,
        stdout,
        stderr,
    })
}

pub fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}
