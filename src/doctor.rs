//! Diagnostic checks for `proxenos doctor` (D10).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;
use tokio_util::sync::CancellationToken;

use crate::discovery::{self, CliStatus};
use crate::integrate::{
    self, CLIENT_CLAUDE_CODE, ENABLED_PLUGIN_KEY, MARKETPLACE_NAME, MCP_SERVER_NAME, PLUGIN_NAME,
};
use crate::mcp::Activity;
use crate::platform;
use crate::providers::{self, Provider};
use crate::state;
use crate::supervise::{Ending, StartError, write_stdin};

const REPORT_SCHEMA_VERSION: u32 = 1;
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_CLEAN_EXIT_GRACE: Duration = Duration::from_secs(2);
const MAX_HANDSHAKE_LINE_BYTES: usize = 1 << 20;
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Check {
    fn new(
        name: impl Into<String>,
        status: Status,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self {
            name: name.into(),
            status,
            message: message.into(),
            details,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub ok: bool,
    pub checks: Vec<Check>,
}

struct RegisteredCommand {
    settings_path: PathBuf,
    marketplace_dir: Option<PathBuf>,
    command: PathBuf,
    args: Vec<String>,
}

#[derive(Clone, Copy)]
enum IntegrationClient<'a> {
    ClaudeCode,
    Codex(&'a Value),
}

impl IntegrationClient<'_> {
    fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => CLIENT_CLAUDE_CODE,
            Self::Codex(_) => "codex",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex(_) => "Codex",
        }
    }

    fn registration(self) -> Result<RegisteredCommand, String> {
        match self {
            Self::ClaudeCode => read_claude_registration(&state::claude_settings_path()?),
            Self::Codex(record) => {
                let (settings_path, command, args) = integrate::read_codex_registration(record)
                    .map_err(|error| error.to_string())?;
                Ok(RegisteredCommand {
                    settings_path,
                    marketplace_dir: None,
                    command,
                    args,
                })
            }
        }
    }
}

enum HandshakeError {
    Io(io::Error),
    Protocol(String),
    TimedOut(Duration),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "MCP handshake I/O error: {error}"),
            Self::Protocol(message) => f.write_str(message),
            Self::TimedOut(limit) => write!(
                f,
                "MCP handshake exceeded its {} ms deadline.",
                limit.as_millis()
            ),
        }
    }
}

struct HandshakeInfo {
    server_name: Option<String>,
    server_version: Option<String>,
    tools: Vec<String>,
}

fn handshake_timeout() -> Duration {
    #[cfg(feature = "test-tools")]
    if let Some(ms) = std::env::var("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    DEFAULT_HANDSHAKE_TIMEOUT
}

fn handshake_cleanup_error(
    finish_status: io::Result<std::process::ExitStatus>,
) -> Option<io::Error> {
    #[cfg(feature = "test-tools")]
    if std::env::var_os("PROXENOS_TEST_FAIL_HANDSHAKE_CLEANUP").is_some() {
        return Some(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected handshake cleanup failure",
        ));
    }
    finish_status.err()
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes =
        fs::read(path).map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| format!("Could not parse {}: {err}", path.display()))
}

fn read_claude_registration(settings_path: &Path) -> Result<RegisteredCommand, String> {
    let settings = read_json(settings_path)?;
    let marketplace_str = settings["extraKnownMarketplaces"][MARKETPLACE_NAME]["source"]
        .as_object()
        .filter(|source| source.get("source").and_then(Value::as_str) == Some("directory"))
        .and_then(|source| source.get("path").and_then(Value::as_str))
        .filter(|path| !path.is_empty());
    let enabled = settings["enabledPlugins"][ENABLED_PLUGIN_KEY].as_bool() == Some(true);
    let (Some(marketplace_str), true) = (marketplace_str, enabled) else {
        return Err(format!(
            "Proxenos is not registered and enabled in {}.",
            settings_path.display()
        ));
    };

    let marketplace_dir = PathBuf::from(marketplace_str);
    let marketplace = read_json(
        &marketplace_dir
            .join(".claude-plugin")
            .join("marketplace.json"),
    )?;
    let plugin_source = marketplace["plugins"]
        .as_array()
        .and_then(|plugins| plugins.iter().find(|entry| entry["name"] == PLUGIN_NAME))
        .filter(|_| marketplace["name"] == MARKETPLACE_NAME)
        .and_then(|entry| entry["source"].as_str())
        .filter(|source| !source.is_empty())
        .ok_or_else(|| {
            format!(
                "Invalid marketplace manifest under {}.",
                marketplace_dir.display()
            )
        })?;

    let plugin_dir = marketplace_dir.join(plugin_source);
    let plugin = read_json(&plugin_dir.join(".claude-plugin").join("plugin.json"))?;
    let mcp = read_json(&plugin_dir.join(".mcp.json"))?;
    let server = &mcp["mcpServers"][MCP_SERVER_NAME];
    let command = server["command"].as_str().filter(|path| !path.is_empty());
    let args = server["args"].as_array().and_then(|items| {
        items
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    });
    let (true, Some(command), Some(args)) = (plugin["name"] == PLUGIN_NAME, command, args) else {
        return Err(format!(
            "Invalid plugin or MCP server definition under {}.",
            plugin_dir.display()
        ));
    };

    Ok(RegisteredCommand {
        settings_path: settings_path.to_path_buf(),
        marketplace_dir: Some(marketplace_dir),
        command: PathBuf::from(command),
        args,
    })
}

fn installed_clients() -> Result<BTreeMap<String, Value>, integrate::IntegrateError> {
    match state::state_dir() {
        Some(dir) => integrate::read_installed_clients(&dir),
        None => Ok(BTreeMap::new()),
    }
}

fn registration_clients(installed: &BTreeMap<String, Value>) -> Vec<IntegrationClient<'_>> {
    let codex = installed.get("codex");
    let mut clients = Vec::new();
    if installed.contains_key(CLIENT_CLAUDE_CODE) || codex.is_none() {
        clients.push(IntegrationClient::ClaudeCode);
    }
    if let Some(record) = codex {
        clients.push(IntegrationClient::Codex(record));
    }
    clients
}

fn check_registration(client: IntegrationClient<'_>) -> (Check, Option<RegisteredCommand>) {
    let name = format!("{}.registration", client.name());
    let label = client.label();
    let update = format!(
        "Run `proxenos integrate {}` again to update the registration.",
        client.name()
    );
    let reg = match client.registration() {
        Ok(reg) => reg,
        Err(reason) => return (Check::new(name, Status::Fail, reason, None), None),
    };
    let command = reg.command.display();
    if !reg.command.is_file() {
        let reason = format!("Registered executable does not exist: {command}. {update}");
        return (Check::new(name, Status::Fail, reason, None), None);
    }

    let mut details = json!({
        "settings_path": reg.settings_path.display().to_string(),
        "command": command.to_string(),
        "args": reg.args,
    });
    if let Some(marketplace) = &reg.marketplace_dir {
        details["marketplace_dir"] = json!(marketplace.display().to_string());
    }
    let mismatch = state::find_on_path(Path::new("proxenos"), |_| true).and_then(|on_path| {
        let reg_real = state::normalize_path(fs::canonicalize(&reg.command).ok()?);
        let path_real = state::normalize_path(fs::canonicalize(&on_path).ok()?);
        (reg_real != path_real).then_some((on_path, path_real))
    });
    let (status, message) = if let Some((on_path, path_real)) = mismatch {
        details["path_command"] = json!(on_path.display().to_string());
        details["path_resolved"] = json!(path_real.display().to_string());
        let message = format!(
            "{label} registration points to {command}, but `proxenos` on PATH resolves to {}. {update}",
            path_real.display()
        );
        (Status::Warn, message)
    } else {
        (
            Status::Ok,
            format!("{label} registration points to {command}."),
        )
    };
    (Check::new(name, status, message, Some(details)), Some(reg))
}

fn check_state_dir() -> Check {
    let dir = match state::require_state_dir() {
        Ok(path) => path,
        Err(message) => return Check::new("state_dir.permissions", Status::Fail, message, None),
    };
    match fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Check::new(
            "state_dir.permissions",
            Status::Warn,
            format!("State directory {} does not exist yet.", dir.display()),
            Some(json!({ "path": dir.display().to_string() })),
        ),
        Err(err) => Check::new(
            "state_dir.permissions",
            Status::Fail,
            format!("Could not inspect state directory {}: {err}", dir.display()),
            None,
        ),
        Ok(meta) => check_state_dir_metadata(&dir, &meta),
    }
}

#[cfg(unix)]
fn check_state_dir_metadata(dir: &Path, meta: &fs::Metadata) -> Check {
    use std::os::unix::fs::PermissionsExt;

    let mode = meta.permissions().mode() & 0o777;
    let mode_text = format!("{mode:04o}");
    let details = Some(json!({
        "path": dir.display().to_string(),
        "mode": mode_text,
    }));
    if !state::is_private_dir(meta) {
        Check::new(
            "state_dir.permissions",
            Status::Fail,
            format!(
                "State directory {} is not a private directory (mode {mode_text}; expected 0700 owned by the current user, not a symlink).",
                dir.display()
            ),
            details,
        )
    } else {
        Check::new(
            "state_dir.permissions",
            Status::Ok,
            format!(
                "State directory {} has private permissions ({mode_text}).",
                dir.display()
            ),
            details,
        )
    }
}

#[cfg(not(unix))]
fn check_state_dir_metadata(dir: &Path, meta: &fs::Metadata) -> Check {
    let details = Some(json!({ "path": dir.display().to_string() }));
    if !state::is_private_dir(meta) {
        Check::new(
            "state_dir.permissions",
            Status::Fail,
            format!(
                "State directory {} is not a real directory (must not be a file, symlink, or reparse point).",
                dir.display()
            ),
            details,
        )
    } else {
        Check::new(
            "state_dir.permissions",
            Status::Ok,
            format!(
                "State directory {} exists (Unix permission bits do not apply on this platform).",
                dir.display()
            ),
            details,
        )
    }
}

fn provider_check(provider: &Provider, status: CliStatus) -> Check {
    let name = format!("cli.{}", provider.name);
    match status {
        CliStatus::Ready { program, version } => Check::new(
            name,
            Status::Ok,
            format!("{} {version} ({})", provider.cli, program.display()),
            Some(json!({
                "provider": provider.name,
                "cli": provider.cli,
                "path": program.display().to_string(),
                "version": version,
            })),
        ),
        CliStatus::NotFound(failure) => Check::new(
            name,
            Status::Warn,
            failure.message,
            Some(json!({
                "provider": provider.name,
                "cli": provider.cli,
            })),
        ),
        #[cfg(windows)]
        CliStatus::UnsupportedLauncher(failure) => Check::new(
            name,
            Status::Fail,
            failure.message,
            Some(json!({
                "provider": provider.name,
                "cli": provider.cli,
            })),
        ),
        CliStatus::ProbeFailed { program, error } => Check::new(
            name,
            Status::Fail,
            error.failure.message,
            Some(json!({
                "provider": provider.name,
                "cli": provider.cli,
                "path": program.display().to_string(),
            })),
        ),
    }
}

async fn read_bounded_line(
    reader: &mut BufReader<ChildStdout>,
    limit: usize,
) -> Result<Option<Vec<u8>>, HandshakeError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(HandshakeError::Io)?;
        if available.is_empty() {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |pos| pos + 1);
        if bytes.len() + take > limit {
            return Err(HandshakeError::Protocol(format!(
                "MCP server response line exceeded {limit} bytes."
            )));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(bytes));
        }
    }
}

async fn read_response(
    reader: &mut BufReader<ChildStdout>,
    expected_id: i64,
) -> Result<Value, HandshakeError> {
    loop {
        let Some(line) = read_bounded_line(reader, MAX_HANDSHAKE_LINE_BYTES).await? else {
            return Err(HandshakeError::Protocol(
                "MCP server closed stdout before answering.".to_owned(),
            ));
        };
        let frame: Value = serde_json::from_slice(&line).map_err(|err| {
            HandshakeError::Protocol(format!("MCP server returned invalid JSON: {err}"))
        })?;
        if frame.get("id") == Some(&json!(expected_id)) {
            if let Some(error) = frame.get("error") {
                return Err(HandshakeError::Protocol(format!(
                    "MCP server returned error: {error}"
                )));
            }
            return Ok(frame);
        }
    }
}

async fn run_handshake(
    command: &Path,
    args: &[String],
    activity: Activity,
) -> Result<HandshakeInfo, HandshakeError> {
    let limit = handshake_timeout();
    let deadline = Instant::now() + limit;
    let tokio_deadline = tokio::time::Instant::from_std(deadline);

    let mut cmd = Command::new(command);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    // Spawn via D6's platform layer under the same deadline so the child runs in its own Unix
    // session/process group or Windows kill-on-close job and descendants are stopped too.
    let launched = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        platform::spawn(
            cmd,
            platform::Input::Piped,
            &mut files,
            None,
            activity,
            || {
                (Instant::now() < deadline)
                    .then_some(())
                    .ok_or(StartError::Skipped(Ending::TimedOut))
            },
        )
    });
    let spawned = tokio::time::timeout_at(tokio_deadline, launched)
        .await
        .map(|res| res.expect("handshake spawn worker finished"));
    let mut child = match spawned {
        Ok(Ok(child)) => child,
        Ok(Err(StartError::Io(err))) => return Err(HandshakeError::Io(err)),
        Ok(Err(StartError::Skipped(_))) | Err(_) => return Err(HandshakeError::TimedOut(limit)),
    };

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let drain_stderr = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take();

    let exchange = async {
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "proxenos-doctor",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        });
        write_stdin(&mut stdin, format!("{init_req}\n"))
            .await
            .map_err(HandshakeError::Io)?;
        let init = read_response(&mut reader, 1).await?;

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        write_stdin(&mut stdin, format!("{initialized}\n{list_req}\n"))
            .await
            .map_err(HandshakeError::Io)?;
        let listed = read_response(&mut reader, 2).await?;

        let Some(entries) = listed["result"]["tools"].as_array() else {
            return Err(HandshakeError::Protocol(
                "MCP tools/list response did not include a tools array.".to_owned(),
            ));
        };
        let tools = entries
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        let server_info = &init["result"]["serverInfo"];
        Ok(HandshakeInfo {
            server_name: server_info["name"].as_str().map(str::to_owned),
            server_version: server_info["version"].as_str().map(str::to_owned),
            tools,
        })
    };

    let outcome = match tokio::time::timeout_at(tokio_deadline, exchange).await {
        Ok(result) => result,
        Err(_) => Err(HandshakeError::TimedOut(limit)),
    };

    // Drain stdout while the server handles stdin EOF so a full pipe cannot prevent it from
    // exiting. Keep the clean-exit grace bounded by the handshake deadline.
    let drain_stdout = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    });
    drop(stdin);
    let grace_deadline =
        tokio_deadline.min(tokio::time::Instant::now() + HANDSHAKE_CLEAN_EXIT_GRACE);
    let exited_cleanly = if tokio::time::Instant::now() < grace_deadline {
        matches!(
            tokio::time::timeout_at(grace_deadline, child.wait_for_exit()).await,
            Ok(Ok(_))
        )
    } else {
        false
    };
    drain_stdout.abort();
    let stop_error = if exited_cleanly {
        None
    } else {
        child.stop().await.err()
    };
    let (finish_status, _accounting) = child.finish().await;
    drain_stderr.abort();
    let cleanup_error = handshake_cleanup_error(finish_status).or(stop_error);

    match (outcome, cleanup_error) {
        (Ok(info), None) => Ok(info),
        (Ok(_), Some(error)) => Err(HandshakeError::Protocol(format!(
            "MCP handshake cleanup failed ({error}); the server process may still be running."
        ))),
        (Err(error), None) => Err(error),
        (Err(error), Some(cleanup)) => Err(HandshakeError::Protocol(format!(
            "{error} Handshake cleanup also failed ({cleanup}); the server process may still be running."
        ))),
    }
}

async fn check_handshake(
    client: IntegrationClient<'_>,
    registration: Option<&RegisteredCommand>,
    any_provider_found: bool,
    activity: Activity,
) -> Check {
    let name = format!("{}.handshake", client.name());
    let Some(reg) = registration else {
        return Check::new(
            name,
            Status::Fail,
            format!("Skipped: {} registration check failed.", client.label()),
            None,
        );
    };
    let command_str = reg.command.display().to_string();
    match run_handshake(&reg.command, &reg.args, activity).await {
        Ok(info) => {
            let details = Some(json!({
                "command": command_str,
                "args": reg.args,
                "server_name": info.server_name,
                "server_version": info.server_version,
                "tools": info.tools,
            }));
            if info.tools.is_empty() {
                let message = if any_provider_found {
                    "MCP handshake succeeded, but tools/list returned 0 tools.".to_owned()
                } else {
                    let clis: Vec<&str> = providers::ALL.iter().map(|p| p.cli).collect();
                    format!(
                        "MCP handshake succeeded, but tools/list returned 0 tools: no provider CLI found on PATH (looked for: {}).",
                        clis.join(", ")
                    )
                };
                Check::new(name, Status::Warn, message, details)
            } else {
                Check::new(
                    name,
                    Status::Ok,
                    format!("MCP handshake succeeded ({} tools).", info.tools.len()),
                    details,
                )
            }
        }
        Err(error) => Check::new(
            name,
            Status::Fail,
            error.to_string(),
            Some(json!({
                "command": command_str,
                "args": reg.args,
            })),
        ),
    }
}

pub fn run(json_output: bool) -> (bool, String) {
    let mut checks = Vec::new();
    let mut registrations = Vec::new();
    let installed = installed_clients();
    match &installed {
        Ok(installed) => {
            for client in registration_clients(installed) {
                let (check, registration) = check_registration(client);
                checks.push(check);
                registrations.push((client, registration));
            }
        }
        Err(error) => checks.push(Check::new(
            "integration.manifest",
            Status::Fail,
            error.to_string(),
            None,
        )),
    }
    let state_check = check_state_dir();

    let runtime = tokio::runtime::Runtime::new().expect("the async runtime starts");
    let (cli_checks, handshake_checks) = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let (activity, mut finished) = Activity::track();
        let mut handles = Vec::with_capacity(providers::ALL.len());
        for provider in providers::ALL {
            let cancel = cancel.clone();
            let activity = activity.clone();
            handles.push((
                provider,
                tokio::spawn(async move { discovery::inspect(provider, &cancel, activity).await }),
            ));
        }

        let mut cli_checks = Vec::with_capacity(handles.len());
        let mut any_provider_found = false;
        for (provider, handle) in handles {
            let status = handle.await.expect("provider check finished");
            any_provider_found |= !matches!(status, CliStatus::NotFound(_));
            cli_checks.push(provider_check(provider, status));
        }

        let mut handshake_checks = Vec::with_capacity(registrations.len());
        for (client, registration) in &registrations {
            handshake_checks.push(
                check_handshake(
                    *client,
                    registration.as_ref(),
                    any_provider_found,
                    activity.clone(),
                )
                .await,
            );
        }
        drop(activity);
        let _ = tokio::time::timeout(Duration::from_secs(2), finished.recv()).await;
        (cli_checks, handshake_checks)
    });
    runtime.shutdown_timeout(Duration::from_millis(500));

    checks.push(state_check);
    checks.extend(cli_checks);
    checks.extend(handshake_checks);

    let ok = checks.iter().all(|check| check.status != Status::Fail);
    let report = Report {
        schema_version: REPORT_SCHEMA_VERSION,
        ok,
        checks,
    };

    let output = if json_output {
        let mut text = serde_json::to_string_pretty(&report).expect("doctor report serializes");
        text.push('\n');
        text
    } else {
        let mut text = String::new();
        for check in &report.checks {
            text.push_str(&format!(
                "[{}] {}: {}\n",
                check.status.label(),
                check.name,
                check.message
            ));
        }
        text
    };

    (ok, output)
}
