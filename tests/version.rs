//! Drives the compiled binary, as every behavioural test of Proxenos will.

use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};

fn proxenos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_proxenos"))
}

/// Held for the whole of each test, so that no process starts while another test holds a pipe end.
/// A process starting at that moment would hold a copy of it, at least until it executes the binary,
/// and a write into a pipe whose reader the test has closed would then succeed.
fn alone() -> MutexGuard<'static, ()> {
    static ALONE: Mutex<()> = Mutex::new(());
    ALONE.lock().unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn version_prints_the_package_version_on_stdout() {
    let _alone = alone();
    for spelling in ["--version", "version"] {
        let output = proxenos().arg(spelling).output().expect("the binary runs");

        assert!(output.status.success(), "spelling: {spelling}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("proxenos {}\n", env!("CARGO_PKG_VERSION")),
            "spelling: {spelling}"
        );
        assert!(output.stderr.is_empty(), "spelling: {spelling}");
    }
}

#[test]
fn anything_else_is_a_usage_error_and_leaves_stdout_empty() {
    let _alone = alone();
    let cases: [&[&str]; 7] = [
        &[],
        &["mcp"],
        &["--version", "extra"],
        &["mcp", "--client"],
        &["mcp", "--client", "nobody"],
        &["mcp", "--client", "claude", "extra"],
        &["mcp", "claude"],
    ];

    for args in cases {
        let output = proxenos().args(args).output().expect("the binary runs");

        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert!(output.stdout.is_empty(), "args: {args:?}");
        assert!(!output.stderr.is_empty(), "args: {args:?}");
    }
}

#[test]
fn a_version_that_cannot_be_written_is_a_failure() {
    let _alone = alone();
    // Nobody reads this pipe, so the write to it fails.
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);

    let status = proxenos()
        .arg("--version")
        .stdout(writer)
        .status()
        .expect("the binary runs");

    assert_eq!(status.code(), Some(1));
}
