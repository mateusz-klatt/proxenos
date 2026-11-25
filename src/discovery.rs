//! Resolve provider CLIs once per server. Version checks use the same process ownership and
//! cancellation as a turn. A negative check can recover on the next call without a restart.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::{Component, PathBuf};
use std::time::Duration;

use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::envelope::{Failure, FailureKind};
use crate::mcp::Activity;
use crate::providers::{self, Provider};
use crate::supervise::{self, Ending, Launch};

#[cfg(windows)]
mod windows;

#[derive(Clone)]
pub struct Executable {
    pub program: PathBuf,
    pub prefix: Vec<OsString>,
}

fn absolute_env(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from).filter(|path| {
        path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
    })
}

fn search_path() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path)
                .filter_map(|dir| std::path::absolute(dir.join(".")).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn locate(provider: &Provider) -> Result<Executable, Failure> {
    let cli = provider.cli;
    let mut directories = search_path();
    #[cfg(windows)]
    let path_end = directories.len();
    directories.extend(
        provider
            .install_dirs
            .iter()
            .filter_map(|(key, subdir)| absolute_env(key).map(|root| root.join(subdir))),
    );

    #[cfg(unix)]
    let executable = {
        directories
            .into_iter()
            .map(|dir| dir.join(cli))
            .find(|path| path.is_file())
            .map(|program| {
                Ok(Executable {
                    program,
                    prefix: Vec::new(),
                })
            })
    };
    #[cfg(windows)]
    let executable = windows::locate(cli, &directories, path_end);
    executable.unwrap_or_else(|| {
        Err(Failure::new(
            FailureKind::NotFound,
            format!("Could not find {cli} on PATH or in its install directories."),
        ))
    })
}

struct Located {
    executable: Result<Executable, Failure>,
    check: Mutex<Result<(), Failure>>,
}

type Inventory = BTreeMap<&'static str, Located>;

pub struct Discovery {
    client: Client,
    cache: OnceCell<Inventory>,
}

impl Discovery {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            cache: OnceCell::new(),
        }
    }

    async fn inventory(&self, cancel: &CancellationToken, activity: Activity) -> &Inventory {
        self.cache
            .get_or_init(|| async {
                let mut checks = JoinSet::new();
                for provider in providers::ALL
                    .iter()
                    .filter(|provider| provider.name != self.client.name())
                {
                    let cancel = cancel.clone();
                    let activity = activity.clone();
                    checks.spawn(async move {
                        let executable = tokio::task::spawn_blocking(move || locate(provider))
                            .await
                            .expect("resolver finished");
                        let check = match &executable {
                            Ok(exe) => probe(exe, &cancel, activity).await,
                            Err(error) => Err(error.clone()),
                        };
                        (
                            provider.name,
                            Located {
                                executable,
                                check: Mutex::new(check),
                            },
                        )
                    });
                }
                checks.join_all().await.into_iter().collect()
            })
            .await
    }

    pub async fn listed(
        &self,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> Vec<&'static str> {
        self.inventory(cancel, activity)
            .await
            .iter()
            .filter_map(|(&name, entry)| {
                (!matches!(&entry.executable, Err(error) if error.kind == FailureKind::NotFound))
                    .then_some(name)
            })
            .collect()
    }

    pub async fn resolve(
        &self,
        name: &str,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> Result<Executable, Failure> {
        let checked_before = self.cache.initialized();
        let inventory = self.inventory(cancel, activity.clone()).await;
        // The registry has already matched this tool against the visible provider declarations.
        let entry = &inventory[name];
        let executable = entry.executable.as_ref().map_err(Clone::clone)?;
        let mut check = entry.check.lock().await;
        // Only the request that started the shared scan can cancel its probes. An unrelated
        // request already waiting on that scan gets a fresh check under its own token.
        if let Err(previous) = &*check
            && (checked_before
                || (previous.kind == FailureKind::Cancellation && !cancel.is_cancelled()))
        {
            *check = probe(executable, cancel, activity).await;
        }
        check.clone().map(|()| executable.clone())
    }
}

async fn probe(
    exe: &Executable,
    cancel: &CancellationToken,
    activity: Activity,
) -> Result<(), Failure> {
    let mut args = exe.prefix.clone();
    args.push("--version".into());
    let launch = Launch {
        program: exe.program.clone(),
        args,
        cwd: None,
        env_set: crate::delegation::env_set(),
        env_remove: crate::delegation::env_remove(),
        deadline: Duration::from_secs(10),
        capture_limit: 2048,
        temporary_files: Vec::new(),
    };
    let outcome = supervise::run(launch, cancel, activity)
        .await
        .map_err(|err| {
            Failure::new(
                FailureKind::Launch,
                format!(
                    "Version check could not run {}: {err}",
                    exe.program.display()
                ),
            )
        })?;
    match outcome.ending {
        Ending::TimedOut => Err(Failure::new(
            FailureKind::Timeout,
            "Version check exceeded its deadline.",
        )),
        Ending::Cancelled => Err(Failure::new(
            FailureKind::Cancellation,
            "Version check was cancelled.",
        )),
        Ending::Exited
            if outcome.code == Some(0) && outcome.stdout.total + outcome.stderr.total > 0 =>
        {
            Ok(())
        }
        Ending::Exited => {
            let detail = if outcome.stderr.bytes.is_empty() {
                &outcome.stdout.bytes
            } else {
                &outcome.stderr.bytes
            };
            Err(Failure::new(
                FailureKind::Launch,
                format!(
                    "Version check for {} failed (exit {:?}): {}",
                    exe.program.display(),
                    outcome.code,
                    String::from_utf8_lossy(detail).trim()
                ),
            ))
        }
    }
}
