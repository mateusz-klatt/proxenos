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
    pub redacted: bool,
    pub scanned: bool,
}

pub enum CliStatus {
    Ready {
        program: PathBuf,
        version: String,
    },
    NotFound(Failure),
    #[cfg(windows)]
    UnsupportedLauncher(Failure),
    ProbeFailed {
        program: PathBuf,
        error: DiscoveryError,
    },
}

pub async fn inspect(
    provider: &'static Provider,
    cancel: &CancellationToken,
    activity: Activity,
) -> CliStatus {
    let located = tokio::task::spawn_blocking(move || locate(provider))
        .await
        .expect("resolver finished");
    let exe = match located {
        Ok(exe) => exe,
        Err(failure) => {
            #[cfg(windows)]
            if failure.kind == FailureKind::UnsupportedLauncher {
                return CliStatus::UnsupportedLauncher(failure);
            }
            return CliStatus::NotFound(failure);
        }
    };
    match probe(&exe, cancel, None, activity).await {
        Ok(version) => CliStatus::Ready {
            program: exe.program,
            version,
        },
        Err(error) => CliStatus::ProbeFailed {
            program: exe.program,
            error,
        },
    }
}

impl From<Failure> for DiscoveryError {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            diagnostic: None,
            redacted: false,
            scanned: false,
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
    locate_cli(provider.cli, provider.install_dirs)
}

fn locate_cli(cli: &str, install_dirs: &[(&str, &str)]) -> Result<Executable, Failure> {
    let mut directories = search_path();
    #[cfg(windows)]
    let path_end = directories.len();
    directories.extend(
        install_dirs
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

/// A shared account read also works for the calling client's own CLI.
pub async fn locate_for_usage(
    cli: &'static str,
    install_dirs: &'static [(&'static str, &'static str)],
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Executable, DiscoveryError> {
    let lookup = tokio::task::spawn_blocking(move || locate_cli(cli, install_dirs));
    wait_for(lookup, cancel, Some(deadline))
        .await?
        .expect("CLI lookup worker finished")
        .map_err(DiscoveryError::from)
}

struct Located {
    executable: Result<Executable, Failure>,
    check: Mutex<Result<(), DiscoveryError>>,
}

type ProviderSlot = watch::Receiver<Option<Arc<Located>>>;
type Slots = BTreeMap<&'static str, ProviderSlot>;

pub struct Discovery {
    client: Client,
    slots: StdMutex<Option<Slots>>,
}

impl Discovery {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            slots: StdMutex::new(None),
        }
    }

    fn slots(&self, cancel: &CancellationToken, activity: Activity) -> Slots {
        let mut guard = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slots) = &*guard {
            return slots.clone();
        }
        let scan_cancel = cancel.child_token();
        let mut slots = BTreeMap::new();
        for provider in providers::ALL
            .iter()
            .filter(|provider| provider.name != self.client.name())
        {
            let (publish, done) = watch::channel(None);
            let cancel = scan_cancel.clone();
            let activity = activity.clone();
            tokio::spawn(async move {
                let executable = tokio::task::spawn_blocking(move || locate(provider))
                    .await
                    .expect("resolver finished");
                let check = match &executable {
                    Ok(exe) => probe(exe, &cancel, None, activity.clone())
                        .await
                        .map(|_| ()),
                    Err(error) => Err(DiscoveryError::from(error.clone())),
                };
                let _ = publish.send(Some(Arc::new(Located {
                    executable,
                    check: Mutex::new(check),
                })));
                drop(activity);
            });
            slots.insert(provider.name, done);
        }
        *guard = Some(slots.clone());
        slots
    }

    pub async fn listed(
        &self,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> Vec<&'static str> {
        let slots = self.slots(cancel, activity);
        let mut visible = Vec::new();
        for (name, slot) in slots {
            let Ok((entry, _)) = await_slot(slot, cancel, None).await else {
                return Vec::new();
            };
            if !matches!(&entry.executable, Err(error) if error.kind == FailureKind::NotFound) {
                visible.push(name);
            }
        }
        visible
    }

    pub async fn resolve(
        &self,
        name: &str,
        cancel: &CancellationToken,
        deadline: Instant,
        activity: Activity,
    ) -> Result<Executable, DiscoveryError> {
        // The registry has already matched this tool against the visible provider declarations.
        let slot = self.slots(cancel, activity.clone())[name].clone();
        let (entry, checked_before) = await_slot(slot, cancel, Some(deadline)).await?;
        let executable = entry
            .executable
            .as_ref()
            .map_err(|error| DiscoveryError::from(error.clone()))?;
        let mut check = wait_for(entry.check.lock(), cancel, Some(deadline)).await?;
        // Only an explicit cancellation of the request that started the shared scan cancels its
        // probes. An unrelated caller waiting on that scan gets a fresh check under its own token.
        if let Err(previous) = &*check
            && (checked_before
                || (previous.failure.kind == FailureKind::Cancellation && !cancel.is_cancelled()))
        {
            *check = probe(executable, cancel, Some(deadline), activity)
                .await
                .map(|_| ());
        }
        check.clone().map(|()| executable.clone())
    }
}

async fn await_slot(
    mut done: ProviderSlot,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(Arc<Located>, bool), DiscoveryError> {
    let checked_before = done.borrow().is_some();
    let entry = wait_for(
        async {
            done.wait_for(Option::is_some)
                .await
                .expect("provider discovery completed")
                .clone()
                .expect("provider entry published")
        },
        cancel,
        deadline,
    )
    .await?;
    Ok((entry, checked_before))
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
) -> Result<String, DiscoveryError> {
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
        memory_limit_bytes: None,
        capture_limit: crate::envelope::CAPTURE_LIMIT,
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
            let version = [&outcome.stdout, &outcome.stderr]
                .into_iter()
                .filter_map(|stream| {
                    let raw = String::from_utf8_lossy(&stream.bytes);
                    crate::envelope::probe_version_excerpt(&raw, 2048, stream.truncated())
                })
                .next()
                .unwrap_or_default();
            Ok(version)
        }
        _ => {
            let detail = if outcome.stderr.bytes.is_empty() {
                &outcome.stdout
            } else {
                &outcome.stderr
            };
            let raw = String::from_utf8_lossy(&detail.bytes);
            let (diagnostic, redacted) =
                crate::envelope::diagnostic_excerpt(raw.trim(), 2048, detail.truncated());
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
                redacted,
                scanned: true,
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
