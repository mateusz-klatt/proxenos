//! Hermetic stub fleet environments shared identically by Proxenos and claude-delegator.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerKind {
    Proxenos,
    ClaudeDelegator,
}

impl ServerKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Proxenos => "proxenos",
            Self::ClaudeDelegator => "claude-delegator",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Codex,
    Antigravity,
}

impl ProviderKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "agy",
        }
    }

    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "agy",
        }
    }

    pub fn start_tool(self, server: ServerKind) -> &'static str {
        match (server, self) {
            (_, Self::Codex) => "codex",
            (ServerKind::Proxenos, Self::Antigravity) => "antigravity",
            (ServerKind::ClaudeDelegator, Self::Antigravity) => "agy",
        }
    }

    pub fn reply_tool(self, server: ServerKind) -> &'static str {
        match (server, self) {
            (_, Self::Codex) => "codex-reply",
            (ServerKind::Proxenos, Self::Antigravity) => "antigravity-reply",
            (ServerKind::ClaudeDelegator, Self::Antigravity) => "agy-reply",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Binaries {
    pub proxenos_bin: PathBuf,
    pub stub_bin: PathBuf,
    pub delegator_dir: PathBuf,
    pub node_bin: PathBuf,
}

/// An isolated directory tree holding hard-linked stubs, scratch temp space, workspace
/// directories, and an Antigravity conversation store. Cleaned up automatically on Drop.
pub struct StubFleet {
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub session_cwd: PathBuf,
    pub conversations_dir: PathBuf,
}

impl StubFleet {
    pub fn new(stub_bin: &Path, clis: &[&str]) -> Result<Self, String> {
        // Place hard links beside the stub binary so TMPDIR mounted noexec never blocks exec.
        let parent = stub_bin
            .parent()
            .ok_or_else(|| format!("stub path has no parent: {}", stub_bin.display()))?;
        let root = parent.join(format!("proxenos-cmp-{}", uuid::Uuid::new_v4()));
        let bin_dir = root.join("bin");
        let tmp_dir = root.join("tmp");
        let workspace_dir = root.join("workspace");
        let session_cwd = root.join("session-cwd");
        let conversations_dir = root.join(".gemini/antigravity-cli/conversations");
        for dir in [
            &bin_dir,
            &tmp_dir,
            &workspace_dir,
            &session_cwd,
            &conversations_dir,
        ] {
            fs::create_dir_all(dir)
                .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
        }
        for cli in clis {
            let dest = bin_dir.join(format!("{cli}{}", std::env::consts::EXE_SUFFIX));
            fs::hard_link(stub_bin, &dest).map_err(|err| {
                format!(
                    "failed to hard-link {} to {}: {err}",
                    stub_bin.display(),
                    dest.display()
                )
            })?;
        }
        Ok(Self {
            root,
            bin_dir,
            tmp_dir,
            workspace_dir,
            session_cwd,
            conversations_dir,
        })
    }

    pub fn seed_agy_conversation(&self, id: &str) -> Result<(), String> {
        let path = self.conversations_dir.join(format!("{id}.db"));
        fs::write(&path, []).map_err(|err| format!("failed to seed {}: {err}", path.display()))
    }

    pub fn script(&self, cli: &str, steps: &Value) -> Result<(), String> {
        let path = self.root.join(format!("{cli}.steps"));
        fs::write(&path, steps.to_string())
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
    }

    pub fn records(&self) -> Result<Vec<Value>, String> {
        let path = self.root.join("invocations");
        let Ok(mut file) = File::open(&path) else {
            return Ok(Vec::new());
        };
        file.lock_shared()
            .map_err(|err| format!("failed to lock {}: {err}", path.display()))?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|err| format!("invalid invocation record ({err}): {line}"))
            })
            .collect()
    }

    /// Build a command for either Proxenos or a claude-delegator Node bridge with strictly
    /// isolated `PATH` (only `self.bin_dir`) and `HOME` (`self.root`), so real CLIs can
    /// never be discovered or launched in Part 1.
    pub fn server_command(
        &self,
        bins: &Binaries,
        server: ServerKind,
        provider: ProviderKind,
    ) -> Command {
        let mut command = match server {
            ServerKind::Proxenos => {
                let mut cmd = Command::new(&bins.proxenos_bin);
                cmd.args(["mcp", "--client", "claude"]);
                cmd
            }
            ServerKind::ClaudeDelegator => {
                let bridge = match provider {
                    ProviderKind::Codex => bins.delegator_dir.join("server/codex/index.js"),
                    ProviderKind::Antigravity => bins.delegator_dir.join("server/agy/index.js"),
                };
                let mut cmd = Command::new(&bins.node_bin);
                cmd.arg(bridge);
                cmd
            }
        };

        scrub_delegation_env(&mut command);

        command
            .current_dir(&self.workspace_dir)
            .env("PATH", &self.bin_dir)
            .env("PROXENOS_STUB_DIR", &self.root)
            .env("PROXENOS_AGY_CONVERSATIONS_DIR", &self.conversations_dir)
            .env(
                "CLAUDE_DELEGATOR_AGY_CONVERSATIONS_DIR",
                &self.conversations_dir,
            )
            .env("TMPDIR", &self.tmp_dir)
            .env("TEMP", &self.tmp_dir)
            .env("TMP", &self.tmp_dir)
            .env("TOKIO_WORKER_THREADS", "2");

        for key in ["HOME", "USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
            command.env(key, &self.root);
        }
        command
    }
}

impl Drop for StubFleet {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Build a command for Part 2 (`--real-models`) using the user's real `PATH` and `HOME`
/// in an empty temporary workspace directory, scrubbing stub and depth-guard variables.
pub fn real_server_command(
    bins: &Binaries,
    server: ServerKind,
    provider: ProviderKind,
    cwd: &Path,
) -> Command {
    let mut command = match server {
        ServerKind::Proxenos => {
            let mut cmd = Command::new(&bins.proxenos_bin);
            cmd.args(["mcp", "--client", "claude"]);
            cmd
        }
        ServerKind::ClaudeDelegator => {
            let bridge = match provider {
                ProviderKind::Codex => bins.delegator_dir.join("server/codex/index.js"),
                ProviderKind::Antigravity => bins.delegator_dir.join("server/agy/index.js"),
            };
            let mut cmd = Command::new(&bins.node_bin);
            cmd.arg(bridge);
            cmd
        }
    };
    scrub_delegation_env(&mut command);
    command.current_dir(cwd);
    command
}

fn scrub_delegation_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("PROXENOS_")
            || upper.starts_with("CLAUDE_DELEGATOR_")
            || upper == "CODEX_DELEGATOR_CODEX_BIN"
            || upper == "IRIS_EXECUTION"
            || upper == "CLAUDECODE"
        {
            command.env_remove(key);
        }
    }
}
