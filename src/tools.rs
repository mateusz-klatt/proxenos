//! Routes the client's installed providers and the optional protocol test tools.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::background::{self, Background};
use crate::client::Client;
use crate::discovery::Discovery;
use crate::mcp::{Activity, Service, ToolOutcome, ToolSpec, UnknownTool};
use crate::providers::{self, RememberedStart};

const MAX_STARTS: usize = 1024;

fn start_limit() -> usize {
    #[cfg(feature = "test-tools")]
    if let Some(limit) = std::env::var("PROXENOS_TEST_START_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return limit;
    }
    MAX_STARTS
}

/// State of a cached D19 start entry in [`StartStore`].
///
/// Only [`StartEntry::Known`] is dispatchable from memory; it is reached solely after a
/// confirmed `SaveOutcome::Saved` (or `Unconfigured`) or a matching disk load.
#[derive(Clone, PartialEq, Eq)]
pub enum StartEntry {
    Known(RememberedStart),
    Unverified(RememberedStart),
    Ambiguous,
}

/// Bounded in-memory D19 cache in front of the durable session files. Oldest entries are evicted
/// past `limit`.
pub struct StartStore {
    entries: HashMap<(&'static str, String), StartEntry>,
    order: VecDeque<(&'static str, String)>,
    limit: usize,
}

impl StartStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            limit: start_limit(),
        }
    }

    pub fn get(&self, provider: &'static str, id: &str) -> Option<&StartEntry> {
        self.entries.get(&(provider, id.into()))
    }

    fn insert(&mut self, key: (&'static str, String), value: StartEntry) {
        if self.entries.len() == self.limit {
            let oldest = self.order.pop_front().expect("bounded store is non-empty");
            self.entries.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    pub fn set(&mut self, provider: &'static str, id: String, value: StartEntry) {
        let key = (provider, id);
        if let Some(slot) = self.entries.get_mut(&key) {
            *slot = value;
            return;
        }
        self.insert(key, value);
    }

    pub fn conflicts_with(
        &self,
        provider: &'static str,
        id: &str,
        remembered: &RememberedStart,
    ) -> bool {
        matches!(self.get(provider, id), Some(StartEntry::Ambiguous))
            || matches!(
                self.get(provider, id),
                Some(StartEntry::Known(existing) | StartEntry::Unverified(existing))
                    if existing != remembered
            )
    }

    pub fn record_unverified(
        &mut self,
        provider: &'static str,
        id: String,
        remembered: RememberedStart,
    ) {
        let next = if self.conflicts_with(provider, &id, &remembered) {
            StartEntry::Ambiguous
        } else {
            StartEntry::Unverified(remembered)
        };
        self.set(provider, id, next);
    }

    pub fn promote(&mut self, provider: &'static str, id: &str, remembered: &RememberedStart) {
        if let Some(slot) = self.entries.get_mut(&(provider, id.into()))
            && matches!(slot, StartEntry::Unverified(existing) if existing == remembered)
        {
            *slot = StartEntry::Known(remembered.clone());
        }
    }
}

type SessionKey = (&'static str, String);
type KeyLock = tokio::sync::Mutex<()>;

/// Serialises D19 session saves and reply lookups per `(provider, thread_id)` inside the process.
#[derive(Default)]
pub struct SessionLocks {
    locks: Mutex<HashMap<SessionKey, Weak<KeyLock>>>,
}

impl SessionLocks {
    pub fn for_key(&self, provider: &'static str, id: &str) -> Arc<KeyLock> {
        let mut map = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, weak| weak.strong_count() > 0);
        let key = (provider, id.to_owned());
        map.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            map.insert(key, Arc::downgrade(&lock));
            lock
        })
    }
}

pub struct Registry {
    client: Client,
    pub discovery: Discovery,
    pub answers: crate::envelope::AnswerArchive,
    pub starts: Arc<Mutex<StartStore>>,
    pub session_locks: SessionLocks,
    pub background: Arc<Background>,
}

impl Registry {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            discovery: Discovery::new(client),
            answers: Default::default(),
            starts: Arc::new(Mutex::new(StartStore::new())),
            session_locks: SessionLocks::default(),
            background: Background::new(),
        }
    }
}

impl Service for Registry {
    async fn tools(&self, cancel: &CancellationToken, activity: Activity) -> Vec<ToolSpec> {
        let installed = self.discovery.listed(cancel, activity).await;
        let mut tools = extra_tools();
        for provider in providers::ALL
            .iter()
            .filter(|p| installed.contains(&p.name))
        {
            tools.extend([provider.tool(false), provider.tool(true)]);
        }
        tools.extend(background::tool_specs());
        tools
    }

    async fn call(
        self: Arc<Self>,
        name: String,
        arguments: Map<String, Value>,
        cancel: CancellationToken,
        activity: Activity,
    ) -> Result<ToolOutcome, UnknownTool> {
        if name == "await" {
            return Ok(self.background.call_await(&arguments, &cancel).await);
        }
        if name == "cancel" {
            return Ok(self.background.call_cancel(&arguments));
        }
        let bare = name.strip_suffix("-reply").unwrap_or(&name);
        if let Some(provider) = providers::ALL
            .iter()
            .find(|p| p.name == bare && bare != self.client.name())
        {
            return Ok(self
                .delegate(provider, bare != name, arguments, cancel, activity)
                .await);
        }
        extra_call(&name, &arguments, &cancel, activity, &self.background)
            .await
            .ok_or(UnknownTool)
    }
}

#[cfg(feature = "test-tools")]
fn extra_tools() -> Vec<ToolSpec> {
    test_tools::specs()
}

#[cfg(not(feature = "test-tools"))]
fn extra_tools() -> Vec<ToolSpec> {
    Vec::new()
}

#[cfg(feature = "test-tools")]
async fn extra_call(
    name: &str,
    arguments: &Map<String, Value>,
    cancel: &CancellationToken,
    activity: Activity,
    background: &Arc<Background>,
) -> Option<ToolOutcome> {
    test_tools::call(name, arguments, cancel, activity, background).await
}

#[cfg(not(feature = "test-tools"))]
async fn extra_call(
    _name: &str,
    _arguments: &Map<String, Value>,
    _cancel: &CancellationToken,
    _activity: Activity,
    _background: &Arc<Background>,
) -> Option<ToolOutcome> {
    None
}

#[cfg(feature = "test-tools")]
mod test_tools {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Map, Value, json};
    use tokio_util::sync::CancellationToken;

    use crate::background::Background;
    use crate::mcp::{Activity, ToolOutcome, ToolSpec};
    use crate::supervise::{self, Finished, Launch};

    fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: schema.as_object().cloned().unwrap_or_default(),
        }
    }

    pub fn specs() -> Vec<ToolSpec> {
        let open = json!({ "type": "object" });
        vec![
            spec("test_echo", "Returns its arguments as JSON.", open.clone()),
            spec("test_fail", "Fails as a tool.", open.clone()),
            spec("test_panic", "Panics.", open),
            spec(
                "test_run",
                "Runs a program under the supervisor and returns what it left, as JSON.",
                json!({
                    "type": "object",
                    "properties": {
                        "program": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "cwd": { "type": "string" },
                        "env": { "type": "object", "additionalProperties": { "type": "string" } },
                        "env_remove": { "type": "array", "items": { "type": "string" } },
                        "deadline_ms": { "type": "integer", "minimum": 0 },
                        "memory_limit_bytes": { "type": "integer", "minimum": 1 },
                        "capture_limit": { "type": "integer", "minimum": 0 },
                        "temp_file": { "type": "string", "description": "Create a new temporary file for the child at this path." },
                        "sharing_retry_ms": { "type": "integer", "minimum": 0, "description": "Windows sharing-violation retry budget for the temporary file, in milliseconds (default 1000)." },
                        "on_cancel": { "enum": ["start", "drop"], "description": "Start with the cancelled token, or drop the running supervisor when the call is cancelled." }
                    },
                    "required": ["program", "deadline_ms", "capture_limit"]
                }),
            ),
            spec(
                "test_wait",
                "Waits for `ms` milliseconds or until cancelled, then for `linger_ms` more.",
                json!({
                    "type": "object",
                    "properties": {
                        "ms": { "type": "integer", "minimum": 0 },
                        "linger_ms": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["ms"],
                    "additionalProperties": false
                }),
            ),
        ]
    }

    fn strings(value: Option<&Value>) -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn millis(arguments: &Map<String, Value>, name: &str) -> Option<u64> {
        arguments.get(name).and_then(Value::as_u64)
    }

    async fn run(
        arguments: &Map<String, Value>,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> std::io::Result<Finished> {
        let text = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let env = arguments.get("env").and_then(Value::as_object);
        let started = std::time::Instant::now();
        let mut temporary_files = Vec::new();
        if let Some(path) = text("temp_file") {
            let file = supervise::cleanup::TemporaryFile::create(
                path.into(),
                #[cfg(windows)]
                Duration::from_millis(millis(arguments, "sharing_retry_ms").unwrap_or(1000)),
            )
            .await?;
            temporary_files.push(file);
        }
        let launch = Launch {
            program: text("program").unwrap_or_default().into(),
            args: strings(arguments.get("args"))
                .into_iter()
                .map(Into::into)
                .collect(),
            cwd: text("cwd").map(Into::into),
            env_set: env
                .into_iter()
                .flatten()
                .map(|(name, value)| (name.into(), value.as_str().unwrap_or_default().into()))
                .collect(),
            env_remove: strings(arguments.get("env_remove"))
                .into_iter()
                .map(Into::into)
                .collect(),
            deadline: started
                + Duration::from_millis(millis(arguments, "deadline_ms").unwrap_or(0)),
            memory_limit_bytes: millis(arguments, "memory_limit_bytes"),
            capture_limit: usize::try_from(millis(arguments, "capture_limit").unwrap_or(0))
                .unwrap_or(usize::MAX),
            temporary_files,
        };
        let on_cancel = arguments.get("on_cancel").and_then(Value::as_str);
        if on_cancel == Some("start") {
            cancel.cancelled().await;
        }
        // In drop mode the client abandons a live supervisor without asking it to stop.
        let supervisor_cancel = if on_cancel == Some("drop") {
            CancellationToken::new()
        } else {
            cancel.clone()
        };
        let mut finished = tokio::select! {
            biased;
            result = supervise::run(launch, &supervisor_cancel, activity) => result,
            () = cancel.cancelled(), if on_cancel == Some("drop") => {
                Err(std::io::Error::other("caller dropped the supervisor"))
            }
        }?;
        finished.error.take().map_or(Ok(finished), Err)
    }

    fn run_outcome(result: std::io::Result<Finished>) -> ToolOutcome {
        match result {
            Ok(finished) => ToolOutcome {
                text: json!({
                    "ending": finished.ending.name(),
                    "code": finished.code,
                    "signal": finished.signal,
                    "stdout": String::from_utf8_lossy(&finished.stdout.bytes),
                    "stdout_bytes": finished.stdout.bytes,
                    "stdout_total": finished.stdout.total,
                    "stderr": String::from_utf8_lossy(&finished.stderr.bytes),
                    "stderr_bytes": finished.stderr.bytes,
                    "stderr_total": finished.stderr.total,
                    "elapsed_ms": finished.elapsed.as_millis(),
                    "last_output_ms": finished.last_output.map(|at| at.as_millis()),
                    "peak_memory_bytes": finished.accounting.peak_memory_bytes,
                })
                .to_string(),
                is_error: false,
            },
            Err(error) => ToolOutcome {
                text: format!("supervision failed: {error}"),
                is_error: true,
            },
        }
    }

    pub async fn call(
        name: &str,
        arguments: &Map<String, Value>,
        cancel: &CancellationToken,
        activity: Activity,
        background: &Arc<Background>,
    ) -> Option<ToolOutcome> {
        let outcome = match name {
            "test_echo" => ToolOutcome {
                text: Value::Object(arguments.clone()).to_string(),
                is_error: false,
            },
            "test_fail" => ToolOutcome {
                text: "failed on purpose".to_owned(),
                is_error: true,
            },
            "test_panic" if arguments.get("background") == Some(&Value::Bool(true)) => {
                let bg_cancel = activity.shutdown_token();
                let task = background
                    .try_start("test", bg_cancel, activity)
                    .expect("slot available for background test_panic");
                let handle = task.handle().to_owned();
                tokio::spawn(async move {
                    let _guard = task;
                    panic!("test_panic background was called");
                });
                ToolOutcome {
                    text: json!({ "handle": handle }).to_string(),
                    is_error: false,
                }
            }
            "test_panic" => panic!("test_panic was called"),
            "test_run" => run_outcome(run(arguments, cancel, activity).await),
            "test_wait" => {
                let ms = arguments.get("ms").and_then(Value::as_u64).unwrap_or(0);
                // The tests read these lines to know the call is running, and how it ended.
                eprintln!("proxenos test_wait: started");
                let text = tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(ms)) => "waited",
                    () = cancel.cancelled() => "cancelled",
                };
                eprintln!("proxenos test_wait: {text}");
                // A cleanup that takes time, as stopping a child does.
                let linger = arguments
                    .get("linger_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(linger)).await;
                eprintln!("proxenos test_wait: done");
                ToolOutcome {
                    text: text.to_owned(),
                    is_error: false,
                }
            }
            _ => return None,
        };
        Some(outcome)
    }
}
