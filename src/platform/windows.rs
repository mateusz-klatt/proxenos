//! Start suspended and resume the primary thread only after joining a non-breakaway job.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::{ChildExt, CommandExt};
use std::process::{Child as StdChild, Command, ExitStatus};
use std::ptr::{NonNull, null};

use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::oneshot;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED, ResumeThread};

use crate::mcp::Activity;
use crate::supervise::cleanup::{self, TemporaryFile};
use crate::supervise::{StartError, finish_reap};

struct Group(OwnedHandle);

impl Group {
    fn new() -> io::Result<Self> {
        // SAFETY: null security attributes create a non-inheritable handle; null name is valid.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        let raw = NonNull::new(raw).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: creation succeeded and transfers ownership of this new job handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw.as_ptr()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
        Ok(Self(job))
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: this is our live job handle, with the right to terminate its members.
        let rc = unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
        (rc != 0).then_some(()).ok_or_else(io::Error::last_os_error)
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

    pub async fn finish(mut self) -> io::Result<ExitStatus> {
        drop(self.job);
        drop(self.release);
        finish_reap(&mut self.status, &mut self.cleaned).await
    }
}

pub fn spawn(
    mut command: Command,
    files: &mut Vec<TemporaryFile>,
    activity: Activity,
    before_spawn: impl FnOnce() -> Result<(), StartError>,
) -> Result<Child, StartError> {
    let job = Group::new()?;
    // Rust exposes the primary thread handle on std's Child. Keep it through job assignment.
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    before_spawn()?;
    let mut process = Process(command.spawn()?);
    // SAFETY: the job and suspended process handles are open with assignment rights.
    let rc = unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), process.0.as_raw_handle()) };
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
        cleanup::remove_files_blocking(files_to_remove.as_deref().unwrap_or_default());
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
