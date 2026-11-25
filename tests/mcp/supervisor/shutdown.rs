use super::*;

#[cfg(target_os = "linux")]
#[test]
fn eof_fails_at_the_shutdown_deadline_if_a_denied_stop_leaves_a_reaper_waiting() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let lock = directory.0.join("child");
    let mut args = launch(json!([
        "close-output",
        format!("hold:{}", lock.display()),
        format!("until-released:{}", lock.display())
    ]));
    args["temp_file"] = json!(input);
    let mut server = super::unix::platform::server_without_signal_permission();
    server.start_call(1, "test_run", args);
    let live = lock_file(&lock);
    let closed = Instant::now();
    server.close_input();
    let failure = server.response(1);
    assert_eq!(failure["result"]["isError"], true, "{failure}");
    assert!(text(&failure).contains("os error 1"), "{failure}");
    let (status, _) = server.wait_for_exit(crate::common::EXIT_WAIT);
    let elapsed = closed.elapsed();
    let still_live = !is_free(&live);
    fs::write(lock.with_extension("release"), []).unwrap();
    wait_for("stub did not exit after release", || is_free(&live));
    assert_eq!(status.code(), Some(1), "{}", server.stderr());
    assert!(server.stderr().contains("did not finish within 10 s"));
    assert!(elapsed >= Duration::from_secs(10));
    assert!(still_live && input.exists());
}

fn server_with_full_stderr() -> (Server, std::os::unix::net::UnixStream) {
    use std::io::{ErrorKind, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Command;

    let (mut writer, unread, stderr) = {
        let _spawning = crate::common::spawning();
        let (writer, unread) = UnixStream::pair().unwrap();
        let stderr = OwnedFd::from(writer.try_clone().unwrap());
        (writer, unread, stderr)
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_proxenos"));
    command.args(["mcp", "--client", "claude"]).stderr(stderr);
    let mut server = Server::start_command(command);
    server.initialize("2025-11-25");

    // Back up stderr after startup. The test holds the unread socket open.
    writer.set_nonblocking(true).unwrap();
    let fill_by = Instant::now() + WAIT;
    for chunk in [&[0; 4096][..], &[0][..]] {
        loop {
            assert!(Instant::now() < fill_by);
            match writer.write(chunk) {
                Ok(count) => assert!(count > 0),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("filling diagnostic stream: {error}"),
            }
        }
    }
    writer.set_nonblocking(false).unwrap();
    (server, unread)
}

#[test]
fn a_full_diagnostic_stream_cannot_hold_a_reaped_result_or_prevent_shutdown() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let mut args = launch(json!([
        format!("remove:{}", input.display()),
        format!("mkdir:{}", input.display()),
        "exit:19",
    ]));
    args["temp_file"] = json!(input);

    let (mut server, _unread) = server_with_full_stderr();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["code"], 19);
    assert!(input.is_dir());
    let (status, elapsed) = server.finish();
    assert_eq!(status.code(), Some(1));
    assert!(elapsed >= Duration::from_secs(10));
}

#[test]
fn a_skipped_launch_reports_cancellation_while_its_cleanup_is_blocked() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let marker = directory.0.join("started");
    let mut args = launch(json!([format!("hold:{}", marker.display())]));
    args["temp_file"] = json!(input);
    args["on_cancel"] = json!("start");
    let (mut server, _unread) = server_with_full_stderr();
    server.start_call(1, "test_run", args);
    wait_for("call did not prepare its file", || input.exists());

    // Make removal fail. Reporting the error will block until the client reads stderr.
    fs::remove_file(&input).unwrap();
    fs::create_dir(&input).unwrap();
    server.close_input();
    let result = finished(&server.response(1));
    assert_eq!(result["ending"], "cancelled");
    assert!(!marker.exists());
    // The skipped launch's cleanup worker still counts against the shutdown deadline.
    assert_eq!(server.finish().0.code(), Some(1));
}
