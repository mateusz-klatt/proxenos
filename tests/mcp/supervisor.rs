//! The D6 child lifecycle, observed by an MCP client and a stand-in CLI.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};

#[path = "supervisor/cleanup.rs"]
mod cleanup;
#[path = "supervisor/lifecycle.rs"]
mod lifecycle;
#[cfg(unix)]
#[path = "supervisor/shutdown.rs"]
mod shutdown;
#[cfg(unix)]
#[path = "supervisor/unix.rs"]
mod unix;

const STUB: &str = env!("CARGO_BIN_EXE_proxenos-stub");

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        Self::under(&std::env::temp_dir())
    }

    fn under(parent: &Path) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("proxenos-test-{}-{serial}", std::process::id()));
        fs::create_dir(&path).expect("a fresh directory for this call");
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn server() -> Server {
    Server::initialized("2025-11-25").0
}

fn launch(args: Value) -> Value {
    json!({ "program": STUB, "args": args, "capture_limit": 4096, "deadline_ms": 60_000 })
}

fn finished(reply: &Value) -> Value {
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    serde_json::from_str(text(reply)).expect("a supervisor result")
}

fn wait_for(description: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{description}");
        thread::sleep(Duration::from_millis(10));
    }
}

// The kernel releases the stub's lock when the process dies, including when a Unix grandchild
// is waiting for its new parent to reap it. The path itself survives the stub.
fn lock_file(path: &Path) -> File {
    wait_for("stub did not acquire the lock", || {
        path.with_extension("ready").exists()
    });
    File::options()
        .read(true)
        .write(true)
        .open(path)
        .expect("stub lock file")
}

fn is_free(file: &File) -> bool {
    match file.try_lock() {
        Ok(()) => {
            file.unlock().expect("release the probe lock");
            true
        }
        Err(TryLockError::WouldBlock) => false,
        Err(error) => panic!("probing the stub's lock failed: {error}"),
    }
}

#[test]
fn a_child_receives_its_launch_settings_and_keeps_its_files_until_exit() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let lock = directory.0.join("child");
    let mut args = launch(json!([
        format!("hold:{}", lock.display()),
        "sleep:150",
        "cwd",
        "env:PROXENOS_TEST",
        "env:PATH",
        "out:two words",
        "err:diagnostic",
        format!("exists:{}", temp.display()),
        "exit:37"
    ]));
    args["cwd"] = json!(directory.0);
    args["temp_file"] = json!(temp);
    args["env"] = json!({"PROXENOS_TEST": "a value with spaces"});
    args["env_remove"] = json!(["PATH"]);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 37);
    assert_eq!(result["signal"], Value::Null);
    let output = result["stdout"].as_str().expect("stdout");
    let mut lines = output.lines();
    let cwd = Path::new(lines.next().expect("cwd"))
        .canonicalize()
        .expect("the child cwd");
    assert_eq!(cwd, directory.0.canonicalize().unwrap());
    assert_eq!(
        lines.collect::<Vec<_>>(),
        [
            "PROXENOS_TEST=a value with spaces",
            "PATH unset",
            "two words",
            "exists=true"
        ]
    );
    assert_eq!(result["stderr"], "diagnostic\n");
    assert_eq!(result["stderr_total"], 11);
    assert_eq!(result["stdout_total"], output.len());
    assert!(is_free(&lock_file(&lock)));
    assert!(!temp.exists(), "the reaped child's temporary file remains");
}

#[test]
fn a_child_can_read_stdin_to_eof_while_the_client_keeps_serving() {
    let mut server = server();
    let result = finished(&server.call(1, "test_run", launch(json!(["stdin"]))));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 0);
    assert_eq!(result["stdout"], "stdin=0\n");
}

#[test]
fn concurrent_calls_wait_for_each_child_after_its_pipes_close() {
    let directory = Directory::new();
    let mut server = server();
    for id in 1..=2 {
        let lock = directory.0.join(id.to_string());
        server.start_call(
            id,
            "test_run",
            launch(json!([
                format!("out:child {id}"),
                "close-output",
                format!("hold:{}", lock.display()),
                format!("until-released:{}", lock.display()),
                format!("exit:{id}"),
            ])),
        );
    }
    for id in 1..=2 {
        assert!(!is_free(&lock_file(&directory.0.join(id.to_string()))));
    }
    // Both children have closed their pipes. Each reply still requires that child's exit.
    assert!(
        server
            .next_frame(Instant::now() + Duration::from_millis(100))
            .is_none()
    );
    for id in [2, 1] {
        fs::write(directory.0.join(format!("{id}.release")), []).unwrap();
        let reply = server.exact_next_frame();
        assert_eq!(reply["id"], id);
        let result = finished(&reply);
        assert_eq!(result["ending"], "exited");
        assert_eq!(result["code"], id);
        assert_eq!(result["stdout"], format!("child {id}\n"));
    }
}

#[test]
fn a_full_capture_still_drains_binary_output_and_times_the_last_byte() {
    let mut args = launch(json!([
        "flood:1048576",
        "err:warning",
        "flood-err:1048576",
        "sleep:200",
        "flood:1"
    ]));
    args["capture_limit"] = json!(3);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 0);
    assert_eq!(result["stdout_bytes"], json!([255, 255, 255]));
    assert_eq!(result["stdout_total"], 1_048_577);
    assert_eq!(result["stderr_bytes"], json!(b"war"));
    assert_eq!(result["stderr_total"], 1_048_584);
    let last_output = result["last_output_ms"]
        .as_u64()
        .expect("output was recorded");
    assert!(
        last_output >= 200 && last_output <= result["elapsed_ms"].as_u64().unwrap(),
        "{result}"
    );
}

#[test]
fn a_missing_working_directory_fails_before_the_program_runs() {
    let directory = Directory::new();
    let marker = directory.0.join("started");
    let mut args = launch(json!([format!("hold:{}", marker.display())]));
    args["cwd"] = json!(directory.0.join("missing-directory"));
    let mut server = server();
    let failed = server.call(1, "test_run", args);
    assert_eq!(failed["result"]["isError"], true);
    assert!(text(&failed).contains("supervision failed"), "{failed}");
    assert!(!marker.exists());
    let next = server.call(2, "test_run", launch(json!(["exit:24"])));
    assert_eq!(finished(&next)["code"], 24);
}

#[test]
fn launch_failure_removes_only_the_file_created_for_this_call() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let mut args = launch(json!([]));
    args["program"] = json!(directory.0.join("missing-cli"));
    args["temp_file"] = json!(temp);
    let mut server = server();
    let failed = server.call(1, "test_run", args.clone());
    assert_eq!(failed["result"]["isError"], true);
    assert!(text(&failed).contains("supervision failed"), "{failed}");
    wait_for("failed launch kept its input or reservation", || {
        fs::read_dir(&directory.0).unwrap().next().is_none()
    });

    fs::write(&temp, "another call").unwrap();
    args["program"] = json!(STUB);
    let refused = server.call(2, "test_run", args);
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(fs::read_to_string(temp).unwrap(), "another call");
}

#[test]
fn a_deadline_stops_and_reaps_a_silent_child() {
    let mut args = launch(json!(["sleep:60000"]));
    args["deadline_ms"] = json!(4000);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["ending"], "timed_out");
    assert_eq!(result["last_output_ms"], Value::Null);
    assert!(result["elapsed_ms"].as_u64().unwrap() >= 4000);
    #[cfg(unix)]
    assert_eq!(result["signal"], libc::SIGTERM);
    #[cfg(windows)]
    assert_eq!(result["code"], 1);
}

#[test]
fn cancelling_a_call_stops_its_grandchild_and_cleans_up_while_serving() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let grandchild = directory.0.join("grandchild");
    let mut args = launch(json!([
        format!("grandchild:{}", grandchild.display()),
        "sleep:60000"
    ]));
    args["temp_file"] = json!(temp);
    let mut server = server();
    server.start_call(1, "test_run", args);
    let file = lock_file(&grandchild);
    assert!(!is_free(&file));
    assert!(temp.exists());
    let stopped = Instant::now();
    server.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 }
    }));
    wait_for("grandchild survived cancellation", || is_free(&file));
    // On Unix the grandchild ignores SIGTERM; the leader does not.
    #[cfg(unix)]
    assert!(
        stopped.elapsed() >= Duration::from_millis(2900),
        "descendant lost its grace period"
    );
    #[cfg(windows)]
    assert!(stopped.elapsed() < WAIT);
    wait_for("cancelled child's files were not removed", || {
        !temp.exists()
    });
    server.start_call(2, "test_echo", json!({}));
    assert_eq!(server.exact_next_frame()["id"], 2);
    server.close_input();
    assert!(server.frames_until_end().is_empty());
}

#[test]
fn closing_the_connection_cancels_and_reaps_the_child() {
    let directory = Directory::new();
    let lock = directory.0.join("child");
    let mut server = server();
    server.start_call(
        1,
        "test_run",
        launch(json!([format!("hold:{}", lock.display()), "sleep:60000"])),
    );
    let file = lock_file(&lock);
    assert!(!is_free(&file));
    server.close_input();
    let result = finished(&server.response(1));
    assert_eq!(result["ending"], "cancelled");
    assert!(is_free(&file));
    assert!(server.finish().0.success());
}

#[test]
fn a_pipe_holding_descendant_gets_a_deadline_after_its_parent_exits() {
    let directory = Directory::new();
    let lock = directory.0.join("grandchild");
    let mut server = server();
    let leader = directory.0.join("leader");
    let args = launch(json!([
        format!("grandchild:{}", lock.display()),
        format!("hold:{}", leader.display()),
        format!("until-released:{}", leader.display()),
        "exit:19"
    ]));
    server.start_call(1, "test_run", args);
    let file = lock_file(&lock);
    let leader_lock = lock_file(&leader);
    assert!(!is_free(&file));
    assert!(!is_free(&leader_lock));
    let started = Instant::now();
    fs::write(leader.with_extension("release"), []).unwrap();
    let result = finished(&server.response(1));
    let elapsed = started.elapsed();
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 19);
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("grandchild ready")
    );
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed < WAIT,
        "pipe drain took {elapsed:?}"
    );
    wait_for("pipe-holding grandchild survived", || is_free(&file));
}

#[cfg(unix)]
#[test]
fn a_child_in_a_new_session_can_finish_during_its_sigterm_grace() {
    let directory = Directory::new();
    let ready = directory.0.join("handler-ready");
    let args = launch(json!([
        "session",
        "ignore-term",
        format!("await-term:{}", ready.display()),
        "exit:37"
    ]));
    let mut server = server();
    server.start_call(1, "test_run", args);
    wait_for("child did not install its SIGTERM handler", || {
        ready.exists()
    });
    server.close_input();
    let result = finished(&server.response(1));
    assert_eq!(result["stdout"], "session_leader=true\nterminal=false\n");
    assert_eq!(result["ending"], "cancelled");
    assert_eq!(result["code"], 37);
    assert_eq!(result["signal"], Value::Null);
}

#[cfg(unix)]
#[test]
fn a_child_ignoring_sigterm_is_killed_after_three_seconds() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let lock = directory.0.join("child");
    let mut args = launch(json!([
        "ignore-term",
        format!("hold:{}", lock.display()),
        "sleep:60000"
    ]));
    args["temp_file"] = json!(temp);
    let mut server = server();
    server.start_call(1, "test_run", args);
    let file = lock_file(&lock);
    let stopped = Instant::now();
    server.close_input();
    thread::sleep(Duration::from_millis(500));
    assert!(!is_free(&file));
    assert!(
        temp.exists(),
        "temporary input disappeared during SIGTERM grace"
    );
    let result = finished(&server.response(1));
    let elapsed = stopped.elapsed();
    assert_eq!(result["ending"], "cancelled");
    assert_eq!(result["signal"], libc::SIGKILL);
    assert_eq!(result["code"], Value::Null);
    assert!(
        elapsed >= Duration::from_millis(2900) && elapsed < WAIT,
        "stop took {elapsed:?}"
    );
    assert!(is_free(&file));
    assert!(!temp.exists());
}

#[cfg(unix)]
#[test]
fn an_escaped_descendant_cannot_keep_the_readers_open() {
    let directory = Directory::new();
    let lock = directory.0.join("escaped");
    let mut server = server();
    let result = finished(&server.call(
        1,
        "test_run",
        launch(json!([format!("escape:{}", lock.display())])),
    ));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 0);
    let file = lock_file(&lock);
    assert!(
        !is_free(&file),
        "escaped descendant should still be alive after the drain"
    );
    fs::write(lock.with_extension("release"), []).unwrap();
    wait_for("escaped stub did not exit", || is_free(&file));
}

#[cfg(windows)]
#[test]
fn a_windows_job_refuses_breakaway_and_ends_the_grandchild() {
    let directory = Directory::new();
    let lock = directory.0.join("grandchild");
    let mut server = server();
    let result = finished(&server.call(
        1,
        "test_run",
        launch(json!(["window", format!("breakaway:{}", lock.display())])),
    ));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 0);
    let stdout = result["stdout"].as_str().unwrap();
    assert!(
        stdout.lines().any(|line| line == "visible=false"),
        "{stdout}"
    );
    assert!(stdout.lines().any(|line| line == "breakaway=5"), "{stdout}");
    let file = lock_file(&lock);
    wait_for("breakaway fallback survived the job", || is_free(&file));
}

#[cfg(unix)]
mod platform {
    #![allow(unsafe_code)]

    use super::*;
    use std::mem::MaybeUninit;

    #[test]
    fn a_client_with_sigchld_blocked_still_observes_its_child_exit() {
        // Set the signal mask only on the thread that launches the server. Its other threads
        // inherit that mask at startup. Other tests can start processes concurrently.
        let (send, receive) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut set = MaybeUninit::<libc::sigset_t>::uninit();
            // SAFETY: sigemptyset initializes set, sigaddset adds a valid signal, and the
            // mask only applies to this fresh thread. The kernel copies it during the call.
            unsafe {
                assert_eq!(libc::sigemptyset(set.as_mut_ptr()), 0);
                assert_eq!(libc::sigaddset(set.as_mut_ptr(), libc::SIGCHLD), 0);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut()),
                    0
                );
            }
            let _ = send.send(server());
        });
        let mut server = receive
            .recv_timeout(WAIT)
            .expect("server completed its handshake");
        let result = finished(&server.call(1, "test_run", launch(json!(["sleep:150", "exit:31"]))));
        assert_eq!(result["ending"], "exited");
        assert_eq!(result["code"], 31);
    }
}
