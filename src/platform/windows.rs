//! Windows: each child starts suspended, joins a job object, and only then runs. The job ends
//! every process in it when Proxenos closes it, or when Proxenos dies.
#![allow(unsafe_code)]

use std::io;
use std::mem::{size_of, zeroed};
use std::process::ExitStatus;
use std::ptr::null;

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};
use windows_sys::core::BOOL;

/// A handle that is closed when dropped.
struct Handle(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Handle {}
// SAFETY: as above; the handles here are only passed to thread-safe kernel calls.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn check(ok: BOOL) -> io::Result<()> {
    (ok != 0).then_some(()).ok_or_else(io::Error::last_os_error)
}

fn owned(handle: HANDLE) -> io::Result<Handle> {
    (!handle.is_null() && handle != INVALID_HANDLE_VALUE)
        .then_some(Handle(handle))
        .ok_or_else(io::Error::last_os_error)
}

/// The child gets no console window, and is created suspended so that it is in its job before
/// it runs any code.
pub fn prepare(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

/// The job object of one child.
pub struct Group {
    job: Handle,
}

impl Group {
    /// Puts the suspended child into a new job, then lets it run.
    pub fn attach(child: &Child) -> io::Result<Self> {
        // SAFETY: no security attributes and no name are valid arguments.
        let job = owned(unsafe { CreateJobObjectW(null(), null()) })?;
        // SAFETY: the structure is plain data, for which all zeroes is a valid value.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let length = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(io::Error::other)?;
        // SAFETY: the pointer and length describe `limits`, which outlives the call.
        check(unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                length,
            )
        })?;
        let process = child.raw_handle().ok_or_else(|| io::Error::other("the child has exited"))?;
        // SAFETY: both handles are open; the process handle is owned by `child`.
        check(unsafe { AssignProcessToJobObject(job.0, process.cast()) })?;
        let pid = child.id().ok_or_else(|| io::Error::other("the child has exited"))?;
        resume(pid)?;
        Ok(Self { job })
    }

    /// There is no polite stop on Windows: the job is terminated.
    pub async fn stop(&self, child: &mut Child) -> io::Result<ExitStatus> {
        self.kill();
        child.wait().await
    }

    /// Ends every process in the job.
    pub fn kill(&self) {
        // SAFETY: the job handle is open for as long as `self` lives.
        let _ = unsafe { TerminateJobObject(self.job.0, 1) };
    }
}

/// Resumes the threads of a process created suspended.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: a thread snapshot takes no process id.
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    // SAFETY: plain data, for which all zeroes is a valid value.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = u32::try_from(size_of::<THREADENTRY32>()).map_err(io::Error::other)?;
    let mut resumed = 0_u32;
    // SAFETY: `entry` is a valid THREADENTRY32 with its size set.
    let mut more = unsafe { Thread32First(snapshot.0, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: plain arguments; the returned handle is owned and closed.
            let thread = owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            // SAFETY: the thread handle is open with the access ResumeThread needs.
            let _ = unsafe { ResumeThread(thread.0) };
            resumed += 1;
        }
        // SAFETY: as for Thread32First.
        more = unsafe { Thread32Next(snapshot.0, &raw mut entry) } != 0;
    }
    (resumed > 0)
        .then_some(())
        .ok_or_else(|| io::Error::other("no thread of the child was found to resume"))
}

/// Windows has no signals.
pub fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}
