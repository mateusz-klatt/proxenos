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

/// Resolves the absolute path of the running Proxenos executable without following symlinks.
///
/// On Linux `current_exe()` reads `/proc/self/exe`, which has already resolved symlinks, so
/// `argv[0]` is preferred when it identifies the same executable: a path with a separator is
/// made absolute against the current directory, and a bare name is matched against `PATH`.
pub fn current_executable() -> io::Result<PathBuf> {
    let exe = normalize_path(std::path::absolute(env::current_exe()?)?);
    let exe_canonical = fs::canonicalize(&exe)?;
    let invoked = env::args_os()
        .next()
        .and_then(|arg0| resolve_arg0(Path::new(&arg0), &exe_canonical));
    Ok(invoked.unwrap_or(exe))
}

fn resolve_arg0(arg0: &Path, expected_canonical: &Path) -> Option<PathBuf> {
    let has_separator = arg0.parent().is_some_and(|p| !p.as_os_str().is_empty());
    if has_separator {
        let abs = normalize_path(std::path::absolute(arg0).ok()?);
        return matches_canonical(&abs, expected_canonical).then_some(abs);
    }
    find_on_path(arg0, |candidate| {
        matches_canonical(candidate, expected_canonical)
    })
}

fn matches_canonical(candidate: &Path, expected_canonical: &Path) -> bool {
    fs::canonicalize(candidate).is_ok_and(|resolved| resolved == expected_canonical)
}

/// Searches absolute `PATH` entries for an executable named `name` (using `PATHEXT` on Windows
/// and executable mode bits on Unix) matching `predicate`, returning the `PATH` entry itself
/// without resolving symlinks.
pub fn find_on_path(name: &Path, mut predicate: impl FnMut(&Path) -> bool) -> Option<PathBuf> {
    let raw = env::var_os("PATH")?;
    let names = path_candidate_names(name);
    for dir in env::split_paths(&raw).filter(|dir| dir.is_absolute()) {
        for filename in &names {
            let candidate = normalize_path(dir.join(filename));
            if is_executable_file(&candidate) && predicate(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
}

#[cfg(windows)]
fn pathext_extensions() -> Vec<String> {
    let env_val = env::var("PATHEXT").ok().filter(|s| !s.trim().is_empty());
    let raw = env_val.as_deref().unwrap_or(".COM;.EXE;.BAT;.CMD");
    raw.split(';')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect()
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    let exts = pathext_extensions();
    path.is_file()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| exts.iter().any(|allowed| allowed.eq_ignore_ascii_case(ext)))
}

#[cfg(windows)]
fn path_candidate_names(name: &Path) -> Vec<std::ffi::OsString> {
    let mut names = vec![name.as_os_str().to_owned()];
    for ext in pathext_extensions() {
        let mut with_ext = name.as_os_str().to_owned();
        with_ext.push(".");
        with_ext.push(ext);
        names.push(with_ext);
    }
    names.dedup();
    names
}

#[cfg(not(windows))]
fn path_candidate_names(name: &Path) -> Vec<std::ffi::OsString> {
    vec![name.as_os_str().to_owned()]
}

#[cfg(windows)]
pub fn normalize_path(path: PathBuf) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) else {
        return path;
    };
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    if rest.as_bytes().get(1) == Some(&b':') {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
pub fn normalize_path(path: PathBuf) -> PathBuf {
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
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    let result = write_and_rename();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_handles_drive_unc_and_other_prefixes() {
        assert_eq!(
            normalize_path(PathBuf::from(r"\\?\C:\Users\alice\proxenos.exe")),
            PathBuf::from(r"C:\Users\alice\proxenos.exe")
        );
        assert_eq!(
            normalize_path(PathBuf::from(r"\\?\UNC\server\share\proxenos.exe")),
            PathBuf::from(r"\\server\share\proxenos.exe")
        );
        assert_eq!(
            normalize_path(PathBuf::from(r"\\?\Volume{1}\proxenos.exe")),
            PathBuf::from(r"\\?\Volume{1}\proxenos.exe")
        );
        assert_eq!(
            normalize_path(PathBuf::from(r"C:\plain\proxenos.exe")),
            PathBuf::from(r"C:\plain\proxenos.exe")
        );
    }
}
