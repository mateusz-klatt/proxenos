//! The tools that `proxenos mcp` serves.
//!
//! Providers are added in later steps of milestone 1. Until then the server lists only the
//! tools of the `test-tools` feature, which exists so that the MCP layer can be tested through
//! the compiled binary. A release is built without it.

use std::sync::Arc;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::mcp::{Service, ToolOutcome, ToolSpec, UnknownTool};

pub struct Registry;

impl Service for Registry {
    fn tools(&self) -> Vec<ToolSpec> {
        extra_tools()
    }

    async fn call(
        self: Arc<Self>,
        name: String,
        arguments: Map<String, Value>,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome, UnknownTool> {
        extra_call(&name, &arguments, &cancel)
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
) -> Option<ToolOutcome> {
    test_tools::call(name, arguments, cancel).await
}

#[cfg(not(feature = "test-tools"))]
async fn extra_call(
    _name: &str,
    _arguments: &Map<String, Value>,
    _cancel: &CancellationToken,
) -> Option<ToolOutcome> {
    None
}

#[cfg(feature = "test-tools")]
mod test_tools {
    use std::time::Duration;

    use serde_json::{Map, Value, json};
    use tokio_util::sync::CancellationToken;

    use crate::mcp::{ToolOutcome, ToolSpec};
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
                        "capture_limit": { "type": "integer", "minimum": 0 },
                        "temp_file": { "type": "string", "description": "Create a new temporary file for the child at this path." }
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
    ) -> std::io::Result<Finished> {
        let text = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let env = arguments.get("env").and_then(Value::as_object);
        let mut temporary_files = Vec::new();
        if let Some(path) = text("temp_file") {
            std::fs::File::create_new(&path)?;
            temporary_files.push(path.into());
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
            deadline: Duration::from_millis(millis(arguments, "deadline_ms").unwrap_or(0)),
            capture_limit: usize::try_from(millis(arguments, "capture_limit").unwrap_or(0))
                .unwrap_or(usize::MAX),
            temporary_files,
        };
        supervise::run(launch, cancel).await
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
            "test_panic" => panic!("test_panic was called"),
            "test_run" => run_outcome(run(arguments, cancel).await),
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
