//! Proxenos: MCP delegation between coding-agent CLIs.

mod background;
mod client;
mod delegation;
mod discovery;
mod doctor;
mod envelope;
mod integrate;
mod mcp;
mod platform;
mod providers;
mod sessions;
mod state;
mod supervise;
mod tools;
mod usage;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use client::Client;

const USAGE: &str = "usage: proxenos --version\n       proxenos mcp --client <claude|codex|antigravity|kimi|grok|cursor|copilot>\n       proxenos integrate <claude-code|codex> [--remove]\n       proxenos doctor [--json]";

/// How long the runtime may take to stop once serving has ended.
const RUNTIME_STOPS_WITHIN: Duration = Duration::from_millis(500);

enum Command {
    Version,
    Mcp(Client),
    Integrate(fn() -> Result<String, integrate::IntegrateError>),
    Doctor { json: bool },
}

fn parse(args: &[OsString]) -> Option<Command> {
    let args: Vec<&str> = args.iter().map(|arg| arg.to_str()).collect::<Option<_>>()?;
    match args.as_slice() {
        ["--version" | "version"] => Some(Command::Version),
        ["mcp", "--client", name] => Client::parse(name).map(Command::Mcp),
        ["integrate", "claude-code"] => Some(Command::Integrate(integrate::integrate_claude_code)),
        ["integrate", "claude-code", "--remove"] => {
            Some(Command::Integrate(integrate::remove_claude_code))
        }
        ["integrate", "codex"] => Some(Command::Integrate(integrate::integrate_codex)),
        ["integrate", "codex", "--remove"] => Some(Command::Integrate(integrate::remove_codex)),
        ["doctor"] => Some(Command::Doctor { json: false }),
        ["doctor", "--json"] => Some(Command::Doctor { json: true }),
        _ => None,
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse(&args) {
        Some(Command::Version) => {
            let line = format!("proxenos {}\n", env!("CARGO_PKG_VERSION"));
            emit(true, &line, None)
        }
        Some(Command::Mcp(client)) => serve(client),
        Some(Command::Integrate(action)) => match action() {
            Ok(message) => emit(true, &message, None),
            Err(error) => emit(false, "", Some(&format!("proxenos: {error}"))),
        },
        Some(Command::Doctor { json }) => {
            let (ok, output) = doctor::run(json);
            emit(ok, &output, None)
        }
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn emit(ok: bool, stdout: &str, stderr: Option<&str>) -> ExitCode {
    if let Some(error) = stderr {
        eprintln!("{error}");
    }
    // Standard output carries the MCP protocol in `mcp` mode, so nothing reaches it through a
    // print macro (the lint denies that): every write to it is explicit.
    if std::io::stdout().write_all(stdout.as_bytes()).is_ok() && ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn serve(client: Client) -> ExitCode {
    eprintln!("proxenos: serving MCP for {}", client.name());
    let runtime = tokio::runtime::Runtime::new().expect("the async runtime starts");
    let served = runtime.block_on(mcp::serve(tools::Registry::new(client)));
    let stopping = Instant::now();
    let code = match served {
        mcp::Served::Ended => ExitCode::SUCCESS,
        mcp::Served::Failed(reason) => {
            // A cleanup diagnostic can already be blocked writing to this same stream.
            let report = runtime.spawn_blocking(move || {
                let _ = writeln!(std::io::stderr().lock(), "proxenos: {reason}");
            });
            runtime.block_on(async {
                let _ = tokio::time::timeout(RUNTIME_STOPS_WITHIN, report).await;
            });
            ExitCode::FAILURE
        }
    };
    // A client can leave an output pipe full, including standard error.
    runtime.shutdown_timeout(RUNTIME_STOPS_WITHIN.saturating_sub(stopping.elapsed()));
    code
}
