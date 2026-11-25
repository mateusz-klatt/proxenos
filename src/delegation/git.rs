//! Best-effort observations from Git, with private configuration for the worktree scan.
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::mcp::Activity;
use crate::supervise::cleanup::DirectoryLease;
use crate::supervise::{self, Ending, Finished, Launch};

// Input filenames can contain line breaks; status uses NUL termination.
// If the listing does not fit, omit it instead of presenting an incomplete inventory.
const LIMIT: usize = 256 * 1024;

async fn find_git() -> Option<PathBuf> {
    static GIT: OnceCell<Option<PathBuf>> = OnceCell::const_new();
    GIT.get_or_init(|| async {
        tokio::task::spawn_blocking(|| {
            // Never look up git in the provider's working directory or execute a shell launcher.
            std::env::split_paths(&std::env::var_os("PATH")?)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(format!("git{}", std::env::consts::EXE_SUFFIX)))
                .find(|candidate| candidate.is_file())
        })
        .await
        .expect("git lookup worker finished")
    })
    .await
    .clone()
}

pub(super) async fn snapshot(
    cwd: &Path,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
    activity: Activity,
) -> Option<Value> {
    let program = find_git().await?;
    let launch = |cwd: &Path, args: &[&str]| {
        let limit = Instant::now() + Duration::from_secs(2);
        Launch {
            program: program.clone(),
            args: args.iter().map(OsString::from).collect(),
            cwd: Some(cwd.into()),
            env_remove: std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| {
                    key.to_string_lossy()
                        .to_ascii_uppercase()
                        .starts_with("GIT_")
                })
                .collect(),
            env_set: [
                ("GIT_CONFIG_NOSYSTEM", "1"),
                (
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                ),
                ("GIT_ATTR_NOSYSTEM", "1"),
                ("GIT_ALLOW_PROTOCOL", ""),
                ("GIT_NO_LAZY_FETCH", "1"),
            ]
            .map(|(k, v)| (k.into(), v.into()))
            .to_vec(),
            deadline: deadline.map_or(limit, |call| call.min(limit)),
            memory_limit_bytes: None,
            capture_limit: LIMIT,
            temporary_files: Vec::new(),
        }
    };
    let query = launch(
        cwd,
        &[
            "--no-pager",
            "--no-optional-locks",
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-path",
            "objects",
            "--git-path",
            "index",
            "--show-object-format",
            "--verify",
            "--quiet",
            "HEAD",
        ],
    );
    let metadata = supervise::run(query, cancel, activity.clone()).await.ok()?;
    // --verify --quiet exits 1 with the preceding path fields when HEAD is unborn.
    let metadata = output(&metadata, &[0, 1])?;
    let mut fields = metadata.lines();
    let root = Path::new(fields.next()?);
    let objects = fields.next()?;
    let index = fields.next()?;
    let format = fields.next()?;
    let commit = fields.next();
    if !matches!(format, "sha1" | "sha256")
        || fields.next().is_some()
        || commit.is_some_and(|oid| {
            !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return None;
    }
    // git status can invoke arbitrary clean filters from repository config. Give it the real
    // objects, index and worktree, but our own Git directory with no hooks, filters or remotes.
    let config =
        format!("[core]\nrepositoryformatversion=1\n[extensions]\nobjectformat={format}\n");
    let head = format!("{}\n", commit.unwrap_or("ref: refs/heads/unborn"));
    let root_owned = root.to_path_buf();
    let setup_activity = activity.clone();
    let (view, relative) = tokio::task::spawn_blocking(move || {
        let view = DirectoryLease::create_blocking(setup_activity).ok()?;
        fs::create_dir(view.path().join("refs")).ok()?;
        fs::write(view.path().join("config"), config).ok()?;
        fs::write(view.path().join("HEAD"), head).ok()?;
        let physical = view.path().canonicalize().ok()?;
        let relative = physical
            .strip_prefix(root_owned.canonicalize().ok()?)
            .ok()
            .map(Path::to_path_buf);
        Some((view, relative))
    })
    .await
    .expect("Git view setup worker finished")?;
    let mut status = launch(
        root,
        &[
            "--no-pager",
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ],
    );
    status.temporary_files.push(view.for_child());
    status.env_set.extend([
        ("GIT_DIR".into(), view.path().as_os_str().to_owned()),
        ("GIT_WORK_TREE".into(), root.into()),
        ("GIT_INDEX_FILE".into(), index.into()),
        ("GIT_OBJECT_DIRECTORY".into(), objects.into()),
    ]);
    view.finish().await;
    let listing = supervise::run(status, cancel, activity).await.ok()?;
    let mut changed = Vec::new();
    let mut untracked = Vec::new();
    for record in output(&listing, &[0])?.split_terminator('\0') {
        let path = record.get(3..)?;
        if record.starts_with("?? ") {
            // TMPDIR can itself be inside the worktree.
            if !relative
                .as_deref()
                .is_some_and(|scratch| Path::new(path).starts_with(scratch))
            {
                untracked.push(path);
            }
        } else {
            changed.push(path);
        }
    }
    Some(json!({"commit":commit,"changed":changed,"untracked":untracked}))
}

fn output<'a>(run: &'a Finished, codes: &[i32]) -> Option<&'a str> {
    if run.error.is_some()
        || !matches!(run.ending, Ending::Exited)
        || !codes.contains(&run.code?)
        || run.stdout.total > LIMIT as u64
    {
        return None;
    }
    std::str::from_utf8(&run.stdout.bytes).ok()
}
