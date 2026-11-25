//! Release the files owned by a launch after its child has been reaped.

#[cfg(feature = "test-tools")]
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;

pub(crate) enum TemporaryFile {
    #[cfg(feature = "test-tools")]
    Reserved(ReservedFile),
    // Both the parser and the reaper retain the directory. The last owner releases it.
    Directory(std::sync::Arc<PrivateDirectory>),
}

pub(crate) struct PrivateDirectory {
    pub path: PathBuf,
}

impl PrivateDirectory {
    pub fn create() -> io::Result<std::sync::Arc<Self>> {
        let directory = std::env::temp_dir().join(format!("proxenos-run-{}", uuid::Uuid::new_v4()));
        create_private_directory(&directory)?;
        Ok(std::sync::Arc::new(Self { path: directory }))
    }
}

pub(crate) fn create_private_directory(directory: &std::path::Path) -> io::Result<()> {
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
    builder.create(directory)
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        report_error(&self.path, fs::remove_dir_all(&self.path));
    }
}

#[cfg(feature = "test-tools")]
pub(crate) struct ReservedFile {
    path: PathBuf,
    reservation: PathBuf,
    #[cfg(windows)]
    retry_budget: std::time::Duration,
}

impl TemporaryFile {
    #[cfg(feature = "test-tools")]
    pub(crate) async fn create(
        path: PathBuf,
        #[cfg(windows)] retry_budget: std::time::Duration,
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
            TemporaryFile::Directory(directory) => drop(directory),
        }
    }
}

#[cfg(feature = "test-tools")]
impl ReservedFile {
    fn remove(&self) {
        let removed = fs::remove_file(&self.path);
        #[cfg(windows)]
        let removed = retry_shared_file(&self.path, removed, self.retry_budget);
        // No further unlink will target this name. Return the reservation before writing a
        // diagnostic, which can block when a client stops reading stderr.
        let released = fs::remove_dir(&self.reservation);
        report_error(&self.path, removed);
        report_error(&self.reservation, released);
    }
}

fn report_error(path: &std::path::Path, result: io::Result<()>) {
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

#[cfg(all(windows, feature = "test-tools"))]
fn retry_shared_file(
    path: &std::path::Path,
    mut removed: std::io::Result<()>,
    budget: std::time::Duration,
) -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    // Terminating a job can return while its processes still hold this file without delete sharing.
    let started = Instant::now();
    while removed
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()))
    {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
        removed = std::fs::remove_file(path);
    }
    removed
}
