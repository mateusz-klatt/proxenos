//! Resolve provider CLIs once per server. Version checks use the same process ownership and
//! cancellation as a turn. A negative check can recover on the next call without a restart.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, watch};
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

#[derive(Clone)]
pub struct DiscoveryError {
    pub failure: Failure,
    pub diagnostic: Option<String>,
}

impl From<Failure> for DiscoveryError {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            diagnostic: None,
        }
    }
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
                .filter(|dir| dir.is_absolute())
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
    check: Mutex<Result<(), DiscoveryError>>,
}

type Inventory = BTreeMap<&'static str, Located>;

pub struct Discovery {
    client: Client,
    scan: StdMutex<Option<watch::Receiver<Option<Arc<Inventory>>>>>,
}

impl Discovery {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            scan: StdMutex::new(None),
        }
    }

    fn scan(
        &self,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> watch::Receiver<Option<Arc<Inventory>>> {
        let mut slot = self.scan.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(done) = &*slot {
            return done.clone();
        }
        let (publish, done) = watch::channel(None);
        let scan_cancel = cancel.child_token();
        let client = self.client;
        tokio::spawn(async move {
            let mut checks = JoinSet::new();
            for provider in providers::ALL
                .iter()
                .filter(|provider| provider.name != client.name())
            {
                let cancel = scan_cancel.clone();
                let activity = activity.clone();
                checks.spawn(async move {
                    let executable = tokio::task::spawn_blocking(move || locate(provider))
                        .await
                        .expect("resolver finished");
                    let check = match &executable {
                        Ok(exe) => probe(exe, &cancel, None, activity).await,
                        Err(error) => Err(DiscoveryError::from(error.clone())),
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
            let inventory: Inventory = checks.join_all().await.into_iter().collect();
            let _ = publish.send(Some(Arc::new(inventory)));
            drop(activity);
        });
        *slot = Some(done.clone());
        done
    }

    async fn inventory(
        &self,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
        activity: Activity,
    ) -> Result<(Arc<Inventory>, bool), DiscoveryError> {
        let mut done = self.scan(cancel, activity);
        let checked_before = done.borrow().is_some();
        let inventory = wait_for(
            async {
                done.wait_for(Option::is_some)
                    .await
                    .expect("discovery scan completed")
                    .clone()
                    .expect("inventory published")
            },
            cancel,
            deadline,
        )
        .await?;
        Ok((inventory, checked_before))
    }

    pub async fn listed(
        &self,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> Vec<&'static str> {
        let Ok((inventory, _)) = self.inventory(cancel, None, activity).await else {
            return Vec::new();
        };
        inventory
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
        deadline: Instant,
        activity: Activity,
    ) -> Result<Executable, DiscoveryError> {
        let (inventory, checked_before) = self
            .inventory(cancel, Some(deadline), activity.clone())
            .await?;
        // The registry has already matched this tool against the visible provider declarations.
        let entry = &inventory[name];
        let executable = entry
            .executable
            .as_ref()
            .map_err(|error| DiscoveryError::from(error.clone()))?;
        let mut check = wait_for(entry.check.lock(), cancel, Some(deadline)).await?;
        // Only the request that started the shared scan can cancel its probes. An unrelated
        // request already waiting on that scan gets a fresh check under its own token.
        if let Err(previous) = &*check
            && (checked_before
                || (previous.failure.kind == FailureKind::Cancellation && !cancel.is_cancelled()))
        {
            *check = probe(executable, cancel, Some(deadline), activity).await;
        }
        check.clone().map(|()| executable.clone())
    }
}

async fn wait_for<T>(
    fut: impl Future<Output = T>,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<T, DiscoveryError> {
    let timeout = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(DiscoveryError::from(Failure::new(
            FailureKind::Cancellation,
            "Version check was cancelled.",
        ))),
        () = timeout => Err(DiscoveryError::from(Failure::new(
            FailureKind::Timeout,
            "Version check exceeded its deadline.",
        ))),
        value = fut => Ok(value),
    }
}

async fn probe(
    exe: &Executable,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
    activity: Activity,
) -> Result<(), DiscoveryError> {
    let mut args = exe.prefix.clone();
    args.push("--version".into());
    let limit = Instant::now() + Duration::from_secs(10);
    let launch = Launch {
        program: exe.program.clone(),
        args,
        cwd: None,
        env_set: crate::delegation::env_set(),
        env_remove: crate::delegation::env_remove(),
        deadline: deadline.map_or(limit, |call| call.min(limit)),
        capture_limit: 2048,
        temporary_files: Vec::new(),
    };
    let outcome = supervise::run(launch, cancel, activity)
        .await
        .and_then(|mut done| done.error.take().map_or(Ok(done), Err))
        .map_err(|err| {
            DiscoveryError::from(Failure::new(
                FailureKind::Launch,
                format!(
                    "Version check could not run {}: {err}",
                    exe.program.display()
                ),
            ))
        })?;
    match outcome.ending {
        Ending::TimedOut => Err(DiscoveryError::from(Failure::new(
            FailureKind::Timeout,
            "Version check exceeded its deadline. On macOS, first-run Gatekeeper (AppleSystemPolicy) evaluation can exceed this limit; the next call retries the check.",
        ))),
        Ending::Cancelled => Err(DiscoveryError::from(Failure::new(
            FailureKind::Cancellation,
            "Version check was cancelled.",
        ))),
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
            let diagnostic = String::from_utf8_lossy(detail).trim().to_owned();
            let status = exit_description(outcome.code, outcome.signal);
            Err(DiscoveryError {
                failure: Failure::new(
                    FailureKind::Launch,
                    format!(
                        "Version check for {} failed ({status}).",
                        exe.program.display(),
                    ),
                ),
                diagnostic: (!diagnostic.is_empty()).then_some(diagnostic),
            })
        }
    }
}

pub(crate) fn exit_description(code: Option<i32>, signal: Option<i32>) -> String {
    #[cfg(unix)]
    if let Some(sig) = signal {
        return format!("signal {sig}");
    }
    #[cfg(not(unix))]
    let _ = signal;
    format!("exit code {}", code.unwrap_or_default())
}
