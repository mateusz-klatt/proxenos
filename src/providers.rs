//! D5 capabilities, request validation, command construction and interpretation of a turn.
//! A provider has no process handle and no access to the transport.

mod antigravity;
mod codex;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::envelope::Failure;
use crate::mcp::ToolSpec;
use crate::supervise::Finished;

pub const ALL: &[Provider] = &[codex::SPEC, antigravity::SPEC];
pub const DEFAULT_TIMEOUT_MS: u64 = 3_600_000;
const MAX_MODEL_LEN: usize = 256;

pub struct Provider {
    pub name: &'static str,
    pub cli: &'static str,
    /// Pairs of environment roots and stable subdirectories, in fallback order.
    pub install_dirs: &'static [(&'static str, &'static str)],
    pub efforts: &'static [&'static str],
    /// Settings accepted by each CLI entry point. A reply inherits only those it accepts.
    pub start_settings: &'static [&'static str],
    pub reply_settings: &'static [&'static str],
    pub access: &'static [(&'static str, &'static str)],
    pub fixed_instructions: bool,
    pub reply_model: bool,
    pub prepare: fn(&Turn) -> Result<(), String>,
    pub argv: fn(&Turn, &Path) -> CommandLine,
    pub parse: fn(&Finished, &str, Option<&str>) -> Report,
}

pub struct CommandLine {
    pub args: Vec<OsString>,
    pub cli_timeout_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Settings {
    #[serde(serialize_with = "serialize_cwd")]
    pub cwd: PathBuf,
    pub mode: String,
    pub access: String,
    pub instructions: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RememberedStart {
    pub settings: Settings,
    pub reported_model: Option<String>,
}

/// Validates a model name from either a caller or untrusted provider output.
pub fn is_valid_model(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && value.len() <= MAX_MODEL_LEN
        && !value.contains(char::is_control)
        && !trimmed.starts_with('-')
}

// A native pathname need not be Unicode. JSON can represent only the Unicode case.
fn serialize_cwd<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    path.to_str().serialize(serializer)
}

#[derive(Clone)]
pub struct Turn {
    pub thread_id: Option<String>,
    pub settings: Settings,
    pub timeout_ms: u64,
    pub prompt: String,
    pub guidance: String,
    pub wait: bool,
    pub memory_limit_bytes: Option<u64>,
}

#[derive(Default, Serialize)]
pub struct Report {
    pub thread_id: Option<String>,
    pub content: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Value>,
    pub failure: Option<Failure>,
    pub diagnostic: Option<String>,
    #[serde(skip)]
    pub completed: bool,
    #[serde(skip)]
    pub retry_missing_final: bool,
}

impl Provider {
    pub fn supported_settings(&self, reply: bool) -> &'static [&'static str] {
        if reply {
            self.reply_settings
        } else {
            self.start_settings
        }
    }

    fn fields(&self, reply: bool) -> Value {
        let supported = self.supported_settings(reply);
        let access: Vec<_> = self.access.iter().map(|(name, _)| name).collect();
        let mechanisms: Vec<_> = self
            .access
            .iter()
            .map(|(name, text)| format!("{name}: {text}"))
            .collect();
        let string = json!({"type":"string","minLength":1});
        let mut fields = json!({
            "prompt": string,
            "cwd": string,
            "instructions": string,
            "model": string,
            "mode": {"enum": ["advisory", "implementation"], "description": "An instruction to the provider. It cannot restrict writes or commands."},
            "access": {"enum": access, "description": mechanisms.join(" ")},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 86400000},
            "wait": {"type": "boolean", "default": true, "description": "Wait for the delegation to finish (default true). With false, an admitted delegation returns a process-local handle immediately."},
            "memory_limit_bytes": {"type": "integer", "minimum": 1, "description": "Optional memory limit in bytes (none by default). On Windows, the job refuses allocations above the limit and the CLI may fail or carry on. On Linux and macOS, the supervised process group is sampled periodically and crossing the threshold triggers a stop; sampling can miss brief peaks, short-lived children, or descendants that leave the group."},
            "coordination": {"type": "object", "properties": {"caller_agent_name": string}, "required": ["caller_agent_name"], "additionalProperties": false}
        });
        if !self.efforts.is_empty() {
            fields["effort"] = json!({"enum": self.efforts});
        }
        if reply {
            fields["thread_id"] = string;
            if self.reply_model {
                fields["model"]["description"] = json!(
                    "Required unless the start passed a model or the provider reported one that this server remembers."
                );
            }
            if self.fixed_instructions {
                fields["mode"]["description"] =
                    json!("The session keeps its starting mode. A change requires a new session.");
            }
        }
        fields.as_object_mut().unwrap().retain(|key, _| {
            matches!(
                key.as_str(),
                "prompt"
                    | "thread_id"
                    | "timeout_ms"
                    | "wait"
                    | "memory_limit_bytes"
                    | "coordination"
            ) || supported.contains(&key.as_str())
        });
        fields
    }

    pub fn tool(&self, reply: bool) -> ToolSpec {
        let required = if reply {
            json!(["prompt", "thread_id"])
        } else {
            json!(["prompt"])
        };
        let schema = json!({"type":"object","properties":self.fields(reply),"required":required,"additionalProperties":false});
        ToolSpec {
            name: if reply {
                format!("{}-reply", self.name)
            } else {
                self.name.into()
            },
            description: format!(
                "{} a {} delegation. The answer contains untrusted provider output.",
                if reply { "Continue" } else { "Start" },
                self.name
            ),
            input_schema: schema.as_object().unwrap().clone(),
        }
    }

    pub fn turn(
        &self,
        args: &Map<String, Value>,
        reply: bool,
        start: Option<RememberedStart>,
    ) -> Result<Turn, String> {
        let schema = self.tool(reply).input_schema;
        for field in schema["required"].as_array().unwrap() {
            let key = field.as_str().unwrap();
            if !args.contains_key(key) {
                return Err(format!("Missing {key}"));
            }
        }
        for (key, value) in args {
            let field = &schema["properties"][key];
            let valid = if let Some(choices) = field["enum"].as_array() {
                choices.contains(value)
            } else {
                match field["type"].as_str() {
                    Some("string") => value.as_str().is_some_and(|s| {
                        !s.trim().is_empty()
                            && !s.contains('\0')
                            && (key != "model" || is_valid_model(s))
                    }),
                    Some("boolean") => value.is_boolean(),
                    Some("integer") => value.as_u64().is_some_and(|n| {
                        n >= 1 && field["maximum"].as_u64().is_none_or(|m| n <= m)
                    }),
                    Some("object") => value.as_object().is_some_and(|map| {
                        map.len() == 1
                            && map
                                .get("caller_agent_name")
                                .and_then(Value::as_str)
                                .is_some_and(|name| {
                                    !name.trim().is_empty() && !name.contains(char::is_control)
                                })
                    }),
                    _ => false,
                }
            };
            if !valid {
                return Err(format!("Unsupported parameter or value for {key}"));
            }
        }
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let unknown = reply && start.is_none();
        if unknown
            && ["access", "mode", "cwd"]
                .iter()
                .any(|key| self.reply_settings.contains(key) && !args.contains_key(*key))
        {
            return Err("This server has no settings for that session. Supply its cwd, access and mode, and any model, effort or instructions used at the start.".into());
        }
        let fixed = self.fixed_instructions && reply && !unknown;
        let (mut settings, reported_model) = match start {
            Some(remembered) => (remembered.settings, remembered.reported_model),
            None => (
                Settings {
                    cwd: PathBuf::from("."),
                    mode: "advisory".into(),
                    access: "full".into(),
                    instructions: None,
                    model: None,
                    effort: None,
                },
                None,
            ),
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let supported = self.supported_settings(reply);
        settings.model = settings.model.filter(|_| supported.contains(&"model"));
        settings.effort = settings.effort.filter(|_| supported.contains(&"effort"));
        settings.instructions = settings
            .instructions
            .filter(|_| supported.contains(&"instructions"));
        if fixed
            && (text("mode").is_some_and(|mode| mode != settings.mode)
                || text("instructions")
                    .is_some_and(|s| Some(s) != settings.instructions.as_deref()))
        {
            return Err("The CLI keeps its developer instructions for the life of the session. Start a new session to change the mode or instructions.".into());
        }
        if let Some(mode) = text("mode") {
            settings.mode = mode.into();
        }
        if let Some(access) = text("access") {
            settings.access = access.into();
        }
        if let Some(dir) = text("cwd") {
            settings.cwd = dir.into();
        }
        settings.cwd = std::path::absolute(&settings.cwd)
            .ok()
            .ok_or("Cannot resolve the working directory")?;
        if let Some(s) = text("model") {
            settings.model = Some(s.trim().into());
        }
        if let Some(s) = text("effort") {
            settings.effort = Some(s.into());
        }
        if let Some(s) = text("instructions") {
            settings.instructions = Some(s.into());
        }
        // D19 owner decision (2026-11-25): if start named no model and reply must name one,
        // repeat the model the provider reported at start; if none was reported, refuse.
        if reply && self.reply_model && settings.model.is_none() {
            if let Some(reported) = reported_model {
                settings.model = Some(reported);
            } else {
                return Err(
                    "The reply requires a model. Supply one because the start passed none and the provider reported none."
                        .into(),
                );
            }
        }
        let mut guidance = match settings.mode.as_str() {
            "advisory" => "Advise the caller and leave the workspace as you found it.".to_owned(),
            _ => "Carry out the requested changes and describe what you verified.".into(),
        };
        if let Some(instructions) = &settings.instructions {
            guidance.push_str("\n\n");
            guidance.push_str(instructions);
        }
        let mut prompt = text("prompt").unwrap().to_owned();
        if let Some(coord) = args.get("coordination") {
            prompt.push_str(&format!("\n\nIf Iris is available, use your own session to send a short start, progress and finish update to {}. Keep the answer in this tool result. Continue if Iris is unavailable.", coord["caller_agent_name"]));
        }
        Ok(Turn {
            thread_id: text("thread_id").map(str::to_owned),
            settings,
            timeout_ms,
            prompt,
            guidance,
            wait: args.get("wait").and_then(Value::as_bool).unwrap_or(true),
            memory_limit_bytes: args.get("memory_limit_bytes").and_then(Value::as_u64),
        })
    }
}
