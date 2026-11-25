//! Process survival checks via `pgrep -f` on unique per-trial markers.

#[cfg(unix)]
use std::process::Command;

/// Return PIDs whose full command line matches `marker` via `pgrep -f`.
/// Excludes the current harness process defensively.
#[cfg(unix)]
pub fn pgrep_marker(marker: &str) -> Result<Vec<u32>, String> {
    let output = Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .map_err(|err| format!("failed to run pgrep: {err}"))?;
    // pgrep exits 0 when matches exist, 1 when no process matched, >1 on error.
    match output.status.code() {
        Some(0) => {
            let self_pid = std::process::id();
            let pids = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .filter(|&pid| pid != self_pid)
                .collect();
            Ok(pids)
        }
        Some(1) => Ok(Vec::new()),
        other => Err(format!(
            "pgrep exited with {:?}: {}",
            other,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

#[cfg(not(unix))]
pub fn pgrep_marker(_marker: &str) -> Result<Vec<u32>, String> {
    Err("pgrep-based survivor checks are only supported on Unix hosts".into())
}

/// Defensively terminate any surviving stub processes after an observation window ends
/// (for example after scenario d's 10-second check) so the harness leaks no background
/// processes on the host.
#[cfg(unix)]
pub fn cleanup_marker(marker: &str) {
    if let Ok(pids) = pgrep_marker(marker)
        && !pids.is_empty()
    {
        let mut cmd = Command::new("kill");
        cmd.arg("-KILL");
        for pid in pids {
            cmd.arg(pid.to_string());
        }
        let _ = cmd.output();
    }
}

#[cfg(not(unix))]
pub fn cleanup_marker(_marker: &str) {}
