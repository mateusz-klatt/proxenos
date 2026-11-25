//! Release the files owned by a launch after its child has been reaped.

#[cfg(feature = "test-tools")]
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::mcp::Activity;

pub(crate) enum TemporaryFile {
    #[cfg(feature = "test-tools")]
    Reserved(ReservedFile),
    // Removed only after both the supervisor clears it (reaped or never spawned) and the caller finishes.
    Directory(Arc<PrivateDirectory>),
}

struct DirectoryState {
    reaped: bool,
    caller_done: bool,
}

pub(crate) struct PrivateDirectory {
    path: PathBuf,
    state: Mutex<DirectoryState>,
}

pub(crate) struct DirectoryLease {
    dir: Option<Arc<PrivateDirectory>>,
    activity: Activity,
}

pub(crate) fn temp_root() -> io::Result<PathBuf> {
    std::path::absolute(std::env::temp_dir())
}

impl DirectoryLease {
    pub fn create_blocking(activity: Activity) -> io::Result<Self> {
        let directory = temp_root()?.join(format!("proxenos-run-{}", uuid::Uuid::new_v4()));
        create_private_directory(&directory)?;
        Ok(Self {
            dir: Some(Arc::new(PrivateDirectory {
                path: directory,
                state: Mutex::new(DirectoryState {
                    reaped: true,
                    caller_done: false,
                }),
            })),
            activity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir.as_ref().expect("lease owns its directory").path
    }

    pub fn for_child(&self) -> TemporaryFile {
        let dir = self.dir.as_ref().expect("lease owns its directory");
        dir.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reaped = false;
        TemporaryFile::Directory(Arc::clone(dir))
    }

    fn release(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let dir = self.dir.take()?;
        let remove = {
            let mut state = dir.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.caller_done = true;
            state.reaped
        };
        if !remove {
            return None;
        }
        let activity = self.activity.clone();
        Some(tokio::task::spawn_blocking(move || {
            dir.remove_blocking();
            drop(activity);
        }))
    }

    pub async fn finish(mut self) {
        if let Some(task) = self.release() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

impl Drop for DirectoryLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl PrivateDirectory {
    fn mark_reaped(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.reaped = true;
        state.caller_done
    }

    fn remove_blocking(&self) {
        #[cfg(windows)]
        let removed =
            retry_sharing_violation(|| fs::remove_dir_all(&self.path), Duration::from_secs(1));
        #[cfg(not(windows))]
        let removed = fs::remove_dir_all(&self.path);
        report_error(&self.path, removed);
    }
}

pub(crate) fn create_private_directory(directory: &Path) -> io::Result<()> {
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
    builder.create(directory)
}

#[cfg(feature = "test-tools")]
pub(crate) struct ReservedFile {
    path: PathBuf,
    reservation: PathBuf,
    #[cfg(windows)]
    retry_budget: Duration,
}

impl TemporaryFile {
    #[cfg(feature = "test-tools")]
    pub(crate) async fn create(
        path: PathBuf,
        #[cfg(windows)] retry_budget: Duration,
    ) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let name = path.file_name().ok_or(ErrorKind::InvalidInput)?;
            let mut reserved_name = OsString::from(".proxenos-reserved-");
            reserved_name.push(name);
            let reservation = path.with_file_name(reserved_name);
            // mkdir reserves the name across server processes, even after the CLI unlinks its
            // own input. As a directory it cannot be unlinked by another file's cleanup.
            fs::create_dir(&reservation)?;
            if let Err(error) = fs::File::create_new(&path) {
                let _ = fs::remove_dir(reservation);
                return Err(error);
            }
            Ok(Self::Reserved(ReservedFile {
                path,
                reservation,
                #[cfg(windows)]
                retry_budget,
            }))
        })
        .await?
    }
}

/// Called on the blocking launch or reaper worker.
pub(crate) fn remove_files_blocking(files: Vec<TemporaryFile>) {
    for file in files {
        match file {
            #[cfg(feature = "test-tools")]
            TemporaryFile::Reserved(file) => file.remove(),
            TemporaryFile::Directory(directory) => {
                if directory.mark_reaped() {
                    directory.remove_blocking();
                }
            }
        }
    }
}

#[cfg(feature = "test-tools")]
impl ReservedFile {
    fn remove(&self) {
        #[cfg(windows)]
        let removed = retry_sharing_violation(|| fs::remove_file(&self.path), self.retry_budget);
        #[cfg(not(windows))]
        let removed = fs::remove_file(&self.path);
        // No further unlink will target this name. Return the reservation before writing a
        // diagnostic, which can block when a client stops reading stderr.
        let released = fs::remove_dir(&self.reservation);
        report_error(&self.path, removed);
        report_error(&self.reservation, released);
    }
}

fn report_error(path: &Path, result: io::Result<()>) {
    if let Err(error) = result
        && error.kind() != ErrorKind::NotFound
    {
        let _ = writeln!(
            std::io::stderr().lock(),
            "proxenos: could not remove temporary file {}: {error}",
            path.display()
        );
    }
}

#[cfg(windows)]
fn retry_sharing_violation(
    mut remove: impl FnMut() -> io::Result<()>,
    budget: Duration,
) -> io::Result<()> {
    use std::time::Instant;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    // Terminating a job can return while its processes still hold a file without delete sharing.
    let started = Instant::now();
    let mut removed = remove();
    while removed
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()))
    {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
        removed = remove();
    }
    removed
}
