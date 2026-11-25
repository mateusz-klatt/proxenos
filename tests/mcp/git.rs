//! Git observations through MCP, using a private repository and two installed provider stubs.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs};

use serde_json::json;

use crate::common::{Server, spawning, text};
use crate::provider_support::{Fleet, call, turn};

struct Repository {
    fleet: Fleet,
    repo: PathBuf,
    git: PathBuf,
}

impl Repository {
    fn new() -> Self {
        let fleet = Fleet::new(&["codex", "agy"]);
        let git = env::split_paths(&env::var_os("PATH").unwrap())
            .filter(|p| p.is_absolute())
            .map(|p| p.join(format!("git{}", env::consts::EXE_SUFFIX)))
            .find(|p| p.is_file())
            .expect("Git is required for its repository fixture");
        let repo = fleet.root.join("checkout");
        fs::create_dir(&repo).unwrap();
        let result = Self { fleet, repo, git };
        result.git(&["init", "-q", "--template="]);
        result
    }
    fn git(&self, args: &[&str]) -> String {
        let _spawning = spawning();
        let mut git = Command::new(&self.git);
        // A test run from a Git hook can inherit the caller's index and worktree.
        for (key, _) in env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                git.env_remove(key);
            }
        }
        let out = git
            .current_dir(&self.repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.fleet.root.join("empty-config"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn server(&self, temp_dir: &Path) -> Server {
        let mut command = self.fleet.command("codex");
        // Only the provider stand-ins precede git; codex is hidden from its own client.
        let path = env::join_paths([
            self.fleet.root.join("bin"),
            self.git.parent().unwrap().to_owned(),
        ])
        .unwrap();
        command.env("PATH", path);
        for key in ["TMPDIR", "TMP", "TEMP"] {
            command.env(key, temp_dir);
        }
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        server
    }
}

#[test]
fn git_records_commit_and_paths_before_and_after_a_provider_timeout() {
    let repo = Repository::new();
    fs::write(repo.repo.join("tracked file"), "first").unwrap();
    repo.git(&["add", "tracked file"]);
    repo.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--no-gpg-sign",
        "-qm",
        "Initial",
    ]);
    let oid = repo.git(&["rev-parse", "HEAD"]);
    let subdir = repo.repo.join("nested");
    fs::create_dir(&subdir).unwrap();
    fs::write(subdir.join("new file"), "temporary").unwrap();
    fs::write(repo.repo.join("tracked file"), "modified content").unwrap();

    // An invalid mandatory filter in the real Git config would make status fail when Git
    // examines the modified file. The snapshot reads no commands from that config.
    fs::write(
        repo.repo.join(".gitattributes"),
        "tracked* filter=private\n",
    )
    .unwrap();
    repo.git(&["config", "filter.private.required", "true"]);
    repo.fleet.script("agy", json!([{"sleep_ms":30000}]));
    let mut server = repo.server(&repo.fleet.root);
    server.start_call(
        7,
        "antigravity",
        json!({"cwd":subdir,"prompt":"check","timeout_ms":3000}),
    );
    // The stub consumes its step after the before snapshot. Another writer can edit the
    // checkout while the supervised turn is running.
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs::read(repo.fleet.root.join("agy.steps")).unwrap() != b"[]" {
        assert!(Instant::now() < deadline, "stub never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(subdir.join("during turn"), "new").unwrap();
    let result: serde_json::Value = serde_json::from_str(text(&server.response(7))).unwrap();
    assert_eq!(result["observed"]["failure"]["kind"], "timeout", "{result}");
    for when in ["before", "after"] {
        let snap = &result["observed"]["git"][when];
        assert_eq!(snap["commit"], oid.trim(), "{result}");
        assert_eq!(snap["changed"], json!(["tracked file"]));
        let paths = if when == "before" {
            json!([".gitattributes", "nested/new file"])
        } else {
            json!([".gitattributes", "nested/during turn", "nested/new file"])
        };
        assert_eq!(snap["untracked"], paths);
    }
}

#[test]
fn git_handles_an_unborn_repo_and_omits_an_unavailable_worktree() {
    let repo = Repository::new();
    fs::write(repo.repo.join("staged"), "text").unwrap();
    repo.git(&["add", "staged"]);
    let external = repo.fleet.root.join("outside");
    fs::create_dir(&external).unwrap();
    repo.fleet
        .script("agy", json!([turn("first"), turn("second")]));
    // Keep private scratch inside this checkout. None of its names belong in the result.
    let temp = repo.repo.join("private scratch");
    fs::create_dir(&temp).unwrap();
    let mut server = repo.server(&temp);
    let start = call(
        &mut server,
        "antigravity",
        json!({"cwd":repo.repo,"prompt":"check"}),
    );
    assert_eq!(start["observed"]["state"], "finished", "{start}");
    assert_eq!(
        start["observed"]["git"]["before"],
        json!({"commit":null,"changed":["staged"],"untracked":[]})
    );
    let outside = call(
        &mut server,
        "antigravity",
        json!({"cwd":external,"prompt":"check"}),
    );
    assert_eq!(outside["observed"]["state"], "finished", "{outside}");
    assert!(outside["observed"]["git"].is_null());
}

#[test]
fn unavailable_scratch_space_omits_git_observations_and_prevents_dispatch() {
    let repo = Repository::new();
    let mut server = repo.server(&repo.fleet.root.join("missing"));
    let result = call(
        &mut server,
        "antigravity",
        json!({"cwd": repo.repo, "prompt": "check"}),
    );
    assert_eq!(result["observed"]["failure"]["kind"], "launch");
    assert!(result["observed"]["git"].is_null());
    assert_eq!(repo.fleet.records().len(), 1);
    assert_eq!(repo.fleet.records()[0]["args"], json!(["--version"]));
}

#[test]
fn git_omits_an_unsupported_object_format_without_affecting_the_call() {
    let fleet = Fleet::new(&["git", "agy"]);
    let metadata = "worktree\nobjects\nindex\nunsupported-hash\n";
    fleet.script("git", json!([{"stdout":metadata},{"stdout":metadata}]));
    fleet.script("agy", json!([turn("done")]));
    let mut server = fleet.server("codex");
    let result = call(&mut server, "antigravity", json!({"prompt":"check"}));
    assert_eq!(result["observed"]["state"], "finished", "{result}");
    assert!(result["observed"]["git"].is_null());
}
