//! Drives the compiled binary, as every behavioural test of Proxenos will.

use std::process::Command;

fn proxenos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_proxenos"))
}

#[test]
fn version_prints_the_package_version_on_stdout() {
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
    let cases: [&[&str]; 3] = [&[], &["mcp"], &["--version", "extra"]];

    for args in cases {
        let output = proxenos().args(args).output().expect("the binary runs");

        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert!(output.stdout.is_empty(), "args: {args:?}");
        assert!(!output.stderr.is_empty(), "args: {args:?}");
    }
}
