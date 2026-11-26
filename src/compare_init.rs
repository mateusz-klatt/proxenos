//! Measure a new server process through its first successful MCP initialize response.
//! See docs/comparison.md for the two server commands, artifact identity and boundaries.

use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use uuid::Uuid;

const REVISION: &str = "2024-11-05";
const LIMIT: Duration = Duration::from_secs(10);
const MAX_RESPONSE: u64 = 1024 * 1024;
const USAGE: &str = "usage: compare_init <proxenos> <node> <claude-delegator-dir> <proxenos-stub> <even-pairs: 2..1000>";
type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Serialize)]
struct Artifact {
    #[serde(serialize_with = "display_path")]
    path: PathBuf,
    sha256: String,
}

fn display_path<S: Serializer>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(&path.display())
}

impl Artifact {
    fn open(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        if !path.is_file() {
            return Err(io::Error::other(format!(
                "expected a file: {}",
                path.display()
            )));
        }
        let mut file = io::BufReader::new(File::open(&path)?);
        let mut hash = Sha256::new();
        loop {
            let bytes = file.fill_buf()?;
            if bytes.is_empty() {
                break;
            }
            hash.update(bytes);
            let len = bytes.len();
            file.consume(len);
        }
        let mut sha256 = String::with_capacity(64);
        for byte in hash.finalize() {
            use std::fmt::Write;
            write!(sha256, "{byte:02x}").expect("writing to a String");
        }
        Ok(Self { path, sha256 })
    }
}

// Rust reports extended-length paths on Windows. Node 22.23.5 rejects that spelling
// for its entry script, even when both the executable and the script exist.
#[cfg(windows)]
fn ordinary_windows_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};

    let remove = match path.components().next() {
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::VerbatimDisk(_)) => 0..4,
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::VerbatimUNC(_, _)) => 1..7,
        _ => return path.to_owned(),
    };
    // Strip \\?\ from a drive path or retain two slashes from \\?\UNC\.
    let mut units: Vec<_> = path.as_os_str().encode_wide().collect();
    drop(units.drain(remove));
    OsString::from_wide(&units).into()
}

#[cfg(windows)]
fn verified_node_entry(canonical: &Path, ordinary: PathBuf) -> PathBuf {
    if ordinary.canonicalize().is_ok_and(|path| path == canonical) {
        ordinary
    } else {
        canonical.to_owned()
    }
}

#[derive(Serialize)]
struct Inputs {
    proxenos: Artifact,
    node: Artifact,
    bridge: Artifact,
    #[serde(skip)]
    bridge_entry: PathBuf,
    stub: Artifact,
    package: Artifact,
    baseline_version: String,
    pairs: usize,
}

impl Inputs {
    fn parse() -> Result<Self> {
        let args: Vec<_> = env::args_os().skip(1).collect();
        let [proxenos, node, root, stub, pairs] = args.as_slice() else {
            return Err(USAGE.into());
        };
        let pairs: usize = pairs.to_str().and_then(|v| v.parse().ok()).ok_or(USAGE)?;
        if !(2..=1000).contains(&pairs) || !pairs.is_multiple_of(2) {
            return Err(USAGE.into());
        }
        let root = Path::new(root);
        let package = Artifact::open(&root.join("package.json"))?;
        let metadata: Value = serde_json::from_reader(File::open(&package.path)?)?;
        let version = metadata["version"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("baseline package.json needs a version")?;
        let proxenos = Artifact::open(Path::new(proxenos))?;
        let node = Artifact::open(Path::new(node))?;
        let bridge = Artifact::open(&root.join("server/codex/index.js"))?;
        #[cfg(windows)]
        let bridge_entry = verified_node_entry(&bridge.path, ordinary_windows_path(&bridge.path));
        #[cfg(not(windows))]
        let bridge_entry = bridge.path.clone();
        Ok(Self {
            proxenos,
            node,
            bridge,
            bridge_entry,
            stub: Artifact::open(Path::new(stub))?,
            package,
            baseline_version: version.to_owned(),
            pairs,
        })
    }
}

struct Profile(PathBuf);

impl Profile {
    fn new(stub: &Path) -> Result<Self> {
        // Put the stub on the same executable filesystem without copying it into every trial.
        let root = stub
            .parent()
            .expect("canonical file has a parent")
            .join(format!("proxenos-bench-{}", Uuid::new_v4()));
        fs::create_dir(&root)?;
        let profile = Self(root);
        let bin = profile.0.join("bin");
        fs::create_dir(&bin)?;
        fs::hard_link(stub, bin.join(format!("codex{}", env::consts::EXE_SUFFIX)))?;
        Ok(profile)
    }

    fn configure(&self, cmd: &mut Command) {
        cmd.env_clear()
            .current_dir(&self.0)
            .env("PATH", self.0.join("bin"));
        for key in [
            "HOME",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "TMPDIR",
            "TEMP",
            "TMP",
            "PROXENOS_STUB_DIR",
        ] {
            cmd.env(key, &self.0);
        }
        #[cfg(windows)]
        cmd.envs(env::var_os("SystemRoot").map(|value| ("SystemRoot", value)));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Target {
    Proxenos,
    ClaudeDelegator,
}

impl Target {
    fn command(self, inputs: &Inputs) -> Command {
        match self {
            Self::Proxenos => {
                let mut cmd = Command::new(&inputs.proxenos.path);
                cmd.args(["mcp", "--client", "claude"]);
                cmd
            }
            Self::ClaudeDelegator => {
                let mut cmd = Command::new(&inputs.node.path);
                cmd.arg(&inputs.bridge_entry);
                cmd
            }
        }
    }
}

#[derive(Serialize)]
struct Observation {
    latency_ms: f64,
    server_name: String,
    server_version: String,
}

fn initialize_request() -> String {
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": REVISION, "capabilities": {},
            "clientInfo": {"name": "proxenos-comparison", "version": "1"}
        }
    });
    format!("{request}\n")
}

async fn exchange(
    child: &mut Child,
    start: Instant,
    target: Target,
    baseline_version: &str,
    request: &str,
) -> Result<Observation> {
    let mut input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    input.write_all(request.as_bytes()).await?;
    let mut stream = BufReader::new(output).take(MAX_RESPONSE);
    let mut line = Vec::new();
    loop {
        line.clear();
        stream.read_until(b'\n', &mut line).await?;
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        if !line.ends_with(b"\n") {
            return Err("initialize response missing or exceeded 1 MiB".into());
        }
        let msg: Value = serde_json::from_slice(&line)?;
        if msg["jsonrpc"] != "2.0" {
            return Err("incorrect JSON-RPC version".into());
        }
        if msg.get("id").is_none() && msg["method"].is_string() {
            continue;
        }
        let result = &msg["result"];
        if msg["id"] != 1
            || msg.get("error").is_some()
            || result["protocolVersion"] != REVISION
            || !result["capabilities"]["tools"].is_object()
        {
            return Err(
                "invalid initialize response, protocol revision, or tools capability".into(),
            );
        }
        let info = &result["serverInfo"];
        let name = info["name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("missing server name")?;
        let version = info["version"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("missing server version")?;
        let matches_target = match target {
            Target::Proxenos => name == "proxenos",
            Target::ClaudeDelegator => {
                name == "claude-delegator-codex" && version == baseline_version
            }
        };
        if !matches_target {
            return Err("initialize response does not match the selected server".into());
        }
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        drop(input);
        // Drain anything emitted at shutdown so a write cannot block exit.
        tokio::io::copy(&mut stream.into_inner(), &mut tokio::io::sink()).await?;
        if !child.wait().await?.success() {
            return Err("server exited unsuccessfully".into());
        }
        return Ok(Observation {
            latency_ms,
            server_name: name.into(),
            server_version: version.into(),
        });
    }
}

async fn measure(inputs: &Inputs, target: Target, request: &str) -> Result<Observation> {
    let profile = Profile::new(&inputs.stub.path)?;
    let mut cmd = target.command(inputs);
    profile.configure(&mut cmd);
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let result = match timeout(
        LIMIT,
        exchange(&mut child, start, target, &inputs.baseline_version, request),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err("timed out during initialization or shutdown".into()),
    };
    if result.is_err() {
        let _ = child.start_kill();
        timeout(LIMIT, child.wait()).await??;
    }
    result
}

#[derive(Serialize)]
struct Trial {
    pair: usize,
    position: usize,
    target: Target,
    observation: Option<Observation>,
    error: Option<String>,
}

fn summary(trials: &[Trial], target: Target) -> Value {
    let selected: Vec<_> = trials
        .iter()
        .filter(|trial| trial.target == target)
        .collect();
    let mut times: Vec<_> = selected
        .iter()
        .filter_map(|trial| trial.observation.as_ref())
        .map(|sample| sample.latency_ms)
        .collect();
    times.sort_unstable_by(f64::total_cmp);
    let n = times.len();
    let median = (n > 0).then(|| (times[(n - 1) / 2] + times[n / 2]) / 2.0);
    json!({"successful":n,"failed":selected.len()-n,"median_ms":median})
}

async fn run() -> Result<ExitCode> {
    let inputs = Inputs::parse()?;
    let request = initialize_request();
    let mut trials = Vec::new();
    for pair in 0..inputs.pairs {
        let mut order = [Target::Proxenos, Target::ClaudeDelegator];
        if pair % 2 == 1 {
            order.reverse();
        }
        for (position, target) in order.into_iter().enumerate() {
            let (observation, error) = match measure(&inputs, target, &request).await {
                Ok(observed) => (Some(observed), None),
                Err(error) => (None, Some(error.to_string())),
            };
            trials.push(Trial {
                pair: pair + 1,
                position: position + 1,
                target,
                observation,
                error,
            });
        }
    }
    let all_ok = trials.iter().all(|trial| trial.error.is_none());
    let report = json!({
        "schema_version":1,"scenario":"process_start_to_initialize_response",
        "protocol_revision":REVISION,"os":env::consts::OS,"architecture":env::consts::ARCH,
        "d12_baseline_version_matches":inputs.baseline_version=="3.5.0","inputs":inputs,
        "summary":{"proxenos":summary(&trials, Target::Proxenos),"claude_delegator":summary(&trials, Target::ClaudeDelegator)},
        "trials":trials
    });
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &report)?;
    writeln!(out)?;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn node_entry_supports_drive_and_unc_paths_and_preserves_other_namespaces() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        for (input, expected) in [
            (r"\\?\C:\bench dir\index.js", r"C:\bench dir\index.js"),
            (r"\\?\UNC\host\share\index.js", r"\\host\share\index.js"),
            (r"\\?\Volume{abc}\index.js", r"\\?\Volume{abc}\index.js"),
            (r"C:\plain\index.js", r"C:\plain\index.js"),
        ] {
            assert_eq!(ordinary_windows_path(Path::new(input)), Path::new(expected));
        }
        // A path need not have a UTF-8 representation. Preserve its native code units.
        let mut units: Vec<u16> = r"\\?\D:\folder\".encode_utf16().collect();
        units.push(0xd800);
        let path = PathBuf::from(OsString::from_wide(&units));
        let ordinary = ordinary_windows_path(&path);
        assert!(
            ordinary
                .as_os_str()
                .encode_wide()
                .eq(units[4..].iter().copied())
        );
    }

    #[test]
    fn a_missing_or_different_alias_preserves_the_fingerprinted_entry() {
        let file = env::current_exe().unwrap().canonicalize().unwrap();
        let directory = file.parent().unwrap();
        for alias in [
            directory.to_owned(),
            directory.join(Uuid::new_v4().to_string()),
        ] {
            assert_eq!(verified_node_entry(&file, alias), file);
        }
    }
}
