//! Scenarios (a)-(g) for Part 1 (stubbed) and Part 2 (`--real-models`, a and g only).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::fleet::{Binaries, ProviderKind, ServerKind, StubFleet, real_server_command};
use crate::mcp::{McpServer, StartOutcome, result_text};
use crate::proc_check::{cleanup_marker, pgrep_marker};
use crate::stats::{Summary, round2};

const CODEX_FINISHED: &str = include_str!("../../tests/fixtures/codex/finished.jsonl");
const AGY_FINISHED: &str = include_str!("../../tests/fixtures/antigravity/finished.json");
const CODEX_EXPECTED_ANSWER: &str = "The two paragraphs agree.";
const AGY_EXPECTED_ANSWER: &str = "The fold aligns with the mark.";
const CODEX_THREAD: &str = "eeb3a731-d53a-4444-8a6a-4d9d89e76073";
const AGY_THREAD: &str = "paper-crane";

const CODEX_MODEL: &str = "gpt-6-luna";
const CODEX_EFFORT: &str = "low";
const AGY_MODEL: &str = "gemini-3.8-flash-low";

/// Capture limit enforced per stream by Proxenos (4 MiB).
const PROXENOS_CAPTURE_LIMIT: usize = 4 << 20;

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub runs: usize,
    pub timeout_ms: u64,
    pub concurrency: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxenosProbe {
    pub tools_listed: Vec<String>,
    pub has_await: bool,
    pub has_cancel: bool,
    pub has_wait_param: bool,
    pub has_usage: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TargetKey {
    pub server: String,
    pub provider: String,
}

impl TargetKey {
    fn new(server: ServerKind, provider: ProviderKind) -> Self {
        Self {
            server: server.id().into(),
            provider: provider.id().into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AdvisoryTrial {
    pub wall_ms: f64,
    pub result_bytes: usize,
    pub frame_bytes: usize,
    pub answer_intact: bool,
    pub is_error: bool,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdvisoryResult {
    pub target: TargetKey,
    pub trials: Vec<AdvisoryTrial>,
    pub wall_ms: Option<Summary>,
    pub result_bytes: Option<Summary>,
    pub frame_bytes: Option<Summary>,
    pub intact_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct TimeoutTrial {
    pub wall_ms: f64,
    pub is_error: bool,
    pub structured_kind: Option<String>,
    pub reported_text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TimeoutResult {
    pub target: TargetKey,
    pub requested_timeout_ms: u64,
    pub trials: Vec<TimeoutTrial>,
    pub wall_ms: Option<Summary>,
    pub structured_kind_sample: Option<String>,
    pub reported_text_sample: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CancelTrial {
    pub ack_ms: f64,
    pub cancel_response_received: bool,
    pub cancel_response_ms: Option<f64>,
    pub probe_ack_ms: f64,
    pub response_summary: String,
    pub pre_cli_alive: bool,
    pub pre_gc_alive: bool,
    pub cli_gone_2s: bool,
    pub gc_gone_2s: bool,
    pub cli_gone_10s: bool,
    pub gc_gone_10s: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CancelResult {
    pub target: TargetKey,
    pub trials: Vec<CancelTrial>,
    pub ack_ms: Option<Summary>,
    pub cancel_response_count: usize,
    pub pre_verified_count: usize,
    pub cli_gone_2s_count: usize,
    pub gc_gone_2s_count: usize,
    pub cli_gone_10s_count: usize,
    pub gc_gone_10s_count: usize,
    pub response_summary_sample: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SigkillTrial {
    pub pre_cli_alive: bool,
    pub pre_gc_alive: bool,
    pub cli_survivors_2s: usize,
    pub gc_survivors_2s: usize,
    pub cli_survivors_10s: usize,
    pub gc_survivors_10s: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SigkillResult {
    pub target: TargetKey,
    pub trials: Vec<SigkillTrial>,
    pub pre_verified_count: usize,
    pub cli_survived_2s_runs: usize,
    pub gc_survived_2s_runs: usize,
    pub cli_survived_10s_runs: usize,
    pub gc_survived_10s_runs: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct StdinEofTrial {
    pub exit_ms: f64,
    pub exit_code: Option<i32>,
    pub drain_response_received: bool,
    pub pre_cli_alive: bool,
    pub pre_gc_alive: bool,
    pub cli_survivors_at_exit: usize,
    pub gc_survivors_at_exit: usize,
    pub cli_survivors_post_exit_2s: usize,
    pub gc_survivors_post_exit_2s: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct StdinEofResult {
    pub target: TargetKey,
    pub trials: Vec<StdinEofTrial>,
    pub exit_ms: Option<Summary>,
    pub clean_exit_count: usize,
    pub drain_response_count: usize,
    pub pre_verified_count: usize,
    pub cli_survived_exit_runs: usize,
    pub gc_survived_exit_runs: usize,
    pub cli_survived_post_exit_2s_runs: usize,
    pub gc_survived_post_exit_2s_runs: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FailureTrial {
    pub wall_ms: f64,
    pub channel: String,
    pub is_error: bool,
    pub structured_kind: Option<String>,
    pub exit_code: Option<i32>,
    pub message: String,
    pub diagnostic: Option<String>,
    pub stdout_total: Option<u64>,
    pub stdout_captured: Option<u64>,
    pub result_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FailureCaseResult {
    pub case: String,
    pub target: TargetKey,
    pub trials: Vec<FailureTrial>,
    pub wall_ms: Option<Summary>,
    pub channel_sample: String,
    pub is_error_count: usize,
    pub structured_kind_sample: Option<String>,
    pub exit_code_sample: Option<i32>,
    pub message_sample: String,
    pub diagnostic_sample: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplyTrial {
    pub start_wall_ms: f64,
    pub bare_reply_wall_ms: f64,
    pub bare_reply_succeeded: bool,
    pub bare_reply_error: Option<String>,
    pub model_carried: bool,
    pub effort_carried: Option<bool>,
    pub access_carried: bool,
    pub cwd_carried: bool,
    pub explicit_model_reply_wall_ms: Option<f64>,
    pub explicit_model_reply_succeeded: Option<bool>,
    pub explicit_model_access_carried: Option<bool>,
    pub explicit_model_cwd_carried: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplyResult {
    pub target: TargetKey,
    pub trials: Vec<ReplyTrial>,
    pub bare_reply_wall_ms: Option<Summary>,
    pub bare_reply_succeeded_count: usize,
    pub model_carried_count: usize,
    pub effort_carried_count: Option<usize>,
    pub access_carried_count: usize,
    pub cwd_carried_count: usize,
    pub bare_reply_error_sample: Option<String>,
    pub explicit_model_reply_wall_ms: Option<Summary>,
    pub explicit_model_reply_succeeded_count: Option<usize>,
    pub explicit_model_access_carried_count: Option<usize>,
    pub explicit_model_cwd_carried_count: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Part1Results {
    pub scenario_a_advisory: Vec<AdvisoryResult>,
    pub scenario_b_timeout: Vec<TimeoutResult>,
    pub scenario_c_cancel: Vec<CancelResult>,
    pub scenario_d_sigkill: Vec<SigkillResult>,
    pub scenario_e_stdin_eof: Vec<StdinEofResult>,
    pub scenario_f_failures: Vec<FailureCaseResult>,
    pub scenario_g_reply: Vec<ReplyResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RealReplyTrial {
    pub start_wall_ms: f64,
    pub start_succeeded: bool,
    pub start_canary_intact: bool,
    pub reply_wall_ms: Option<f64>,
    pub reply_succeeded: bool,
    pub reply_canary_intact: bool,
    pub model_passed_on_reply: Option<String>,
    pub thread_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RealReplyResult {
    pub target: TargetKey,
    pub trials: Vec<RealReplyTrial>,
    pub start_wall_ms: Option<Summary>,
    pub reply_wall_ms: Option<Summary>,
    pub start_succeeded_count: usize,
    pub reply_succeeded_count: usize,
    pub reply_canary_intact_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Part2Results {
    pub scenario_a_advisory: Vec<AdvisoryResult>,
    pub scenario_g_reply: Vec<RealReplyResult>,
}

/// Probe `tools/list` on the target Proxenos binary with both stub CLIs present so the
/// report automatically reflects whether step-4 tools (`await`, `cancel`, `wait`) exist.
pub fn probe_proxenos(bins: &Binaries) -> Result<ProxenosProbe, String> {
    let fleet = StubFleet::new(&bins.stub_bin, &["codex", "agy"])?;
    let cmd = fleet.server_command(bins, ServerKind::Proxenos, ProviderKind::Codex);
    let mut server = match McpServer::start_and_initialize(cmd, Duration::from_secs(15))? {
        StartOutcome::Ready(server) => server,
        StartOutcome::ExitedAtStartup {
            exit_code, stderr, ..
        } => {
            return Err(format!(
                "proxenos exited during capability probe (code {exit_code:?}): {stderr}"
            ));
        }
    };
    let listed = server.request(1, "tools/list", json!({}), Duration::from_secs(15))?;
    let _ = server.finish(Duration::from_secs(10));
    let tools = listed.frame["result"]["tools"]
        .as_array()
        .ok_or("tools/list returned no tools array")?;
    let mut names: Vec<String> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|name| !name.starts_with("test_"))
        .map(str::to_owned)
        .collect();
    names.sort();
    let has_wait_param = tools.iter().any(|t| {
        t["name"].as_str() == Some("codex")
            && t["inputSchema"]["properties"]
                .as_object()
                .is_some_and(|props| props.contains_key("wait"))
    });
    Ok(ProxenosProbe {
        has_await: names.iter().any(|n| n == "await"),
        has_cancel: names.iter().any(|n| n == "cancel"),
        has_wait_param,
        has_usage: names.iter().any(|n| n == "usage"),
        tools_listed: names,
    })
}

pub fn run_part1(
    bins: &Binaries,
    config: &RunConfig,
    only: Option<&[String]>,
) -> Result<Part1Results, String> {
    let want = |code: &str| {
        only.is_none_or(|list| list.iter().any(|item| item.eq_ignore_ascii_case(code)))
    };

    let targets = all_targets();

    let mut scenario_a_advisory = Vec::new();
    if want("a") {
        eprintln!(
            "[comparison] Running scenario a (advisory turn, N={})...",
            config.runs
        );
        for &(server, provider) in &targets {
            scenario_a_advisory.push(run_scenario_a(bins, server, provider, config.runs)?);
        }
    }

    let mut scenario_b_timeout = Vec::new();
    if want("b") {
        eprintln!(
            "[comparison] Running scenario b (timeout {} ms, N={}, concurrency={})...",
            config.timeout_ms, config.runs, config.concurrency
        );
        for &(server, provider) in &targets {
            scenario_b_timeout.push(run_scenario_b(bins, server, provider, config)?);
        }
    }

    let mut scenario_c_cancel = Vec::new();
    if want("c") {
        eprintln!(
            "[comparison] Running scenario c (client cancellation, N={}, concurrency={})...",
            config.runs, config.concurrency
        );
        for &(server, provider) in &targets {
            scenario_c_cancel.push(run_scenario_c(bins, server, provider, config)?);
        }
    }

    let mut scenario_d_sigkill = Vec::new();
    if want("d") {
        eprintln!(
            "[comparison] Running scenario d (server SIGKILL, N={}, concurrency={})...",
            config.runs, config.concurrency
        );
        for &(server, provider) in &targets {
            scenario_d_sigkill.push(run_scenario_d(bins, server, provider, config)?);
        }
    }

    let mut scenario_e_stdin_eof = Vec::new();
    if want("e") {
        eprintln!(
            "[comparison] Running scenario e (server stdin EOF, N={}, concurrency={})...",
            config.runs, config.concurrency
        );
        for &(server, provider) in &targets {
            scenario_e_stdin_eof.push(run_scenario_e(bins, server, provider, config)?);
        }
    }

    let mut scenario_f_failures = Vec::new();
    if want("f") {
        eprintln!(
            "[comparison] Running scenario f (injected failures: 4 cases, N={})...",
            config.runs
        );
        for &(server, provider) in &targets {
            scenario_f_failures.extend(run_scenario_f(bins, server, provider, config.runs)?);
        }
    }

    let mut scenario_g_reply = Vec::new();
    if want("g") {
        eprintln!(
            "[comparison] Running scenario g (reply after start, N={})...",
            config.runs
        );
        for &(server, provider) in &targets {
            scenario_g_reply.push(run_scenario_g(bins, server, provider, config.runs)?);
        }
    }

    Ok(Part1Results {
        scenario_a_advisory,
        scenario_b_timeout,
        scenario_c_cancel,
        scenario_d_sigkill,
        scenario_e_stdin_eof,
        scenario_f_failures,
        scenario_g_reply,
    })
}

/// Part 2: runs scenarios a and g against REAL CLIs and cheap models (never called unless
/// both `--real-models` and `PROXENOS_COMPARISON_REAL=yes` are present).
pub fn run_part2(bins: &Binaries, runs: usize) -> Result<Part2Results, String> {
    let targets = all_targets();
    let mut scenario_a_advisory = Vec::new();
    let mut scenario_g_reply = Vec::new();

    for &(server, provider) in &targets {
        eprintln!(
            "[comparison/real] Running scenario a for {}/{} (N={})...",
            server.id(),
            provider.id(),
            runs
        );
        scenario_a_advisory.push(run_real_scenario_a(bins, server, provider, runs)?);
    }

    for &(server, provider) in &targets {
        eprintln!(
            "[comparison/real] Running scenario g for {}/{} (N={})...",
            server.id(),
            provider.id(),
            runs
        );
        scenario_g_reply.push(run_real_scenario_g(bins, server, provider, runs)?);
    }

    Ok(Part2Results {
        scenario_a_advisory,
        scenario_g_reply,
    })
}

fn all_targets() -> [(ServerKind, ProviderKind); 4] {
    [
        (ServerKind::Proxenos, ProviderKind::Codex),
        (ServerKind::ClaudeDelegator, ProviderKind::Codex),
        (ServerKind::Proxenos, ProviderKind::Antigravity),
        (ServerKind::ClaudeDelegator, ProviderKind::Antigravity),
    ]
}

fn fixture_for(provider: ProviderKind) -> (&'static str, &'static str) {
    match provider {
        ProviderKind::Codex => (CODEX_FINISHED, CODEX_EXPECTED_ANSWER),
        ProviderKind::Antigravity => (AGY_FINISHED, AGY_EXPECTED_ANSWER),
    }
}

fn start_args(
    server: ServerKind,
    provider: ProviderKind,
    prompt: &str,
    cwd: &Path,
    timeout_ms: Option<u64>,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("prompt".into(), json!(prompt));
    map.insert("cwd".into(), json!(cwd));
    match server {
        ServerKind::Proxenos => {
            map.insert("mode".into(), json!("advisory"));
            map.insert("access".into(), json!("read-only"));
            map.insert(
                "instructions".into(),
                json!("Advise only; leave files unchanged."),
            );
            match provider {
                ProviderKind::Codex => {
                    map.insert("model".into(), json!(CODEX_MODEL));
                    map.insert("effort".into(), json!(CODEX_EFFORT));
                }
                ProviderKind::Antigravity => {
                    map.insert("model".into(), json!(AGY_MODEL));
                }
            }
            if let Some(ms) = timeout_ms {
                map.insert("timeout_ms".into(), json!(ms));
            }
        }
        ServerKind::ClaudeDelegator => {
            map.insert("sandbox".into(), json!("read-only"));
            map.insert(
                "developer-instructions".into(),
                json!("Advise only; leave files unchanged."),
            );
            match provider {
                ProviderKind::Codex => {
                    map.insert("model".into(), json!(CODEX_MODEL));
                    map.insert("effort".into(), json!(CODEX_EFFORT));
                }
                ProviderKind::Antigravity => {
                    map.insert("model".into(), json!(AGY_MODEL));
                }
            }
            if let Some(ms) = timeout_ms {
                map.insert("timeout".into(), json!(ms));
            }
        }
    }
    Value::Object(map)
}

fn open_ready_server(
    fleet: &StubFleet,
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
) -> Result<McpServer, String> {
    let cmd = fleet.server_command(bins, server, provider);
    let mut mcp = match McpServer::start_and_initialize(cmd, Duration::from_secs(15))? {
        StartOutcome::Ready(mcp) => mcp,
        StartOutcome::ExitedAtStartup {
            exit_code, stderr, ..
        } => {
            return Err(format!(
                "{}/{} exited at startup (code {exit_code:?}): {stderr}",
                server.id(),
                provider.id()
            ));
        }
    };
    // Complete discovery via tools/list just as a real MCP client does at session start.
    mcp.request(1, "tools/list", json!({}), Duration::from_secs(15))?;
    Ok(mcp)
}

fn parse_success_content(server: ServerKind, raw_text: &str) -> (Option<String>, Option<String>) {
    let Ok(parsed) = serde_json::from_str::<Value>(raw_text) else {
        return (None, None);
    };
    match server {
        ServerKind::Proxenos => (
            parsed["reported"]["content"].as_str().map(str::to_owned),
            parsed["reported"]["thread_id"].as_str().map(str::to_owned),
        ),
        ServerKind::ClaudeDelegator => (
            parsed["content"].as_str().map(str::to_owned),
            parsed["threadId"].as_str().map(str::to_owned),
        ),
    }
}

struct ParsedFailure {
    structured_kind: Option<String>,
    exit_code: Option<i32>,
    message: String,
    diagnostic: Option<String>,
    stdout_total: Option<u64>,
    stdout_captured: Option<u64>,
}

fn parse_tool_failure(server: ServerKind, raw_text: &str, tmp_dir: &Path) -> ParsedFailure {
    let sanitized = sanitize_temp_paths(raw_text, tmp_dir);
    match server {
        ServerKind::Proxenos => {
            if let Ok(env) = serde_json::from_str::<Value>(raw_text) {
                let failure = if env["observed"]["failure"].is_object() {
                    &env["observed"]["failure"]
                } else {
                    &env["reported"]["failure"]
                };
                let kind = failure["kind"].as_str().map(str::to_owned);
                let message = failure["message"]
                    .as_str()
                    .map(|m| sanitize_temp_paths(m, tmp_dir))
                    .unwrap_or_else(|| sanitized.clone());
                let attempt = &env["observed"]["attempts"][0];
                let exit_code = attempt["exit_code"].as_i64().map(|v| v as i32);
                let stdout_total = attempt["stdout_bytes"].as_u64();
                let stdout_captured = attempt["stdout_captured"].as_u64();
                let diagnostic = env["reported"]["diagnostic"]
                    .as_str()
                    .map(|d| sanitize_temp_paths(d.trim(), tmp_dir));
                return ParsedFailure {
                    structured_kind: kind,
                    exit_code,
                    message,
                    diagnostic,
                    stdout_total,
                    stdout_captured,
                };
            }
            ParsedFailure {
                structured_kind: None,
                exit_code: None,
                message: sanitized,
                diagnostic: None,
                stdout_total: None,
                stdout_captured: None,
            }
        }
        ServerKind::ClaudeDelegator => ParsedFailure {
            structured_kind: None,
            exit_code: None,
            message: sanitized,
            diagnostic: None,
            stdout_total: None,
            stdout_captured: None,
        },
    }
}

fn sanitize_temp_paths(text: &str, tmp_dir: &Path) -> String {
    let dir = tmp_dir.to_string_lossy();
    let mut replaced = if !dir.is_empty() {
        text.replace(dir.as_ref(), "<tmp>")
    } else {
        text.to_owned()
    };
    // Collapse per-PID log filenames anywhere in the message so samples stay deterministic.
    const MARKER: &str = "claude-delegator-agy-";
    while let Some(start) = replaced.find(MARKER) {
        let after = start + MARKER.len();
        let Some(end_rel) = replaced[after..].find(".log") else {
            break;
        };
        let suffix = &replaced[after..after + end_rel];
        if suffix == "<pid>-<seq>" {
            break;
        }
        replaced.replace_range(after..after + end_rel, "<pid>-<seq>");
    }
    replaced
}

fn run_scenario_a(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    runs: usize,
) -> Result<AdvisoryResult, String> {
    let fleet = StubFleet::new(&bins.stub_bin, &[provider.cli_name()])?;
    let mut mcp = open_ready_server(&fleet, bins, server, provider)?;
    let (fixture, expected) = fixture_for(provider);
    let tool = provider.start_tool(server);
    let prompt = "Compare the two paragraphs and advise.";
    let args = start_args(server, provider, prompt, &fleet.workspace_dir, None);

    let mut trials = Vec::with_capacity(runs);
    for i in 0..runs {
        fleet.script(provider.cli_name(), &json!([{"stdout": fixture}]))?;
        let id = 10 + i as i64;
        let started = Instant::now();
        mcp.start_call(id, tool, args.clone())?;
        let timed = mcp.wait_response(id, Instant::now() + Duration::from_secs(15))?;
        let wall_ms = round2(timed.received_at.duration_since(started).as_secs_f64() * 1000.0);
        let is_error = timed.frame["result"]["isError"] == true;
        let text = result_text(&timed.frame).unwrap_or("");
        let (content, thread_id) = parse_success_content(server, text);
        let answer_intact = !is_error && content.as_deref() == Some(expected);
        trials.push(AdvisoryTrial {
            wall_ms,
            result_bytes: text.len(),
            frame_bytes: timed.raw_bytes,
            answer_intact,
            is_error,
            thread_id,
        });
    }
    let _ = mcp.finish(Duration::from_secs(10));

    let wall_samples: Vec<f64> = trials.iter().map(|t| t.wall_ms).collect();
    let result_samples: Vec<f64> = trials.iter().map(|t| t.result_bytes as f64).collect();
    let frame_samples: Vec<f64> = trials.iter().map(|t| t.frame_bytes as f64).collect();
    let intact_count = trials.iter().filter(|t| t.answer_intact).count();

    Ok(AdvisoryResult {
        target: TargetKey::new(server, provider),
        trials,
        wall_ms: Summary::from_samples(&wall_samples),
        result_bytes: Summary::from_samples(&result_samples),
        frame_bytes: Summary::from_samples(&frame_samples),
        intact_count,
    })
}

/// Run `runs` independent trials with bounded concurrency and staggered launch to avoid
/// overlapping wakeups on sleeping-stub scenarios (b, c, d, e).
fn run_parallel_trials<T: Send + 'static>(
    runs: usize,
    concurrency: usize,
    stagger: Duration,
    f: impl Fn(usize) -> Result<T, String> + Send + Sync + 'static,
) -> Result<Vec<T>, String> {
    let workers = concurrency.max(1).min(runs.max(1));
    if workers == 1 {
        return (0..runs).map(f).collect();
    }
    let f = std::sync::Arc::new(f);
    let mut results: Vec<Option<T>> = (0..runs).map(|_| None).collect();
    let (tx, rx) = mpsc::channel::<(usize, Result<T, String>)>();
    let mut next_to_start = 0;
    let mut in_flight = 0;

    while next_to_start < runs || in_flight > 0 {
        while in_flight < workers && next_to_start < runs {
            let idx = next_to_start;
            next_to_start += 1;
            in_flight += 1;
            if idx > 0 {
                thread::sleep(stagger);
            }
            let tx = tx.clone();
            let f = std::sync::Arc::clone(&f);
            thread::spawn(move || {
                let res = f(idx);
                let _ = tx.send((idx, res));
            });
        }
        let (idx, res) = rx
            .recv()
            .map_err(|err| format!("worker channel closed unexpectedly: {err}"))?;
        in_flight -= 1;
        results[idx] = Some(res?);
    }

    Ok(results.into_iter().map(|item| item.unwrap()).collect())
}

fn run_scenario_b(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    config: &RunConfig,
) -> Result<TimeoutResult, String> {
    let bins = bins.clone();
    let timeout_ms = config.timeout_ms;
    let trials = run_parallel_trials(
        config.runs,
        config.concurrency,
        Duration::from_millis(250),
        move |_| {
            let fleet = StubFleet::new(&bins.stub_bin, &[provider.cli_name()])?;
            let mut mcp = open_ready_server(&fleet, &bins, server, provider)?;
            fleet.script(provider.cli_name(), &json!([{"sleep_ms": 60_000}]))?;
            let tool = provider.start_tool(server);
            let args = start_args(
                server,
                provider,
                "Wait indefinitely.",
                &fleet.workspace_dir,
                Some(timeout_ms),
            );
            let started = Instant::now();
            mcp.start_call(10, tool, args)?;
            let wait = Duration::from_millis(timeout_ms) + Duration::from_secs(20);
            let timed = mcp.wait_response(10, Instant::now() + wait)?;
            let wall_ms = round2(timed.received_at.duration_since(started).as_secs_f64() * 1000.0);
            let is_error = timed.frame["result"]["isError"] == true;
            let raw_text = result_text(&timed.frame).unwrap_or("");
            let parsed = parse_tool_failure(server, raw_text, &fleet.tmp_dir);
            let _ = mcp.finish(Duration::from_secs(10));
            Ok(TimeoutTrial {
                wall_ms,
                is_error,
                structured_kind: parsed.structured_kind,
                reported_text: parsed.message,
            })
        },
    )?;

    let wall_samples: Vec<f64> = trials.iter().map(|t| t.wall_ms).collect();
    let structured_kind_sample = trials.first().and_then(|t| t.structured_kind.clone());
    let reported_text_sample = trials
        .first()
        .map(|t| t.reported_text.clone())
        .unwrap_or_default();

    Ok(TimeoutResult {
        target: TargetKey::new(server, provider),
        requested_timeout_ms: timeout_ms,
        trials,
        wall_ms: Summary::from_samples(&wall_samples),
        structured_kind_sample,
        reported_text_sample,
    })
}

fn wait_for_file(path: &Path, limit: Duration) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {}", path.display()));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn sleep_until_offset(origin: Instant, offset: Duration) {
    let target = origin + offset;
    let now = Instant::now();
    if target > now {
        thread::sleep(target - now);
    }
}

struct HangingSetup {
    fleet: StubFleet,
    mcp: McpServer,
    cli_marker: String,
    gc_marker: String,
    pre_cli_alive: bool,
    pre_gc_alive: bool,
}

fn start_hanging_turn(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    call_id: i64,
) -> Result<HangingSetup, String> {
    let fleet = StubFleet::new(&bins.stub_bin, &[provider.cli_name()])?;
    let mut mcp = open_ready_server(&fleet, bins, server, provider)?;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let cli_marker = format!("PROXENOS_CMP_{uid}_CLI");
    let gc_marker = format!("PROXENOS_CMP_{uid}_GC");
    let gc_path = fleet.root.join(&gc_marker);
    let ready_path = fleet.root.join("turn.ready");
    fleet.script(
        provider.cli_name(),
        &json!([{
            "grandchild": gc_path,
            "ready": ready_path,
            "sleep_ms": 60_000
        }]),
    )?;
    let prompt = format!("Hang until stopped [{cli_marker}]");
    let args = start_args(
        server,
        provider,
        &prompt,
        &fleet.workspace_dir,
        Some(60_000),
    );
    mcp.start_call(call_id, provider.start_tool(server), args)?;
    wait_for_file(&ready_path, Duration::from_secs(15))?;
    let pre_cli_alive = !pgrep_marker(&cli_marker)?.is_empty();
    let pre_gc_alive = !pgrep_marker(&gc_marker)?.is_empty();
    Ok(HangingSetup {
        fleet,
        mcp,
        cli_marker,
        gc_marker,
        pre_cli_alive,
        pre_gc_alive,
    })
}

fn run_scenario_c(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    config: &RunConfig,
) -> Result<CancelResult, String> {
    let bins = bins.clone();
    let trials = run_parallel_trials(
        config.runs,
        config.concurrency,
        Duration::from_millis(250),
        move |_| {
            let mut setup = start_hanging_turn(&bins, server, provider, 10)?;
            let cli_marker = setup.cli_marker.clone();
            let gc_marker = setup.gc_marker.clone();

            let cancel_sent = Instant::now();
            setup.mcp.send_cancelled(10, "comparison-cancel")?;
            // Immediately send a lightweight tools/list probe so servers that follow the
            // MCP rule of omitting responses to cancelled requests still produce a clean
            // acknowledgement timestamp.
            setup.mcp.send(&json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/list",
                "params": {}
            }))?;

            let mut cancel_response_ms = None;
            let mut cancel_response_text = None;
            let mut probe_ack_ms = None;

            let ack_deadline = cancel_sent + Duration::from_secs(15);
            while probe_ack_ms.is_none()
                || (server == ServerKind::ClaudeDelegator && cancel_response_ms.is_none())
            {
                let Some(timed) = setup.mcp.next_frame_until(ack_deadline)? else {
                    break;
                };
                let elapsed =
                    round2(timed.received_at.duration_since(cancel_sent).as_secs_f64() * 1000.0);
                if timed.frame.get("id") == Some(&json!(10)) {
                    cancel_response_ms = Some(elapsed);
                    cancel_response_text = result_text(&timed.frame)
                        .map(|t| sanitize_temp_paths(t, &setup.fleet.tmp_dir))
                        .or_else(|| Some(timed.frame.to_string()));
                } else if timed.frame.get("id") == Some(&json!(11)) {
                    probe_ack_ms = Some(elapsed);
                }
            }

            // Keep server alive through t+2s and t+10s, draining any late frames.
            let t2 = cancel_sent + Duration::from_secs(2);
            while Instant::now() < t2 {
                if let Some(timed) = setup.mcp.next_frame_until(t2)?
                    && timed.frame.get("id") == Some(&json!(10))
                    && cancel_response_ms.is_none()
                {
                    let elapsed = round2(
                        timed.received_at.duration_since(cancel_sent).as_secs_f64() * 1000.0,
                    );
                    cancel_response_ms = Some(elapsed);
                    cancel_response_text = result_text(&timed.frame)
                        .map(|t| sanitize_temp_paths(t, &setup.fleet.tmp_dir));
                }
            }
            let cli_gone_2s = pgrep_marker(&cli_marker)?.is_empty();
            let gc_gone_2s = pgrep_marker(&gc_marker)?.is_empty();

            let t10 = cancel_sent + Duration::from_secs(10);
            while Instant::now() < t10 {
                if let Some(timed) = setup.mcp.next_frame_until(t10)?
                    && timed.frame.get("id") == Some(&json!(10))
                    && cancel_response_ms.is_none()
                {
                    let elapsed = round2(
                        timed.received_at.duration_since(cancel_sent).as_secs_f64() * 1000.0,
                    );
                    cancel_response_ms = Some(elapsed);
                    cancel_response_text = result_text(&timed.frame)
                        .map(|t| sanitize_temp_paths(t, &setup.fleet.tmp_dir));
                }
            }
            let cli_gone_10s = pgrep_marker(&cli_marker)?.is_empty();
            let gc_gone_10s = pgrep_marker(&gc_marker)?.is_empty();

            let _ = setup.mcp.finish(Duration::from_secs(10));
            cleanup_marker(&cli_marker);
            cleanup_marker(&gc_marker);

            let probe_ms = probe_ack_ms.ok_or("tools/list probe did not reply after cancel")?;
            let response_summary = match &cancel_response_text {
                Some(text) => format!("error result: {text}"),
                None => {
                    "no response to cancelled request (per MCP spec); tools/list probe acked".into()
                }
            };
            Ok(CancelTrial {
                ack_ms: cancel_response_ms.unwrap_or(probe_ms),
                cancel_response_received: cancel_response_ms.is_some(),
                cancel_response_ms,
                probe_ack_ms: probe_ms,
                response_summary,
                pre_cli_alive: setup.pre_cli_alive,
                pre_gc_alive: setup.pre_gc_alive,
                cli_gone_2s,
                gc_gone_2s,
                cli_gone_10s,
                gc_gone_10s,
            })
        },
    )?;

    let ack_samples: Vec<f64> = trials.iter().map(|t| t.ack_ms).collect();
    let response_summary_sample = trials
        .first()
        .map(|t| t.response_summary.clone())
        .unwrap_or_default();

    Ok(CancelResult {
        target: TargetKey::new(server, provider),
        ack_ms: Summary::from_samples(&ack_samples),
        cancel_response_count: trials.iter().filter(|t| t.cancel_response_received).count(),
        pre_verified_count: trials
            .iter()
            .filter(|t| t.pre_cli_alive && t.pre_gc_alive)
            .count(),
        cli_gone_2s_count: trials.iter().filter(|t| t.cli_gone_2s).count(),
        gc_gone_2s_count: trials.iter().filter(|t| t.gc_gone_2s).count(),
        cli_gone_10s_count: trials.iter().filter(|t| t.cli_gone_10s).count(),
        gc_gone_10s_count: trials.iter().filter(|t| t.gc_gone_10s).count(),
        response_summary_sample,
        trials,
    })
}

fn run_scenario_d(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    config: &RunConfig,
) -> Result<SigkillResult, String> {
    let bins = bins.clone();
    let trials = run_parallel_trials(
        config.runs,
        config.concurrency,
        Duration::from_millis(200),
        move |_| {
            let mut setup = start_hanging_turn(&bins, server, provider, 10)?;
            let cli_marker = setup.cli_marker.clone();
            let gc_marker = setup.gc_marker.clone();

            let killed_at = Instant::now();
            setup.mcp.sigkill()?;

            sleep_until_offset(killed_at, Duration::from_secs(2));
            let cli_survivors_2s = pgrep_marker(&cli_marker)?.len();
            let gc_survivors_2s = pgrep_marker(&gc_marker)?.len();

            sleep_until_offset(killed_at, Duration::from_secs(10));
            let cli_survivors_10s = pgrep_marker(&cli_marker)?.len();
            let gc_survivors_10s = pgrep_marker(&gc_marker)?.len();

            // Clean up any surviving test stubs after the 10-second measurement.
            cleanup_marker(&cli_marker);
            cleanup_marker(&gc_marker);

            Ok(SigkillTrial {
                pre_cli_alive: setup.pre_cli_alive,
                pre_gc_alive: setup.pre_gc_alive,
                cli_survivors_2s,
                gc_survivors_2s,
                cli_survivors_10s,
                gc_survivors_10s,
            })
        },
    )?;

    Ok(SigkillResult {
        target: TargetKey::new(server, provider),
        pre_verified_count: trials
            .iter()
            .filter(|t| t.pre_cli_alive && t.pre_gc_alive)
            .count(),
        cli_survived_2s_runs: trials.iter().filter(|t| t.cli_survivors_2s > 0).count(),
        gc_survived_2s_runs: trials.iter().filter(|t| t.gc_survivors_2s > 0).count(),
        cli_survived_10s_runs: trials.iter().filter(|t| t.cli_survivors_10s > 0).count(),
        gc_survived_10s_runs: trials.iter().filter(|t| t.gc_survivors_10s > 0).count(),
        trials,
    })
}

fn run_scenario_e(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    config: &RunConfig,
) -> Result<StdinEofResult, String> {
    let bins = bins.clone();
    let trials = run_parallel_trials(
        config.runs,
        config.concurrency,
        Duration::from_millis(250),
        move |_| {
            let mut setup = start_hanging_turn(&bins, server, provider, 10)?;
            let cli_marker = setup.cli_marker.clone();
            let gc_marker = setup.gc_marker.clone();

            let closed_at = Instant::now();
            setup.mcp.close_input();
            let status = setup.mcp.wait_exit(Duration::from_secs(20))?;
            let exit_ms = round2(closed_at.elapsed().as_secs_f64() * 1000.0);
            let drained = setup.mcp.drain_frames();
            let drain_response_received = drained
                .iter()
                .any(|f| f.frame.get("id") == Some(&json!(10)));

            let exited_at = Instant::now();
            let cli_survivors_at_exit = pgrep_marker(&cli_marker)?.len();
            let gc_survivors_at_exit = pgrep_marker(&gc_marker)?.len();

            sleep_until_offset(exited_at, Duration::from_secs(2));
            let cli_survivors_post_exit_2s = pgrep_marker(&cli_marker)?.len();
            let gc_survivors_post_exit_2s = pgrep_marker(&gc_marker)?.len();

            cleanup_marker(&cli_marker);
            cleanup_marker(&gc_marker);

            Ok(StdinEofTrial {
                exit_ms,
                exit_code: status.code(),
                drain_response_received,
                pre_cli_alive: setup.pre_cli_alive,
                pre_gc_alive: setup.pre_gc_alive,
                cli_survivors_at_exit,
                gc_survivors_at_exit,
                cli_survivors_post_exit_2s,
                gc_survivors_post_exit_2s,
            })
        },
    )?;

    let exit_samples: Vec<f64> = trials.iter().map(|t| t.exit_ms).collect();

    Ok(StdinEofResult {
        target: TargetKey::new(server, provider),
        exit_ms: Summary::from_samples(&exit_samples),
        clean_exit_count: trials.iter().filter(|t| t.exit_code == Some(0)).count(),
        drain_response_count: trials.iter().filter(|t| t.drain_response_received).count(),
        pre_verified_count: trials
            .iter()
            .filter(|t| t.pre_cli_alive && t.pre_gc_alive)
            .count(),
        cli_survived_exit_runs: trials
            .iter()
            .filter(|t| t.cli_survivors_at_exit > 0)
            .count(),
        gc_survived_exit_runs: trials.iter().filter(|t| t.gc_survivors_at_exit > 0).count(),
        cli_survived_post_exit_2s_runs: trials
            .iter()
            .filter(|t| t.cli_survivors_post_exit_2s > 0)
            .count(),
        gc_survived_post_exit_2s_runs: trials
            .iter()
            .filter(|t| t.gc_survivors_post_exit_2s > 0)
            .count(),
        trials,
    })
}

fn build_oversized_stdout(provider: ProviderKind) -> String {
    // Emit >4 MiB (4.5 MiB) of non-terminal stdout lines before the valid terminal payload.
    let target_padding = PROXENOS_CAPTURE_LIMIT + (512 << 10);
    let mut out = String::with_capacity(target_padding + 2048);
    match provider {
        ProviderKind::Codex => {
            let chunk = format!(
                "{}\n",
                json!({
                    "type": "item.completed",
                    "item": {
                        "type": "command_execution",
                        "id": "pad",
                        "aggregated_output": "x".repeat(4000)
                    }
                })
            );
            while out.len() < target_padding {
                out.push_str(&chunk);
            }
            out.push_str(CODEX_FINISHED);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        ProviderKind::Antigravity => {
            let chunk = format!("diagnostic {}\n", "y".repeat(4000));
            while out.len() < target_padding {
                out.push_str(&chunk);
            }
            // Compact single-line JSON so both parsers would accept it if un-truncated.
            let compact: Value = serde_json::from_str(AGY_FINISHED).expect("valid agy fixture");
            out.push_str(&compact.to_string());
            out.push('\n');
        }
    }
    out
}

fn run_scenario_f(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    runs: usize,
) -> Result<Vec<FailureCaseResult>, String> {
    let mut cases = Vec::with_capacity(4);

    // f1: CLI not found
    let mut not_found_trials = Vec::with_capacity(runs);
    for _ in 0..runs {
        let fleet = StubFleet::new(&bins.stub_bin, &[])?;
        let cmd = fleet.server_command(bins, server, provider);
        match McpServer::start_and_initialize(cmd, Duration::from_secs(15))? {
            StartOutcome::ExitedAtStartup {
                exit_code,
                stderr,
                elapsed_ms,
            } => {
                not_found_trials.push(FailureTrial {
                    wall_ms: round2(elapsed_ms),
                    channel: "server_exited_at_startup".into(),
                    is_error: true,
                    structured_kind: None,
                    exit_code,
                    message: stderr.trim().to_owned(),
                    diagnostic: None,
                    stdout_total: None,
                    stdout_captured: None,
                    result_bytes: stderr.len(),
                });
            }
            StartOutcome::Ready(mut mcp) => {
                let args = start_args(
                    server,
                    provider,
                    "Missing CLI test.",
                    &fleet.workspace_dir,
                    None,
                );
                let started = Instant::now();
                mcp.start_call(10, provider.start_tool(server), args)?;
                let timed = mcp.wait_response(10, Instant::now() + Duration::from_secs(15))?;
                let wall_ms =
                    round2(timed.received_at.duration_since(started).as_secs_f64() * 1000.0);
                let is_error = timed.frame["result"]["isError"] == true;
                let text = result_text(&timed.frame).unwrap_or("");
                let parsed = parse_tool_failure(server, text, &fleet.tmp_dir);
                let _ = mcp.finish(Duration::from_secs(10));
                not_found_trials.push(FailureTrial {
                    wall_ms,
                    channel: if is_error {
                        "mcp_tool_error".into()
                    } else {
                        "mcp_tool_success".into()
                    },
                    is_error,
                    structured_kind: parsed.structured_kind,
                    exit_code: parsed.exit_code,
                    message: parsed.message,
                    diagnostic: parsed.diagnostic,
                    stdout_total: parsed.stdout_total,
                    stdout_captured: parsed.stdout_captured,
                    result_bytes: text.len(),
                });
            }
        }
    }
    cases.push(summarize_failure_case(
        "not_found",
        server,
        provider,
        not_found_trials,
    ));

    // f2, f3, f4 run on a ready server with the stub present.
    let fleet = StubFleet::new(&bins.stub_bin, &[provider.cli_name()])?;
    let mut mcp = open_ready_server(&fleet, bins, server, provider)?;
    let tool = provider.start_tool(server);
    let args = start_args(
        server,
        provider,
        "Failure injection test.",
        &fleet.workspace_dir,
        None,
    );

    let oversized_stdout = build_oversized_stdout(provider);
    let injected_specs: [(&str, Value); 3] = [
        (
            "nonzero_exit",
            json!([{"code": 2, "stderr": "synthetic provider failure: backend unavailable\n"}]),
        ),
        (
            "malformed_output",
            json!([{"code": 0, "stdout": "not-json {{{ broken stream output\n"}]),
        ),
        (
            "exceed_capture_limit",
            json!([{"code": 0, "stdout": oversized_stdout}]),
        ),
    ];

    let mut next_id = 20_i64;
    for (case_name, step_array) in injected_specs {
        let mut trials = Vec::with_capacity(runs);
        for _ in 0..runs {
            fleet.script(provider.cli_name(), &step_array)?;
            let id = next_id;
            next_id += 1;
            let started = Instant::now();
            mcp.start_call(id, tool, args.clone())?;
            let timed = mcp.wait_response(id, Instant::now() + Duration::from_secs(30))?;
            let wall_ms = round2(timed.received_at.duration_since(started).as_secs_f64() * 1000.0);
            let is_error = timed.frame["result"]["isError"] == true;
            let text = result_text(&timed.frame).unwrap_or("");
            if is_error {
                let parsed = parse_tool_failure(server, text, &fleet.tmp_dir);
                trials.push(FailureTrial {
                    wall_ms,
                    channel: "mcp_tool_error".into(),
                    is_error: true,
                    structured_kind: parsed.structured_kind,
                    exit_code: parsed.exit_code,
                    message: parsed.message,
                    diagnostic: parsed.diagnostic,
                    stdout_total: parsed.stdout_total,
                    stdout_captured: parsed.stdout_captured,
                    result_bytes: text.len(),
                });
            } else {
                let (content, _) = parse_success_content(server, text);
                trials.push(FailureTrial {
                    wall_ms,
                    channel: "mcp_tool_success".into(),
                    is_error: false,
                    structured_kind: None,
                    exit_code: Some(0),
                    message: format!(
                        "succeeded without capture limit (content={:?})",
                        content.unwrap_or_default()
                    ),
                    diagnostic: None,
                    stdout_total: None,
                    stdout_captured: None,
                    result_bytes: text.len(),
                });
            }
        }
        cases.push(summarize_failure_case(case_name, server, provider, trials));
    }

    let _ = mcp.finish(Duration::from_secs(10));
    Ok(cases)
}

fn summarize_failure_case(
    case: &str,
    server: ServerKind,
    provider: ProviderKind,
    trials: Vec<FailureTrial>,
) -> FailureCaseResult {
    let wall_samples: Vec<f64> = trials.iter().map(|t| t.wall_ms).collect();
    let is_error_count = trials.iter().filter(|t| t.is_error).count();
    let first = trials.first();
    FailureCaseResult {
        case: case.into(),
        target: TargetKey::new(server, provider),
        wall_ms: Summary::from_samples(&wall_samples),
        channel_sample: first.map(|t| t.channel.clone()).unwrap_or_default(),
        is_error_count,
        structured_kind_sample: first.and_then(|t| t.structured_kind.clone()),
        exit_code_sample: first.and_then(|t| t.exit_code),
        message_sample: first.map(|t| t.message.clone()).unwrap_or_default(),
        diagnostic_sample: first.and_then(|t| t.diagnostic.clone()),
        trials,
    }
}

fn record_arg_value<'a>(record: &'a Value, flag: &str) -> Option<&'a str> {
    record["args"]
        .as_array()?
        .windows(2)
        .find(|w| w[0].as_str() == Some(flag))
        .and_then(|w| w[1].as_str())
}

fn record_has_arg(record: &Value, flag: &str) -> bool {
    record["args"]
        .as_array()
        .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(flag)))
}

fn record_has_pair(record: &Value, a: &str, b: &str) -> bool {
    record["args"]
        .as_array()
        .is_some_and(|args| args.windows(2).any(|w| w[0] == a && w[1] == b))
}

fn canonical_eq(a: &str, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => Path::new(a) == b,
    }
}

fn inspect_reply_record(
    provider: ProviderKind,
    record: &Value,
    expected_cwd: &Path,
) -> (bool, Option<bool>, bool, bool) {
    let cwd_carried = record["cwd"]
        .as_str()
        .is_some_and(|dir| canonical_eq(dir, expected_cwd));
    match provider {
        ProviderKind::Codex => {
            let model_carried = record_arg_value(record, "-m") == Some(CODEX_MODEL);
            let effort_carried = record_has_pair(
                record,
                "-c",
                &format!("model_reasoning_effort=\"{CODEX_EFFORT}\""),
            );
            let access_carried = record_has_pair(record, "-c", "sandbox_mode=\"read-only\"");
            (
                model_carried,
                Some(effort_carried),
                access_carried,
                cwd_carried,
            )
        }
        ProviderKind::Antigravity => {
            let model_carried = record_arg_value(record, "--model") == Some(AGY_MODEL);
            // In read-only access, neither server passes --dangerously-skip-permissions.
            let access_carried = !record_has_arg(record, "--dangerously-skip-permissions");
            (model_carried, None, access_carried, cwd_carried)
        }
    }
}

fn run_scenario_g(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    runs: usize,
) -> Result<ReplyResult, String> {
    let (fixture, expected) = fixture_for(provider);
    let expected_thread = match provider {
        ProviderKind::Codex => CODEX_THREAD,
        ProviderKind::Antigravity => AGY_THREAD,
    };

    let mut trials = Vec::with_capacity(runs);
    for _ in 0..runs {
        let fleet = StubFleet::new(&bins.stub_bin, &[provider.cli_name()])?;
        if provider == ProviderKind::Antigravity {
            fleet.seed_agy_conversation(AGY_THREAD)?;
        }
        let mut mcp = open_ready_server(&fleet, bins, server, provider)?;

        // 1. Start call with explicit non-default model, effort (codex), read-only access,
        //    and distinct session_cwd.
        fleet.script(provider.cli_name(), &json!([{"stdout": fixture}]))?;
        let s_args = start_args(
            server,
            provider,
            "Start session for reply continuity check.",
            &fleet.session_cwd,
            None,
        );
        let t_start = Instant::now();
        mcp.start_call(10, provider.start_tool(server), s_args)?;
        let start_resp = mcp.wait_response(10, Instant::now() + Duration::from_secs(15))?;
        let start_wall_ms =
            round2(start_resp.received_at.duration_since(t_start).as_secs_f64() * 1000.0);
        let start_text = result_text(&start_resp.frame).unwrap_or("");
        let (start_content, start_thread) = parse_success_content(server, start_text);
        if start_resp.frame["result"]["isError"] == true
            || start_content.as_deref() != Some(expected)
            || start_thread.as_deref() != Some(expected_thread)
        {
            return Err(format!(
                "scenario g start failed on {}/{}: {}",
                server.id(),
                provider.id(),
                start_resp.frame
            ));
        }

        let records_before_reply = fleet.records()?.len();

        // 2. Bare reply call (only thread id + prompt) to test whether server carries settings.
        fleet.script(provider.cli_name(), &json!([{"stdout": fixture}]))?;
        let bare_args = match server {
            ServerKind::Proxenos => json!({
                "thread_id": expected_thread,
                "prompt": "Follow up without repeating settings."
            }),
            ServerKind::ClaudeDelegator => json!({
                "threadId": expected_thread,
                "prompt": "Follow up without repeating settings."
            }),
        };
        let t_reply = Instant::now();
        mcp.start_call(11, provider.reply_tool(server), bare_args)?;
        let bare_resp = mcp.wait_response(11, Instant::now() + Duration::from_secs(15))?;
        let bare_reply_wall_ms =
            round2(bare_resp.received_at.duration_since(t_reply).as_secs_f64() * 1000.0);

        let (
            bare_reply_succeeded,
            bare_reply_error,
            model_carried,
            effort_carried,
            access_carried,
            cwd_carried,
        ) = if let Some(err_obj) = bare_resp.frame.get("error") {
            let msg = format!(
                "JSON-RPC {}: {}",
                err_obj["code"],
                err_obj["message"].as_str().unwrap_or("")
            );
            (
                false,
                Some(msg),
                false,
                (provider == ProviderKind::Codex).then_some(false),
                false,
                false,
            )
        } else if bare_resp.frame["result"]["isError"] == true {
            let msg = result_text(&bare_resp.frame)
                .unwrap_or("tool error")
                .to_owned();
            (
                false,
                Some(msg),
                false,
                (provider == ProviderKind::Codex).then_some(false),
                false,
                false,
            )
        } else {
            let records = fleet.records()?;
            let reply_record = records
                .get(records_before_reply)
                .ok_or("missing reply invocation record")?;
            let (m, e, a, c) = inspect_reply_record(provider, reply_record, &fleet.session_cwd);
            (true, None, m, e, a, c)
        };

        // 3. When claude-delegator rejects a bare agy-reply because `model` is a required
        //    schema parameter, also run an explicit-model reply so we verify reply execution
        //    and check whether `sandbox` and `cwd` carry when `model` is repeated.
        let mut explicit_model_reply_wall_ms = None;
        let mut explicit_model_reply_succeeded = None;
        let mut explicit_model_access_carried = None;
        let mut explicit_model_cwd_carried = None;
        if !bare_reply_succeeded
            && server == ServerKind::ClaudeDelegator
            && provider == ProviderKind::Antigravity
        {
            let before = fleet.records()?.len();
            fleet.script(provider.cli_name(), &json!([{"stdout": fixture}]))?;
            let explicit_args = json!({
                "threadId": expected_thread,
                "prompt": "Follow up repeating only required model.",
                "model": AGY_MODEL
            });
            let t_exp = Instant::now();
            mcp.start_call(12, provider.reply_tool(server), explicit_args)?;
            let exp_resp = mcp.wait_response(12, Instant::now() + Duration::from_secs(15))?;
            explicit_model_reply_wall_ms = Some(round2(
                exp_resp.received_at.duration_since(t_exp).as_secs_f64() * 1000.0,
            ));
            let ok = exp_resp.frame.get("error").is_none()
                && exp_resp.frame["result"]["isError"] != true;
            explicit_model_reply_succeeded = Some(ok);
            if ok {
                let records = fleet.records()?;
                if let Some(rec) = records.get(before) {
                    let (_, _, access_ok, cwd_ok) =
                        inspect_reply_record(provider, rec, &fleet.session_cwd);
                    explicit_model_access_carried = Some(access_ok);
                    explicit_model_cwd_carried = Some(cwd_ok);
                }
            }
        }

        let _ = mcp.finish(Duration::from_secs(10));
        trials.push(ReplyTrial {
            start_wall_ms,
            bare_reply_wall_ms,
            bare_reply_succeeded,
            bare_reply_error,
            model_carried,
            effort_carried,
            access_carried,
            cwd_carried,
            explicit_model_reply_wall_ms,
            explicit_model_reply_succeeded,
            explicit_model_access_carried,
            explicit_model_cwd_carried,
        });
    }

    let bare_wall: Vec<f64> = trials.iter().map(|t| t.bare_reply_wall_ms).collect();
    let exp_wall: Vec<f64> = trials
        .iter()
        .filter_map(|t| t.explicit_model_reply_wall_ms)
        .collect();
    let has_explicit = !exp_wall.is_empty();

    Ok(ReplyResult {
        target: TargetKey::new(server, provider),
        bare_reply_wall_ms: Summary::from_samples(&bare_wall),
        bare_reply_succeeded_count: trials.iter().filter(|t| t.bare_reply_succeeded).count(),
        model_carried_count: trials.iter().filter(|t| t.model_carried).count(),
        effort_carried_count: (provider == ProviderKind::Codex).then(|| {
            trials
                .iter()
                .filter(|t| t.effort_carried == Some(true))
                .count()
        }),
        access_carried_count: trials.iter().filter(|t| t.access_carried).count(),
        cwd_carried_count: trials.iter().filter(|t| t.cwd_carried).count(),
        bare_reply_error_sample: trials.iter().find_map(|t| t.bare_reply_error.clone()),
        explicit_model_reply_wall_ms: Summary::from_samples(&exp_wall),
        explicit_model_reply_succeeded_count: has_explicit.then(|| {
            trials
                .iter()
                .filter(|t| t.explicit_model_reply_succeeded == Some(true))
                .count()
        }),
        explicit_model_access_carried_count: has_explicit.then(|| {
            trials
                .iter()
                .filter(|t| t.explicit_model_access_carried == Some(true))
                .count()
        }),
        explicit_model_cwd_carried_count: has_explicit.then(|| {
            trials
                .iter()
                .filter(|t| t.explicit_model_cwd_carried == Some(true))
                .count()
        }),
        trials,
    })
}

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("proxenos-real-cmp-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_real_server(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    cwd: &Path,
) -> Result<McpServer, String> {
    let cmd = real_server_command(bins, server, provider, cwd);
    let mut mcp = match McpServer::start_and_initialize(cmd, Duration::from_secs(45))? {
        StartOutcome::Ready(mcp) => mcp,
        StartOutcome::ExitedAtStartup {
            exit_code, stderr, ..
        } => {
            return Err(format!(
                "real server {}/{} exited at startup (code {exit_code:?}): {stderr}",
                server.id(),
                provider.id()
            ));
        }
    };
    mcp.request(1, "tools/list", json!({}), Duration::from_secs(45))?;
    Ok(mcp)
}

fn run_real_scenario_a(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    runs: usize,
) -> Result<AdvisoryResult, String> {
    let workspace = TempWorkspace::new()?;
    let mut mcp = open_real_server(bins, server, provider, &workspace.0)?;
    let tool = provider.start_tool(server);
    let canary = "PROXENOS_REAL_CANARY_ALPHA";
    let prompt = format!(
        "Do not run any tools or read any files. Reply with exactly this token and nothing else: {canary}"
    );
    let args = start_args(server, provider, &prompt, &workspace.0, Some(120_000));

    let mut trials = Vec::with_capacity(runs);
    for i in 0..runs {
        let id = 10 + i as i64;
        let started = Instant::now();
        mcp.start_call(id, tool, args.clone())?;
        let timed = mcp.wait_response(id, Instant::now() + Duration::from_secs(150))?;
        let wall_ms = round2(timed.received_at.duration_since(started).as_secs_f64() * 1000.0);
        let is_error = timed.frame["result"]["isError"] == true;
        let text = result_text(&timed.frame).unwrap_or("");
        let (content, thread_id) = parse_success_content(server, text);
        let answer_intact =
            !is_error && content.as_deref().is_some_and(|body| body.contains(canary));
        trials.push(AdvisoryTrial {
            wall_ms,
            result_bytes: text.len(),
            frame_bytes: timed.raw_bytes,
            answer_intact,
            is_error,
            thread_id,
        });
    }
    let _ = mcp.finish(Duration::from_secs(15));

    let wall_samples: Vec<f64> = trials.iter().map(|t| t.wall_ms).collect();
    let result_samples: Vec<f64> = trials.iter().map(|t| t.result_bytes as f64).collect();
    let frame_samples: Vec<f64> = trials.iter().map(|t| t.frame_bytes as f64).collect();
    let intact_count = trials.iter().filter(|t| t.answer_intact).count();

    Ok(AdvisoryResult {
        target: TargetKey::new(server, provider),
        trials,
        wall_ms: Summary::from_samples(&wall_samples),
        result_bytes: Summary::from_samples(&result_samples),
        frame_bytes: Summary::from_samples(&frame_samples),
        intact_count,
    })
}

fn run_real_scenario_g(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    runs: usize,
) -> Result<RealReplyResult, String> {
    let workspace = TempWorkspace::new()?;
    let mut mcp = open_real_server(bins, server, provider, &workspace.0)?;
    let start_tool = provider.start_tool(server);
    let reply_tool = provider.reply_tool(server);

    let mut trials = Vec::with_capacity(runs);
    for i in 0..runs {
        let secret = format!("REMEMBER_TOKEN_{i}_7419");
        let start_prompt = format!(
            "Do not run any tools. Remember the codeword {secret} for our next turn, and reply with exactly: STORED"
        );
        let s_args = start_args(server, provider, &start_prompt, &workspace.0, Some(120_000));
        let start_id = 100 + (i as i64) * 2;
        let t_start = Instant::now();
        mcp.start_call(start_id, start_tool, s_args)?;
        let start_resp = mcp.wait_response(start_id, Instant::now() + Duration::from_secs(150))?;
        let start_wall_ms =
            round2(start_resp.received_at.duration_since(t_start).as_secs_f64() * 1000.0);
        let start_err = start_resp.frame["result"]["isError"] == true
            || start_resp.frame.get("error").is_some();
        let start_text = result_text(&start_resp.frame).unwrap_or("");
        let (start_content, thread_id) = parse_success_content(server, start_text);
        let start_canary_intact = !start_err
            && start_content
                .as_deref()
                .is_some_and(|c| c.contains("STORED"));

        let Some(tid) = thread_id.filter(|id| !id.is_empty() && id != "unknown") else {
            trials.push(RealReplyTrial {
                start_wall_ms,
                start_succeeded: !start_err,
                start_canary_intact,
                reply_wall_ms: None,
                reply_succeeded: false,
                reply_canary_intact: false,
                model_passed_on_reply: None,
                thread_id: None,
                error: Some(format!("no usable thread_id returned: {start_text}")),
            });
            continue;
        };

        // On claude-delegator, explicitly repeat the cheap model/effort on reply so the
        // bridge never falls back to its expensive default (gpt-6.2-sol / ultra).
        // On Proxenos, omit model/effort/access/cwd so D19 session memory supplies them.
        let reply_prompt = "Do not run any tools. Repeat the exact codeword from the previous turn and nothing else.";
        let r_args = match (server, provider) {
            (ServerKind::Proxenos, _) => json!({
                "thread_id": tid,
                "prompt": reply_prompt,
                "timeout_ms": 120_000
            }),
            (ServerKind::ClaudeDelegator, ProviderKind::Codex) => json!({
                "threadId": tid,
                "prompt": reply_prompt,
                "model": CODEX_MODEL,
                "effort": CODEX_EFFORT,
                "sandbox": "read-only",
                "cwd": workspace.0,
                "timeout": 120_000
            }),
            (ServerKind::ClaudeDelegator, ProviderKind::Antigravity) => json!({
                "threadId": tid,
                "prompt": reply_prompt,
                "model": AGY_MODEL,
                "sandbox": "read-only",
                "cwd": workspace.0,
                "timeout": 120_000
            }),
        };

        let reply_id = start_id + 1;
        let t_reply = Instant::now();
        mcp.start_call(reply_id, reply_tool, r_args)?;
        let reply_resp = mcp.wait_response(reply_id, Instant::now() + Duration::from_secs(150))?;
        let reply_wall_ms =
            round2(reply_resp.received_at.duration_since(t_reply).as_secs_f64() * 1000.0);
        let reply_err = reply_resp.frame["result"]["isError"] == true
            || reply_resp.frame.get("error").is_some();
        let reply_text = result_text(&reply_resp.frame).unwrap_or("");
        let (reply_content, _) = parse_success_content(server, reply_text);
        let model_passed_on_reply = if server == ServerKind::Proxenos {
            serde_json::from_str::<Value>(reply_text)
                .ok()
                .and_then(|v| v["observed"]["passed"]["model"].as_str().map(str::to_owned))
        } else {
            Some(match provider {
                ProviderKind::Codex => CODEX_MODEL.into(),
                ProviderKind::Antigravity => AGY_MODEL.into(),
            })
        };
        let reply_canary_intact = !reply_err
            && reply_content
                .as_deref()
                .is_some_and(|c| c.contains(&secret));

        trials.push(RealReplyTrial {
            start_wall_ms,
            start_succeeded: !start_err,
            start_canary_intact,
            reply_wall_ms: Some(reply_wall_ms),
            reply_succeeded: !reply_err,
            reply_canary_intact,
            model_passed_on_reply,
            thread_id: Some(tid),
            error: reply_err.then(|| reply_text.to_owned()),
        });
    }
    let _ = mcp.finish(Duration::from_secs(15));

    let start_samples: Vec<f64> = trials.iter().map(|t| t.start_wall_ms).collect();
    let reply_samples: Vec<f64> = trials.iter().filter_map(|t| t.reply_wall_ms).collect();

    Ok(RealReplyResult {
        target: TargetKey::new(server, provider),
        start_wall_ms: Summary::from_samples(&start_samples),
        reply_wall_ms: Summary::from_samples(&reply_samples),
        start_succeeded_count: trials.iter().filter(|t| t.start_succeeded).count(),
        reply_succeeded_count: trials.iter().filter(|t| t.reply_succeeded).count(),
        reply_canary_intact_count: trials.iter().filter(|t| t.reply_canary_intact).count(),
        trials,
    })
}
