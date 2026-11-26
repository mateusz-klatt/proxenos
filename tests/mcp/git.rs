//! Git observations through MCP, using a private repository and two installed provider stubs.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs};

use serde_json::json;

use crate::common::{Server, spawning, text};
use crate::provider_support::{Fleet, assert_observed_failure, call, turn};

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
    let ready = repo.fleet.root.join("agy.ready");
    repo.fleet
        .script("agy", json!([{"ready": ready, "sleep_ms": 30000}]));
    let mut server = repo.server(&repo.fleet.root);
    server.request(1, "tools/list", json!({}));
    server.start_call(
        7,
        "antigravity",
        json!({"cwd":subdir,"prompt":"check","timeout_ms":3000}),
    );
    // The stub signals readiness after the before snapshot. Another writer can edit the
    // checkout while the supervised turn is running.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "stub never reached ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(subdir.join("during turn"), "new").unwrap();
    let result: serde_json::Value = serde_json::from_str(text(&server.response(7))).unwrap();
    assert_observed_failure(&result, "timeout", "exceeded the call's deadline");
    repo.fleet.assert_steps_remaining("agy", 0);
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
    struct Outside(PathBuf);
    impl Drop for Outside {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let external =
        Outside(env::temp_dir().join(format!("proxenos-outside-{}", uuid::Uuid::new_v4())));
    fs::create_dir(&external.0).unwrap();
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
    repo.fleet.assert_steps_remaining("agy", 1);
    let outside = call(
        &mut server,
        "antigravity",
        json!({"cwd":&external.0,"prompt":"check"}),
    );
    assert_eq!(outside["observed"]["state"], "finished", "{outside}");
    assert!(outside["observed"]["git"].is_null());
    repo.fleet.assert_steps_remaining("agy", 0);
    let turns = repo
        .fleet
        .records()
        .into_iter()
        .filter(|record| record["cli"] == "agy" && record["args"] != json!(["--version"]))
        .count();
    assert_eq!(turns, 2);
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
    assert_observed_failure(&result, "launch", "");
    // A launch failure from the version probe would not exercise the missing scratch space.
    assert!(
        !result["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Version check"),
        "{result}"
    );
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
    fleet.assert_steps_remaining("git", 0);
    fleet.assert_steps_remaining("agy", 0);
    let git_runs = fleet
        .records()
        .into_iter()
        .filter(|record| record["cli"] == "git")
        .count();
    assert_eq!(git_runs, 2);
}

#[test]
fn stalled_git_lookup_can_be_cancelled_or_timed_out_and_is_reused_after_release() {
    use serde_json::Value;

    let fleet = Fleet::new(&["git", "agy"]);
    let gate_path = fleet.root.join("git-lookup.lock");
    let gate = fs::File::create(&gate_path).unwrap();
    gate.lock().unwrap();
    let metadata = json!({"stdout": format!("{}\nobjects\nindex\nsha1\n", fleet.root.display())});
    let status = json!({"stdout": "?? recovered.txt\u{0}"});
    fleet.script("git", json!([metadata, status, metadata, status]));
    fleet.script("agy", json!([turn("without Git"), turn("with Git")]));

    let mut cmd = fleet.command("codex");
    cmd.env("PROXENOS_TEST_GIT_LOOKUP_LOCK", &gate_path);
    let mut server = Server::start_command(cmd);
    server.initialize("2025-11-25");
    server.request(1, "tools/list", json!({}));

    let control = |server: &mut Server, name: &str, args: Value| -> Value {
        let reply = server.call(2, name, args);
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        serde_json::from_str(text(&reply)).unwrap()
    };
    let started = control(
        &mut server,
        "antigravity",
        json!({"prompt": "cancel", "wait": false}),
    );
    server.wait_for_stderr("git lookup waiting on test gate");
    let handle = started["handle"].as_str().unwrap();
    let cancelled = control(&mut server, "cancel", json!({"handle": handle}));
    assert_eq!(cancelled["cancel_requested"], true, "{cancelled}");
    let done = control(&mut server, "await", json!({"handles": [handle]}));
    assert_observed_failure(&done["results"][0]["result"], "cancellation", "");

    let timed = call(
        &mut server,
        "antigravity",
        json!({"prompt": "timeout", "timeout_ms": 500}),
    );
    assert_observed_failure(&timed, "timeout", "deadline");
    fleet.assert_steps_remaining("agy", 2);

    // An ordinary delegation can still run when the best-effort Git observation times out.
    let skipped = call(&mut server, "antigravity", json!({"prompt": "continue"}));
    assert_eq!(skipped["reported"]["content"], "without Git", "{skipped}");
    assert!(skipped["observed"]["git"].is_null());
    fleet.assert_steps_remaining("git", 4);

    gate.unlock().unwrap();
    let recovered = call(&mut server, "antigravity", json!({"prompt": "observe"}));
    assert_eq!(recovered["reported"]["content"], "with Git", "{recovered}");
    for when in ["before", "after"] {
        assert_eq!(
            recovered["observed"]["git"][when]["untracked"],
            json!(["recovered.txt"]),
            "{recovered}"
        );
    }
    fleet.assert_steps_remaining("git", 0);
    fleet.assert_steps_remaining("agy", 0);
    assert!(server.finish().0.success());
    let stderr = server.stderr();
    assert_eq!(stderr.matches("git lookup waiting on test gate").count(), 1);
}
