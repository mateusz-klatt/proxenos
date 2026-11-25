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
                "test_wait",
                "Waits for `ms` milliseconds or until cancelled.",
                json!({
                    "type": "object",
                    "properties": { "ms": { "type": "integer", "minimum": 0 } },
                    "required": ["ms"],
                    "additionalProperties": false
                }),
            ),
        ]
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
            "test_wait" => {
                let ms = arguments.get("ms").and_then(Value::as_u64).unwrap_or(0);
                // The tests read these lines to know the call is running, and how it ended.
                eprintln!("proxenos test_wait: started");
                let text = tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(ms)) => "waited",
                    () = cancel.cancelled() => "cancelled",
                };
                eprintln!("proxenos test_wait: {text}");
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
