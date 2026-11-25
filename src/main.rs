//! Proxenos: MCP delegation between coding-agent CLIs.

mod client;
mod mcp;
#[cfg(feature = "test-tools")]
mod platform;
#[cfg(feature = "test-tools")]
mod supervise;
mod tools;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use client::Client;

const USAGE: &str = "usage: proxenos --version\n       proxenos mcp --client <claude|codex|antigravity|kimi|grok|cursor|copilot>";

/// How long the runtime may take to stop once serving has ended.
const RUNTIME_STOPS_WITHIN: Duration = Duration::from_millis(500);

enum Command {
    Version,
    Mcp(Client),
}

fn parse(args: &[OsString]) -> Option<Command> {
    let args: Vec<&str> = args.iter().map(|arg| arg.to_str()).collect::<Option<_>>()?;
    match args.as_slice() {
        ["--version" | "version"] => Some(Command::Version),
        ["mcp", "--client", name] => Client::parse(name).map(Command::Mcp),
        _ => None,
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse(&args) {
        Some(Command::Version) => version(),
        Some(Command::Mcp(client)) => serve(client),
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn version() -> ExitCode {
    // Standard output carries the MCP protocol, so nothing reaches it through a print macro
    // (the lint denies that): every write to it is explicit.
    let line = format!("proxenos {}\n", env!("CARGO_PKG_VERSION"));
    match std::io::stdout().write_all(line.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn serve(client: Client) -> ExitCode {
    eprintln!("proxenos: serving MCP for {}", client.name());
    let runtime = tokio::runtime::Runtime::new().expect("the async runtime starts");
    let served = runtime.block_on(mcp::serve(tools::Registry));
    // A write to an output that nobody reads blocks a thread that a plain drop would wait for.
    runtime.shutdown_timeout(RUNTIME_STOPS_WITHIN);
    match served {
        mcp::Served::Ended => ExitCode::SUCCESS,
        mcp::Served::Failed(reason) => {
            eprintln!("proxenos: {reason}");
            ExitCode::FAILURE
        }
    }
}
