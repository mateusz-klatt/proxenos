//! Private installations and replay scripts for the provider CLI stand-ins.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

use crate::common::{Server, text};

pub struct Fleet {
    pub root: PathBuf,
}

impl Fleet {
    pub fn new(clis: &[&str]) -> Self {
        // TMPDIR may be mounted noexec, and copying creates a new inode that macOS
        // AppleSystemPolicy must evaluate from scratch. Keep hard links under target/.
        let stub = std::path::Path::new(env!("CARGO_BIN_EXE_proxenos-stub"));
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
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn call(server: &mut Server, name: &str, args: Value) -> Value {
    let answer = server.call(7, name, args);
    let raw = text(&answer);
    let envelope: Value = serde_json::from_str(raw)
        .unwrap_or_else(|error| panic!("invalid JSON envelope ({error}): {answer}"));
    assert_eq!(envelope["schema_version"], 1, "{envelope}");
    assert_eq!(
        answer["result"]["isError"] == true,
        envelope["observed"]["state"] != "finished",
        "{envelope}"
    );
    envelope
}

pub fn turn(text: &str) -> Value {
    json!({"stdout": json!({"status":"SUCCESS","conversation_id":"session","response":text}).to_string()})
}
