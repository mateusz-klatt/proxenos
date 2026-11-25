//! Honest comparison harness driving Proxenos and claude-delegator over stdio.
//!
//! Part 1 (default) drives both MCP servers with the same `proxenos-stub` binary and
//! never starts a real agent CLI or spends a model call. Part 2 (`--real-models`,
//! guarded by `PROXENOS_COMPARISON_REAL=yes`) runs scenarios a and g against real CLIs
//! with cheap models.

mod fleet;
mod mcp;
mod proc_check;
mod report;
mod scenarios;
mod stats;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::fleet::Binaries;
use crate::report::{FullReport, HostMetadata, build_feature_differences, render_markdown};
use crate::scenarios::{RunConfig, probe_proxenos, run_part1, run_part2};

const REAL_CONFIRM_ENV: &str = "PROXENOS_COMPARISON_REAL";

struct Args {
    proxenos_bin: Option<PathBuf>,
    stub_bin: Option<PathBuf>,
    delegator_dir: Option<PathBuf>,
    node_bin: Option<PathBuf>,
    runs: Option<usize>,
    timeout_ms: u64,
    concurrency: usize,
    out_dir: PathBuf,
    markdown_out: Option<PathBuf>,
    real_models: bool,
    scenarios: Option<Vec<String>>,
    help: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut parsed = Args {
        proxenos_bin: None,
        stub_bin: None,
        delegator_dir: None,
        node_bin: None,
        runs: None,
        timeout_ms: 10_000,
        concurrency: 4,
        out_dir: PathBuf::from(".local/comparison-runs"),
        markdown_out: None,
        real_models: false,
        scenarios: None,
        help: false,
    };

    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut next_val = |name: &str| -> Result<String, String> {
            iter.next()
                .ok_or_else(|| format!("missing value for {name}"))
        };
        match flag.as_str() {
            "-h" | "--help" => parsed.help = true,
            "--proxenos-bin" => parsed.proxenos_bin = Some(PathBuf::from(next_val(&flag)?)),
            "--stub-bin" => parsed.stub_bin = Some(PathBuf::from(next_val(&flag)?)),
            "--delegator-dir" => parsed.delegator_dir = Some(PathBuf::from(next_val(&flag)?)),
            "--node-bin" => parsed.node_bin = Some(PathBuf::from(next_val(&flag)?)),
            "--runs" => {
                let n: usize = next_val(&flag)?
                    .parse()
                    .map_err(|_| "--runs must be a positive integer")?;
                if n == 0 {
                    return Err("--runs must be at least 1".into());
                }
                parsed.runs = Some(n);
            }
            "--timeout-ms" => {
                let ms: u64 = next_val(&flag)?
                    .parse()
                    .map_err(|_| "--timeout-ms must be an integer")?;
                if ms < 10_000 {
                    return Err(
                        "--timeout-ms must be >= 10000 (claude-delegator minimum timeout)".into(),
                    );
                }
                parsed.timeout_ms = ms;
            }
            "--concurrency" => {
                let c: usize = next_val(&flag)?
                    .parse()
                    .map_err(|_| "--concurrency must be a positive integer")?;
                if c == 0 {
                    return Err("--concurrency must be at least 1".into());
                }
                parsed.concurrency = c;
            }
            "--out-dir" => parsed.out_dir = PathBuf::from(next_val(&flag)?),
            "--markdown-out" => parsed.markdown_out = Some(PathBuf::from(next_val(&flag)?)),
            "--real-models" => parsed.real_models = true,
            "--scenarios" => {
                let raw = next_val(&flag)?;
                let list: Vec<String> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                if list.is_empty() {
                    return Err("--scenarios must name at least one of a,b,c,d,e,f,g".into());
                }
                parsed.scenarios = Some(list);
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }
    Ok(parsed)
}

fn usage() -> &'static str {
    concat!(
        "Usage: cargo run --features test-tools --example comparison -- [OPTIONS]\n\n",
        "Options:\n",
        "  --proxenos-bin <PATH>   Path to any proxenos binary (default: sibling target binary)\n",
        "  --stub-bin <PATH>       Path to proxenos-stub binary (default: sibling target binary)\n",
        "  --delegator-dir <PATH>  Path to claude-delegator checkout (default: ~/.claude/plugins/marketplaces/jarrodwatts-claude-delegator)\n",
        "  --node-bin <PATH>       Path to node executable (default: resolved from PATH)\n",
        "  --runs <N>              Trials per target/scenario (default: 10 for part 1, 3 for part 2)\n",
        "  --timeout-ms <MS>       Timeout for scenario b (default: 10000, minimum accepted by both)\n",
        "  --concurrency <N>       Parallel workers for sleeping scenarios b/c/d/e (default: 4)\n",
        "  --scenarios <LIST>      Comma-separated subset of a,b,c,d,e,f,g (part 1 only)\n",
        "  --out-dir <DIR>         Directory for raw JSON and rendered Markdown (default: .local/comparison-runs)\n",
        "  --markdown-out <PATH>   Additional path to write rendered Markdown tables\n",
        "  --real-models           Run Part 2 (scenarios a & g) against REAL CLIs and cheap models;\n",
        "                          requires PROXENOS_COMPARISON_REAL=yes in the environment\n",
        "  -h, --help              Print this help text\n"
    )
}

fn default_target_bin(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    // Examples live in target/<profile>/examples/<name>; binaries live in target/<profile>/.
    let profile_dir = exe
        .parent()
        .and_then(|dir| {
            if dir.file_name().is_some_and(|n| n == "examples") {
                dir.parent()
            } else {
                Some(dir)
            }
        })
        .ok_or("cannot locate cargo target profile directory")?;
    Ok(profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

fn resolve_on_path(program: &str) -> Result<PathBuf, String> {
    let path_var = std::env::var_os("PATH").ok_or("PATH is unset")?;
    let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    for dir in std::env::split_paths(&path_var) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("could not find `{program}` on PATH"))
}

fn default_delegator_dir() -> Result<PathBuf, String> {
    let home = std::env::home_dir().ok_or("cannot determine HOME directory")?;
    Ok(home.join(".claude/plugins/marketplaces/jarrodwatts-claude-delegator"))
}

fn command_stdout(program: &Path, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn read_delegator_version(delegator_dir: &Path) -> String {
    let pkg = delegator_dir.join("package.json");
    fs::read_to_string(pkg)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v["version"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn detect_kernel() -> String {
    #[cfg(unix)]
    {
        command_stdout(Path::new("uname"), &["-srm"])
    }
    #[cfg(not(unix))]
    {
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    }
}

fn detect_cpu() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(info) = fs::read_to_string("/proc/cpuinfo") {
        let model = info.lines().find_map(|line| {
            line.strip_prefix("model name")?
                .split_once(':')?
                .1
                .trim()
                .into()
        });
        let cores = info
            .lines()
            .filter(|line| line.starts_with("processor\t:") || line.starts_with("processor :"))
            .count();
        if let Some(model) = model {
            return format!("{model} ({cores} logical CPUs)");
        }
    }
    "unknown".into()
}

fn detect_git_commit() -> String {
    command_stdout(Path::new("git"), &["rev-parse", "--short", "HEAD"])
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    if args.help {
        eprintln!("{}", usage());
        return Ok(());
    }

    if args.real_models {
        let confirmed = std::env::var(REAL_CONFIRM_ENV)
            .ok()
            .is_some_and(|v| v.trim() == "yes");
        if !confirmed {
            return Err(format!(
                "Refusing to run real CLIs and spend model calls without {REAL_CONFIRM_ENV}=yes. \
                 Set {REAL_CONFIRM_ENV}=yes alongside --real-models to run Part 2 explicitly."
            ));
        }
    }

    let proxenos_bin = match args.proxenos_bin {
        Some(path) => path,
        None => default_target_bin("proxenos")?,
    };
    let stub_bin = match args.stub_bin {
        Some(path) => path,
        None => default_target_bin("proxenos-stub")?,
    };
    let delegator_dir = match args.delegator_dir {
        Some(path) => path,
        None => default_delegator_dir()?,
    };
    let node_bin = match args.node_bin {
        Some(path) => path,
        None => resolve_on_path("node")?,
    };

    if !proxenos_bin.is_file() {
        return Err(format!(
            "proxenos binary not found at {} (build first with `cargo build --features test-tools --bins`)",
            proxenos_bin.display()
        ));
    }
    if !args.real_models && !stub_bin.is_file() {
        return Err(format!(
            "proxenos-stub binary not found at {} (build first with `cargo build --features test-tools --bins`)",
            stub_bin.display()
        ));
    }
    for bridge in ["server/codex/index.js", "server/agy/index.js"] {
        let path = delegator_dir.join(bridge);
        if !path.is_file() {
            return Err(format!(
                "claude-delegator bridge not found at {}",
                path.display()
            ));
        }
    }

    let bins = Binaries {
        proxenos_bin: fs::canonicalize(&proxenos_bin).unwrap_or(proxenos_bin),
        stub_bin: fs::canonicalize(&stub_bin).unwrap_or(stub_bin),
        delegator_dir: fs::canonicalize(&delegator_dir).unwrap_or(delegator_dir),
        node_bin: fs::canonicalize(&node_bin).unwrap_or(node_bin),
    };

    let runs = args.runs.unwrap_or(if args.real_models { 3 } else { 10 });
    let config = RunConfig {
        runs,
        timeout_ms: args.timeout_ms,
        concurrency: args.concurrency,
    };

    let proxenos_probe = if !args.real_models {
        Some(probe_proxenos(&bins)?)
    } else {
        None
    };

    let metadata = HostMetadata {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel: detect_kernel(),
        cpu: detect_cpu(),
        node_version: command_stdout(&bins.node_bin, &["--version"]),
        delegator_version: read_delegator_version(&bins.delegator_dir),
        proxenos_version: command_stdout(&bins.proxenos_bin, &["--version"]),
        proxenos_commit: detect_git_commit(),
        proxenos_bin: bins.proxenos_bin.display().to_string(),
        stub_bin: bins.stub_bin.display().to_string(),
        delegator_dir: bins.delegator_dir.display().to_string(),
        mode: if args.real_models {
            "part2-real-models".into()
        } else {
            "part1-stubbed".into()
        },
        runs,
        timeout_ms: args.timeout_ms,
        concurrency: args.concurrency,
    };

    let (part1, part2) = if args.real_models {
        (None, Some(run_part2(&bins, runs)?))
    } else {
        (
            Some(run_part1(&bins, &config, args.scenarios.as_deref())?),
            None,
        )
    };

    let full = FullReport {
        schema_version: 1,
        metadata,
        feature_differences: build_feature_differences(proxenos_probe.as_ref()),
        proxenos_probe,
        part1,
        part2,
    };

    fs::create_dir_all(&args.out_dir)
        .map_err(|err| format!("failed to create {}: {err}", args.out_dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let prefix = if args.real_models { "part2" } else { "part1" };
    let json_path = args.out_dir.join(format!("{prefix}-{stamp}.json"));
    let md_path = args.out_dir.join(format!("{prefix}-{stamp}.md"));

    let json_text = serde_json::to_string_pretty(&full)
        .map_err(|err| format!("failed to serialize JSON report: {err}"))?;
    fs::write(&json_path, format!("{json_text}\n"))
        .map_err(|err| format!("failed to write {}: {err}", json_path.display()))?;

    let md_text = render_markdown(&full);
    fs::write(&md_path, &md_text)
        .map_err(|err| format!("failed to write {}: {err}", md_path.display()))?;
    if let Some(extra_md) = &args.markdown_out {
        if let Some(parent) = extra_md.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        fs::write(extra_md, &md_text)
            .map_err(|err| format!("failed to write {}: {err}", extra_md.display()))?;
    }

    eprintln!("[comparison] Raw JSON written to {}", json_path.display());
    eprintln!(
        "[comparison] Rendered Markdown written to {}",
        md_path.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("comparison error: {err}");
            ExitCode::from(2)
        }
    }
}
