//! Private installations and replay scripts for the provider CLI stand-ins.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

use crate::common::{Server, text};

/// The stub CLI that tests install under provider names.
pub fn stub() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_proxenos-stub"))
}

pub struct Fleet {
    pub root: PathBuf,
}

impl Fleet {
    pub fn new(clis: &[&str]) -> Self {
        // TMPDIR may be mounted noexec, and copying creates a new inode that macOS
        // AppleSystemPolicy must evaluate from scratch. Keep hard links under target/.
        let stub = stub();
        let root = stub
            .parent()
            .unwrap()
            .join(format!("proxenos-test-{}", uuid::Uuid::new_v4()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for cli in clis {
            let dest = bin.join(format!("{cli}{}", std::env::consts::EXE_SUFFIX));
            fs::hard_link(stub, &dest).unwrap_or_else(|error| {
                panic!(
                    "hard-linking {} to {} failed: {error}",
                    stub.display(),
                    dest.display()
                )
            });
        }
        Self { root }
    }

    pub fn command(&self, client: &str) -> Command {
        let mut command = Server::command(&["mcp", "--client", client]);
        command
            .current_dir(&self.root)
            .env("PATH", self.root.join("bin"))
            .env("PROXENOS_STUB_DIR", &self.root)
            .env("TEMP", &self.root)
            .env("TMPDIR", &self.root)
            .env("TMP", &self.root);
        command.env_remove("XDG_STATE_HOME");
        for key in ["HOME", "USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
            command.env(key, &self.root);
        }
        command
    }

    pub fn server(&self, client: &str) -> Server {
        let mut server = Server::start_command(self.command(client));
        server.initialize("2025-11-25");
        server
    }

    pub fn records(&self) -> Vec<Value> {
        let Ok(mut file) = fs::File::open(self.root.join("invocations")) else {
            return Vec::new();
        };
        file.lock_shared().unwrap();
        let mut records = String::new();
        file.read_to_string(&mut records).unwrap();
        records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn script(&self, cli: &str, steps: Value) {
        fs::write(self.root.join(format!("{cli}.steps")), steps.to_string()).unwrap();
    }

    pub fn remaining_steps(&self, cli: &str) -> Vec<Value> {
        let script = self.root.join(format!("{cli}.steps"));
        let bytes = fs::read(&script)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", script.display()));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("invalid JSON in {}: {error}", script.display()))
    }

    #[track_caller]
    pub fn assert_steps_remaining(&self, cli: &str, expected: usize) {
        let remaining = self.remaining_steps(cli);
        assert_eq!(
            remaining.len(),
            expected,
            "{cli}.steps had {} step(s) left instead of {expected}: {remaining:?}",
            remaining.len()
        );
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub const AUTHENTICATION_FAILURE_MESSAGE: &str = "The provider reported that it is not signed in.";
pub const AUTHENTICATION_DIAGNOSTIC: &str = "The provider CLI is not signed in. Its sign-in output was omitted because it can contain one-time login URLs or codes.";

#[track_caller]
pub fn assert_observed_failure(envelope: &Value, kind: &str, detail: &str) {
    let expected_state = if kind == "cancellation" {
        "cancelled"
    } else {
        "failed"
    };
    assert_eq!(envelope["observed"]["state"], expected_state, "{envelope}");
    assert_eq!(envelope["observed"]["failure"]["kind"], kind, "{envelope}");
    let message = envelope["observed"]["failure"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing observed failure message in envelope: {envelope}"));
    assert!(
        !message.is_empty() && message.contains(detail),
        "expected observed failure message to contain {detail:?}: {envelope}"
    );
    assert!(
        envelope["reported"]["failure"].is_null(),
        "unexpected reported failure in envelope: {envelope}"
    );
    assert_eq!(envelope["failure"]["kind"], kind, "{envelope}");
    assert_eq!(envelope["failure"]["source"], "observed", "{envelope}");
    let top_message = envelope["failure"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing top-level failure message in envelope: {envelope}"));
    assert!(
        !top_message.is_empty() && top_message.contains(detail),
        "expected top-level failure message to contain {detail:?}: {envelope}"
    );
    assert!(
        envelope["failure"].get("reset_at").is_none(),
        "unexpected reset_at in observed failure: {envelope}"
    );
}

#[track_caller]
pub fn assert_reported_failure(
    envelope: &Value,
    observed: Option<(&str, &str)>,
    reported_kind: &str,
    reported_detail: &str,
) {
    assert_eq!(envelope["observed"]["state"], "failed", "{envelope}");
    match observed {
        Some((kind, detail)) => {
            assert_eq!(envelope["observed"]["failure"]["kind"], kind, "{envelope}");
            let message = envelope["observed"]["failure"]["message"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!("missing observed failure message in envelope: {envelope}")
                });
            assert!(
                !message.is_empty() && message.contains(detail),
                "expected observed failure message to contain {detail:?}: {envelope}"
            );
        }
        None => {
            assert!(
                envelope["observed"]["failure"].is_null(),
                "unexpected observed failure in envelope: {envelope}"
            );
        }
    }
    assert_eq!(
        envelope["reported"]["failure"]["kind"], reported_kind,
        "{envelope}"
    );
    let message = envelope["reported"]["failure"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing reported failure message in envelope: {envelope}"));
    let top_message = envelope["failure"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing top-level failure message in envelope: {envelope}"));
    if reported_kind == "authentication" {
        assert_eq!(message, AUTHENTICATION_FAILURE_MESSAGE, "{envelope}");
        assert_eq!(top_message, AUTHENTICATION_FAILURE_MESSAGE, "{envelope}");
        assert_eq!(
            envelope["reported"]["diagnostic"], AUTHENTICATION_DIAGNOSTIC,
            "{envelope}"
        );
        assert_eq!(envelope["observed"]["redacted"], true, "{envelope}");
    } else {
        assert!(
            !message.is_empty() && message.contains(reported_detail),
            "expected reported failure message to contain {reported_detail:?}: {envelope}"
        );
        assert_eq!(top_message, message, "{envelope}");
    }
    assert_eq!(envelope["failure"]["kind"], reported_kind, "{envelope}");
    assert_eq!(envelope["failure"]["source"], "reported", "{envelope}");
    assert_eq!(
        envelope["failure"].get("reset_at"),
        envelope["reported"]["failure"].get("reset_at"),
        "{envelope}"
    );
}

pub fn call(server: &mut Server, name: &str, args: Value) -> Value {
    let answer = server.call(7, name, args);
    let raw = text(&answer);
    let envelope: Value = serde_json::from_str(raw)
        .unwrap_or_else(|error| panic!("invalid JSON envelope ({error}): {answer}"));
    assert_eq!(envelope["schema_version"], 1, "{envelope}");
    assert!(
        envelope.as_object().unwrap().contains_key("failure"),
        "missing top-level failure key: {envelope}"
    );
    let finished = envelope["observed"]["state"] == "finished";
    assert_eq!(answer["result"]["isError"] == true, !finished, "{envelope}");
    assert_eq!(envelope["failure"].is_null(), finished, "{envelope}");
    envelope
}

pub fn turn(text: &str) -> Value {
    json!({"stdout": json!({"status":"SUCCESS","conversation_id":"session","response":text}).to_string()})
}
