//! Proxenos: MCP delegation between coding-agent CLIs.

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: proxenos --version";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let asks_for_version = first
        .as_deref()
        .is_some_and(|arg| arg == "--version" || arg == "version");

    if asks_for_version && args.next().is_none() {
        // Standard output will carry the MCP protocol, so nothing reaches it through
        // a print macro (the lint denies that): every write to it is explicit.
        let line = format!("proxenos {}\n", env!("CARGO_PKG_VERSION"));
        return match std::io::stdout().write_all(line.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }

    eprintln!("{USAGE}");
    ExitCode::from(2)
}
