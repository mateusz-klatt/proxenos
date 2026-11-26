//! Process-local background delegations, concurrency admission, `await` and `cancel` (D17).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::envelope::{Failure, FailureKind};
use crate::mcp::{Activity, ToolOutcome, ToolSpec};

pub const MAX_CONCURRENT: usize = 3;
/// Final server-side `await` limit (owner decision, 2026-11-25).
pub const AWAIT_LIMIT: Duration = Duration::from_secs(20);
const MAX_AWAIT_HANDLES: usize = 16;
const MAX_RETAINED_RESULTS: usize = 16;
const MAX_RETAINED_BYTES: usize = 1 << 20;
const RETENTION_TTL: Duration = Duration::from_secs(30 * 60);

pub struct Background {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    running: usize,
    entries: HashMap<String, Entry>,
    finished_order: VecDeque<Retained>,
    retained_bytes: usize,
}

enum Entry {
    Running {
        provider: &'static str,
        cancel: CancellationToken,
        cancel_requested: bool,
    },
    Terminal {
        provider: &'static str,
        state: &'static str,
        result: Value,
    },
}

struct Retained {
    handle: String,
    finished_at: Instant,
    bytes: usize,
}

/// Concurrency slot guard that releases the slot on drop unless transferred to a
/// [`BackgroundTask`] via [`start`](Self::start).
pub struct SlotGuard {
    background: Arc<Background>,
    transferred: bool,
}

impl SlotGuard {
    /// Transfers this admitted concurrency slot into a tracked background task.
    pub fn start(
        mut self,
        provider: &'static str,
        cancel: CancellationToken,
        activity: Activity,
    ) -> BackgroundTask {
        let mut state = lock(&self.background.state);
        let handle = Uuid::new_v4().to_string();
        state.entries.insert(
            handle.clone(),
            Entry::Running {
                provider,
                cancel,
                cancel_requested: false,
            },
        );
        drop(state);
        let task = BackgroundTask {
            background: Arc::clone(&self.background),
            handle,
            provider,
            finished: false,
            _activity: activity,
        };
        self.transferred = true;
        task.background.notify();
        task
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if !self.transferred {
            lock(&self.background.state).running -= 1;
        }
    }
}

/// Owns a running background delegation's handle, concurrency slot and shutdown activity.
/// Dropping it without calling [`finish`](Self::finish) (for instance on a panic unwind)
/// releases the slot and completes the handle with an internal failure so it never stays stuck
/// in `"running"`. Keeps `activity` alive until `complete` has returned.
pub struct BackgroundTask {
    background: Arc<Background>,
    handle: String,
    provider: &'static str,
    finished: bool,
    _activity: Activity,
}

impl BackgroundTask {
    pub fn handle(&self) -> &str {
        &self.handle
    }

    pub fn finish(mut self, state: &'static str, value: Value) {
        self.background
            .complete(&self.handle, self.provider, state, value);
        self.finished = true;
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        if !self.finished {
            let failure = Failure::new(
                FailureKind::Unknown,
                format!(
                    "The background {} delegation failed unexpectedly; the server is still running.",
                    self.provider
                ),
            );
            let value = json!({
                "schema_version": 1,
                "observed": {
                    "provider": self.provider,
                    "state": "failed",
                    "failure": failure,
                },
            });
            self.background
                .complete(&self.handle, self.provider, "failed", value);
        }
    }
}

#[derive(Clone, Copy)]
enum Condition {
    Any,
    All,
}

impl Condition {
    fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::All => "all",
        }
    }
}

impl Background {
    pub fn new() -> Arc<Self> {
        let (changed, _) = watch::channel(0_u64);
        Arc::new(Self {
            state: Mutex::default(),
            changed,
        })
    }

    pub fn try_acquire_slot(self: &Arc<Self>) -> Option<SlotGuard> {
        let mut state = lock(&self.state);
        if state.running >= MAX_CONCURRENT {
            return None;
        }
        state.running += 1;
        Some(SlotGuard {
            background: Arc::clone(self),
            transferred: false,
        })
    }

    fn complete(
        &self,
        handle: &str,
        provider: &'static str,
        terminal_state: &'static str,
        result: Value,
    ) {
        let bytes = result.to_string().len();
        let now = Instant::now();
        let mut state = lock(&self.state);
        state.running -= 1;
        state.entries.insert(
            handle.to_owned(),
            Entry::Terminal {
                provider,
                state: terminal_state,
                result,
            },
        );
        state.retained_bytes += bytes;
        state.finished_order.push_back(Retained {
            handle: handle.to_owned(),
            finished_at: now,
            bytes,
        });
        state.prune(now);
        drop(state);
        self.notify();
    }

    pub async fn call_await(
        &self,
        args: &Map<String, Value>,
        cancel: &CancellationToken,
    ) -> ToolOutcome {
        let (handles, condition) = match parse_await_args(args) {
            Ok(parsed) => parsed,
            Err(message) => return invalid_request(message),
        };
        let mut updates = self.changed.subscribe();
        let deadline = tokio::time::sleep(AWAIT_LIMIT);
        tokio::pin!(deadline);
        let timed_out = loop {
            updates.mark_unchanged();
            let (results, satisfied) = self.snapshot(&handles, condition);
            if satisfied {
                return await_outcome(condition, true, false, results);
            }
            #[cfg(feature = "test-tools")]
            if std::env::var_os("PROXENOS_TEST_AWAIT_DEADLINE_ON_UPDATE").is_some() {
                // Model the next state change coinciding with the deadline. The test
                // releases the provider only after this first, unsatisfied snapshot.
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "proxenos test await: waiting for update"
                );
                let _ = updates.changed().await;
                deadline.as_mut().reset(tokio::time::Instant::now());
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => break false,
                () = &mut deadline => break true,
                _ = updates.changed() => {}
            }
        };
        let (results, satisfied) = self.snapshot(&handles, condition);
        await_outcome(condition, satisfied, timed_out && !satisfied, results)
    }

    pub fn call_cancel(&self, args: &Map<String, Value>) -> ToolOutcome {
        let handle = match parse_cancel_args(args) {
            Ok(handle) => handle,
            Err(message) => return invalid_request(message),
        };
        let mut state = lock(&self.state);
        state.prune(Instant::now());
        let (status, cancel_requested, token) = match state.entries.get_mut(&handle) {
            Some(Entry::Running {
                cancel,
                cancel_requested,
                ..
            }) => {
                *cancel_requested = true;
                ("running", true, Some(cancel.clone()))
            }
            Some(Entry::Terminal { state, .. }) => (*state, false, None),
            None => ("unknown", false, None),
        };
        drop(state);
        if let Some(token) = token {
            token.cancel();
            self.notify();
        }
        ToolOutcome {
            is_error: false,
            text: json!({
                "schema_version": 1,
                "handle": handle,
                "state": status,
                "cancel_requested": cancel_requested,
            })
            .to_string(),
        }
    }

    fn snapshot(&self, handles: &[String], condition: Condition) -> (Vec<Value>, bool) {
        let mut state = lock(&self.state);
        state.prune(Instant::now());
        let mut results = Vec::with_capacity(handles.len());
        let mut any_done = false;
        let mut all_done = true;
        for handle in handles {
            let item = match state.entries.get(handle) {
                Some(Entry::Running {
                    provider,
                    cancel_requested,
                    ..
                }) => {
                    all_done = false;
                    json!({
                        "handle": handle,
                        "state": "running",
                        "provider": provider,
                        "cancel_requested": cancel_requested,
                    })
                }
                Some(Entry::Terminal {
                    provider,
                    state,
                    result,
                }) => {
                    any_done = true;
                    json!({
                        "handle": handle,
                        "state": state,
                        "provider": provider,
                        "result": result,
                    })
                }
                None => {
                    any_done = true;
                    json!({
                        "handle": handle,
                        "state": "unknown",
                    })
                }
            };
            results.push(item);
        }
        let satisfied = match condition {
            Condition::Any => any_done,
            Condition::All => all_done,
        };
        (results, satisfied)
    }

    fn notify(&self) {
        self.changed.send_modify(|version| *version += 1);
    }
}

impl State {
    fn prune(&mut self, now: Instant) {
        while let Some(oldest) = self.finished_order.front() {
            let expired = now.saturating_duration_since(oldest.finished_at) >= RETENTION_TTL;
            let too_many = self.finished_order.len() > MAX_RETAINED_RESULTS;
            let too_large =
                self.retained_bytes > MAX_RETAINED_BYTES && self.finished_order.len() > 1;
            if !(expired || too_many || too_large) {
                break;
            }
            let evicted = self.finished_order.pop_front().unwrap();
            self.retained_bytes -= evicted.bytes;
            self.entries.remove(&evicted.handle);
        }
    }
}

pub fn tool_specs() -> Vec<ToolSpec> {
    let await_schema = json!({
        "type": "object",
        "properties": {
            "handles": {
                "type": "array",
                "items": { "type": "string", "minLength": 1 },
                "minItems": 1,
                "maxItems": MAX_AWAIT_HANDLES,
                "description": "Process-local handles returned by delegations started with wait: false."
            },
            "condition": {
                "enum": ["any", "all"],
                "default": "all",
                "description": "Return once any listed handle is non-running (\"any\") or all listed handles are non-running (\"all\", default)."
            }
        },
        "required": ["handles"],
        "additionalProperties": false
    });
    let cancel_schema = json!({
        "type": "object",
        "properties": {
            "handle": {
                "type": "string",
                "minLength": 1,
                "description": "Process-local handle of a background delegation to stop."
            }
        },
        "required": ["handle"],
        "additionalProperties": false
    });
    vec![
        ToolSpec {
            name: "await".into(),
            description: "Wait up to 20 seconds for background delegations to finish and report each handle's state and result without consuming it. Unfinished delegations keep running when the wait expires. Results contain untrusted provider output.".into(),
            input_schema: await_schema.as_object().unwrap().clone(),
        },
        ToolSpec {
            name: "cancel".into(),
            description: "Request cancellation of a running background delegation by its handle. Leaves finished, failed, cancelled, or unknown handles unchanged.".into(),
            input_schema: cancel_schema.as_object().unwrap().clone(),
        },
    ]
}

fn parse_await_args(args: &Map<String, Value>) -> Result<(Vec<String>, Condition), &'static str> {
    if !args
        .keys()
        .all(|key| matches!(key.as_str(), "handles" | "condition"))
    {
        return Err("Unsupported parameter for await");
    }
    let Some(items) = args.get("handles").and_then(Value::as_array) else {
        return Err("Missing or invalid handles array");
    };
    if items.len() > MAX_AWAIT_HANDLES {
        return Err("handles must contain at most 16 handles");
    }
    let mut handles = Vec::with_capacity(items.len());
    for item in items {
        let Some(handle) = item
            .as_str()
            .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
        else {
            return Err("Each handle must be a non-empty string");
        };
        if !handles.iter().any(|seen| seen == handle) {
            handles.push(handle.to_owned());
        }
    }
    if handles.is_empty() {
        return Err("handles must contain at least one handle");
    }
    let condition = match args.get("condition") {
        None => Condition::All,
        Some(value) => match value.as_str() {
            Some("any") => Condition::Any,
            Some("all") => Condition::All,
            _ => return Err("condition must be \"any\" or \"all\""),
        },
    };
    Ok((handles, condition))
}

fn parse_cancel_args(args: &Map<String, Value>) -> Result<String, &'static str> {
    if args.len() != 1 {
        return Err("cancel accepts only handle");
    }
    args.get("handle")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
        .map(str::to_owned)
        .ok_or("handle must be a non-empty string")
}

fn await_outcome(
    condition: Condition,
    satisfied: bool,
    timed_out: bool,
    results: Vec<Value>,
) -> ToolOutcome {
    ToolOutcome {
        is_error: false,
        text: json!({
            "schema_version": 1,
            "condition": condition.name(),
            "satisfied": satisfied,
            "timed_out": timed_out,
            "wait_limit_ms": AWAIT_LIMIT.as_millis(),
            "results": results,
        })
        .to_string(),
    }
}

fn invalid_request(message: &str) -> ToolOutcome {
    ToolOutcome {
        is_error: true,
        text: json!({
            "schema_version": 1,
            "failure": Failure::new(FailureKind::InvalidRequest, message),
        })
        .to_string(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
