//! Release the files owned by a launch after its child has been reaped.

use std::io::{ErrorKind, Write};
use std::path::PathBuf;

pub(crate) async fn remove_files(paths: Vec<PathBuf>) {
    if !paths.is_empty() {
        let _ = tokio::task::spawn_blocking(move || remove_files_blocking(&paths)).await;
    }
}

/// Also used by a reaper that already runs on a blocking thread.
pub(crate) fn remove_files_blocking(paths: &[PathBuf]) {
    #[cfg(windows)]
    let retry_until = std::time::Instant::now() + std::time::Duration::from_secs(1);
    for path in paths {
        let removed = std::fs::remove_file(path);
        #[cfg(windows)]
        let removed = retry_shared_file(path, removed, retry_until);
        if let Err(error) = removed
            && error.kind() != ErrorKind::NotFound
        {
            let _ = writeln!(
                std::io::stderr().lock(),
                "proxenos: could not remove temporary file {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(windows)]
fn retry_shared_file(
    path: &std::path::Path,
    mut removed: std::io::Result<()>,
    until: std::time::Instant,
) -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    // Terminating a job can return while its processes still hold this file without delete sharing.
    while removed
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()))
    {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
        removed = std::fs::remove_file(path);
    }
    removed
}
