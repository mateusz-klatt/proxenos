//! A Linux write lease holds execve inside native process creation.
#![cfg(all(target_os = "linux", feature = "test-tools"))]

#[allow(dead_code)]
mod common;
#[allow(dead_code)]
mod provider_support;

mod linux {
    #![allow(unsafe_code)]

    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::common::{WAIT, text};
    use crate::provider_support::{Fleet, stub};

    #[test]
    fn cursor_reports_timeout_and_cancellation_while_exec_is_still_blocked() {
        for cancel in [false, true] {
            let fleet = Fleet::new(&[]);
            let executable = fleet.root.join("bin/cursor-agent");
            // Use a separate inode so the lease cannot stall other tests of the same stub.
            fs::copy(stub(), &executable).unwrap();
            fleet.script("cursor-agent", json!([{"sleep_ms":60_000}]));
            let mut server = fleet.server("cursor");
            let lease = File::options().write(true).open(executable).unwrap();
            let fd = lease.as_raw_fd();
            // SAFETY: both commands take one integer argument and use the open lease descriptor.
            unsafe {
                assert_eq!(libc::fcntl(fd, libc::F_SETLEASE, libc::F_WRLCK), 0);
                assert_eq!(libc::fcntl(fd, libc::F_SETOWN, 0), 0);
            }
            server.start_call(9, "usage", json!({"providers":["cursor"]}));
            let until = Instant::now() + WAIT;
            loop {
                // SAFETY: F_GETLEASE only reads the current state of this open descriptor.
                if unsafe { libc::fcntl(fd, libc::F_GETLEASE) } == libc::F_RDLCK {
                    break;
                }
                assert!(Instant::now() < until, "cursor-agent did not enter execve");
                std::thread::sleep(Duration::from_millis(10));
            }
            if cancel {
                server.close_input();
            }
            let frame = server
                .next_frame(Instant::now() + Duration::from_secs(20))
                .expect("usage must answer while native spawn still holds the lease");
            let report: Value = serde_json::from_str(text(&frame)).unwrap();
            assert_eq!(frame["id"], 9);
            let reason = if cancel { "cancelled" } else { "timeout" };
            assert_eq!(
                report["providers"]["cursor"],
                json!({"status":"unavailable","reason":reason})
            );
            assert!(fleet.records().is_empty(), "stub ran before lease release");
            drop(lease);
            // A published child would sleep for a minute. D6's reaper holds the server
            // shutdown handle until the late child has actually exited.
            assert!(server.finish().0.success(), "{}", server.stderr());
        }
    }
}
