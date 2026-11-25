//! Proxenos: MCP delegation between coding-agent CLIs.

mod client;
mod mcp;
mod tools;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use client::Client;

const USAGE: &str = "usage: proxenos --version\n       proxenos mcp --client <claude|codex|antigravity|kimi|grok|cursor|copilot>";

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
    match run_server() {
        mcp::Served::Ended => ExitCode::SUCCESS,
        mcp::Served::Failed(reason) => {
            eprintln!("proxenos: {reason}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run_server() -> mcp::Served {
    mcp::serve(tools::Registry).await
}
