//! User home and Proxenos state directory locations (D10, D19), plus atomic file writes.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use uuid::Uuid;

const HOME_ENV: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

pub fn absolute_env(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from).filter(|path| {
        path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
    })
}

pub fn home_dir() -> Result<PathBuf, String> {
    absolute_env(HOME_ENV).ok_or_else(|| format!("{HOME_ENV} is not set to an absolute path."))
}

#[cfg(unix)]
pub fn state_dir() -> Result<PathBuf, String> {
    let base = match absolute_env("XDG_STATE_HOME") {
        Some(dir) => dir,
        None => home_dir()?.join(".local").join("state"),
    };
    Ok(base.join("proxenos"))
}

#[cfg(windows)]
pub fn state_dir() -> Result<PathBuf, String> {
    absolute_env("LOCALAPPDATA")
        .map(|dir| dir.join("proxenos"))
        .ok_or_else(|| "LOCALAPPDATA is not set to an absolute path.".to_owned())
}

pub fn claude_settings_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

/// Resolves the canonical absolute path of the running Proxenos executable.
pub fn current_executable() -> io::Result<PathBuf> {
    let exe = env::current_exe()?;
    Ok(normalize_canonical(fs::canonicalize(exe)?))
}

#[cfg(windows)]
fn normalize_canonical(path: PathBuf) -> PathBuf {
    let stripped = path
        .to_str()
        .and_then(|s| s.strip_prefix(r"\\?\"))
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        .map(PathBuf::from);
    stripped.unwrap_or(path)
}

#[cfg(not(windows))]
fn normalize_canonical(path: PathBuf) -> PathBuf {
    path
}

pub fn path_string(path: &Path) -> io::Result<String> {
    Ok(path.to_str().ok_or(io::ErrorKind::InvalidData)?.to_owned())
}

#[cfg(unix)]
pub fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
pub fn file_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// Creates a single directory with owner-only permissions (0700) on Unix.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
    builder.create(path)
}

/// Ensures `target` exists under `root`, creating missing components from `root` downward
/// with owner-only permissions on Unix and recording every newly created directory in `created`.
///
/// Missing ancestors above `root` (such as `~/.local/state`) are created with default
/// permissions and are not recorded in `created`, because Proxenos does not own them.
pub fn ensure_private_dir_tree(
    root: &Path,
    target: &Path,
    created: &mut Vec<String>,
) -> io::Result<()> {
    let parent = root.parent().expect("root directory has a parent");
    fs::create_dir_all(parent)?;
    let rel = target.strip_prefix(root).expect("target is under root");
    let mut current = root.to_path_buf();
    ensure_one_private_dir(&current, created)?;
    for component in rel.components() {
        current.push(component);
        ensure_one_private_dir(&current, created)?;
    }
    Ok(())
}

fn ensure_one_private_dir(dir: &Path, created: &mut Vec<String>) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    create_private_dir(dir)?;
    let path = path_string(dir)?;
    if !created.contains(&path) {
        created.push(path);
    }
    Ok(())
}

/// Atomically writes `content` to `path` via a unique temporary file in the same directory.
/// On Unix, files Proxenos owns use mode `0600`; when `preserve_mode` is `Some(mode)`, that
/// permission mode is applied instead to preserve an existing user file's permissions.
pub fn write_atomic(path: &Path, content: &[u8], preserve_mode: Option<u32>) -> io::Result<()> {
    let parent = path.parent().expect("atomic target has a parent directory");
    let temp = parent.join(format!(".proxenos.tmp.{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, preserve_mode.unwrap_or(0o600));
    #[cfg(not(unix))]
    let _ = preserve_mode;

    let mut file = options.open(&temp)?;
    let write_and_rename = || -> io::Result<()> {
        #[cfg(unix)]
        if let Some(mode) = preserve_mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        }
        file.write_all(content)?;
        drop(file);
        fs::rename(&temp, path)
    };
    let result = write_and_rename();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
