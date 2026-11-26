//! Persisted session settings for D19 continuity across server restarts and cache eviction.
//!
//! Files hold only versioned, validated start settings, an ambiguity marker for conflicting
//! duplicate thread IDs, the save timestamp, and the model the provider reported at start.
//! Prompts, instructions text, environment variables, `timeout_ms`, and provider answers are
//! never stored here.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::providers::{Provider, RememberedStart, Settings, is_valid_model};
use crate::state;

const SESSION_SCHEMA_VERSION: u32 = 1;
const SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_SESSION_BYTES: u64 = 64 * 1024;

fn max_session_bytes() -> u64 {
    #[cfg(feature = "test-tools")]
    if let Some(limit) = std::env::var("PROXENOS_TEST_SESSION_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&n| n > 0)
    {
        return limit;
    }
    MAX_SESSION_BYTES
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionFile {
    schema_version: u32,
    provider: String,
    thread_id: String,
    saved_at_epoch_s: u64,
    #[serde(default)]
    ambiguous: bool,
    settings: Option<StoredSettings>,
    reported_model: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSettings {
    cwd: String,
    mode: String,
    access: String,
    model: Option<String>,
    effort: Option<String>,
}

pub enum SessionLookup {
    Missing,
    Found(RememberedStart),
    Ambiguous,
    Corrupt(String),
}

pub enum SaveOutcome {
    Saved,
    Ambiguous,
    Unconfigured,
}

fn provider_dir_chain(state_dir: &Path, provider: &str) -> [PathBuf; 3] {
    let proxenos = state_dir.to_path_buf();
    let sessions = proxenos.join("sessions");
    let provider_dir = sessions.join(provider);
    [proxenos, sessions, provider_dir]
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn lookup_bytes(provider: &Provider, thread_id: &str, read: io::Result<Vec<u8>>) -> SessionLookup {
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SessionLookup::Missing,
        Err(error) => return SessionLookup::Corrupt(error.to_string()),
    };
    if bytes.len() as u64 > MAX_SESSION_BYTES {
        return SessionLookup::Corrupt("session file exceeds size limit".into());
    }
    let record: SessionFile = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => return SessionLookup::Corrupt(format!("invalid session file: {error}")),
    };
    let saved_at = record.saved_at_epoch_s;
    let validated = match validate(provider, thread_id, record) {
        Ok(validated) => validated,
        Err(reason) => return SessionLookup::Corrupt(reason.into()),
    };
    if now_epoch_s().saturating_sub(saved_at) > SESSION_TTL_SECS {
        return SessionLookup::Missing;
    }
    match validated {
        Some(remembered) => SessionLookup::Found(remembered),
        None => SessionLookup::Ambiguous,
    }
}

pub fn load(provider: &Provider, thread_id: &str) -> SessionLookup {
    let Some(state_dir) = state::state_dir() else {
        return SessionLookup::Missing;
    };
    lookup_bytes(
        provider,
        thread_id,
        read_provider_session(&state_dir, provider.name, thread_id),
    )
}

fn stored_from(start: &RememberedStart) -> io::Result<(StoredSettings, Option<String>)> {
    let cwd = start
        .settings
        .cwd
        .to_str()
        .ok_or(io::ErrorKind::InvalidData)?
        .to_owned();
    let stored = StoredSettings {
        cwd,
        mode: start.settings.mode.clone(),
        access: start.settings.access.clone(),
        model: start.settings.model.clone(),
        effort: start.settings.effort.clone(),
    };
    let reported_model = start
        .reported_model
        .as_deref()
        .filter(|model| is_valid_model(model))
        .map(|model| model.trim().to_owned());
    Ok((stored, reported_model))
}

/// Checks whether a session loaded from disk matches the persisted subset of an in-memory start.
pub fn matches_persisted(disk: &RememberedStart, expected: &RememberedStart) -> bool {
    stored_from(expected)
        .ok()
        .is_some_and(|want| stored_from(disk).ok().as_ref() == Some(&want))
}

/// Persists a confirmed start or an ambiguity tombstone (`entry == None`).
/// Returns `SaveOutcome::Unconfigured` when no durable state directory is configured,
/// `SaveOutcome::Ambiguous` when the durable record is ambiguous (including on a cross-restart
/// conflict), or `SaveOutcome::Saved` when persisted cleanly.
pub fn save(
    provider: &Provider,
    thread_id: &str,
    entry: Option<RememberedStart>,
) -> io::Result<SaveOutcome> {
    let Some(state_dir) = state::state_dir() else {
        return Ok(SaveOutcome::Unconfigured);
    };
    let desired = entry.as_ref().map(stored_from).transpose()?;
    let provider_dir = ensure_provider_dir(&state_dir, provider.name)?;
    let key = encode_thread_id(thread_id);
    let _lock = open_lock_file(&provider_dir.join(format!("{key}.lock")))?;
    let target = provider_dir.join(format!("{key}.json"));
    // Re-check the target under the advisory lock: fail closed on corruption, never downgrade
    // an ambiguity tombstone, and detect conflicts across processes or restarts.
    let desired = match (
        desired,
        lookup_bytes(provider, thread_id, read_session_file(&target)),
    ) {
        (_, SessionLookup::Corrupt(reason)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refusing to overwrite corrupt session file: {reason}"),
            ));
        }
        (None, _) | (_, SessionLookup::Ambiguous) => None,
        (Some(next), SessionLookup::Found(existing))
            if stored_from(&existing).ok().as_ref() != Some(&next) =>
        {
            None
        }
        (desired, _) => desired,
    };
    let ambiguous = desired.is_none();
    let (settings, reported_model) = match desired {
        Some((settings, reported_model)) => (Some(settings), reported_model),
        None => (None, None),
    };
    let payload = SessionFile {
        schema_version: SESSION_SCHEMA_VERSION,
        provider: provider.name.to_owned(),
        thread_id: thread_id.to_owned(),
        saved_at_epoch_s: now_epoch_s(),
        ambiguous,
        settings,
        reported_model,
    };
    let encoded = serde_json::to_vec(&payload).expect("session JSON serializes");
    if encoded.len() as u64 > max_session_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session payload exceeds size limit",
        ));
    }
    let staging = provider_dir.join(format!(".tmp-{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let _staging_guard = Staging(staging.clone());
    let mut file = options.open(&staging)?;
    file.write_all(&encoded)?;
    drop(file);
    fs::rename(&staging, &target)?;
    Ok(if ambiguous {
        SaveOutcome::Ambiguous
    } else {
        SaveOutcome::Saved
    })
}

struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn verify_existing_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !state::is_private_dir(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session directory {} is not a private directory",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_provider_dir(state_dir: &Path, provider: &str) -> io::Result<PathBuf> {
    let parent = state_dir.parent().expect("state directory has a parent");
    fs::create_dir_all(parent)?;
    let chain = provider_dir_chain(state_dir, provider);
    for dir in &chain {
        ensure_private_dir(dir)?;
    }
    let [_, _, provider_dir] = chain;
    Ok(provider_dir)
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
    let _ = builder.create(path);
    verify_existing_dir(path)
}

fn is_regular_file(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        meta.file_type().is_file() && (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_file()
    }
}

fn require_regular(meta: &fs::Metadata) -> io::Result<()> {
    if !is_regular_file(meta) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session path is not a regular file",
        ));
    }
    Ok(())
}

fn open_regular_read(path: &Path) -> io::Result<fs::File> {
    require_regular(&fs::symlink_metadata(path)?)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    );
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let file = options.open(path)?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let file = options.open(path)?;
    require_regular(&file.metadata()?)?;
    file.lock()?;
    Ok(file)
}

fn read_provider_session(state_dir: &Path, provider: &str, thread_id: &str) -> io::Result<Vec<u8>> {
    let chain = provider_dir_chain(state_dir, provider);
    for dir in &chain {
        verify_existing_dir(dir)?;
    }
    let key = encode_thread_id(thread_id);
    let _lock = match open_regular_read(&chain[2].join(format!("{key}.lock"))) {
        Ok(file) => {
            file.lock_shared()?;
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    read_session_file(&chain[2].join(format!("{key}.json")))
}

fn read_session_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_regular_read(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_SESSION_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn validate(
    provider: &Provider,
    thread_id: &str,
    record: SessionFile,
) -> Result<Option<RememberedStart>, &'static str> {
    let header_valid = record.schema_version == SESSION_SCHEMA_VERSION
        && record.provider == provider.name
        && record.thread_id == thread_id;
    if header_valid
        && record.ambiguous
        && record.settings.is_none()
        && record.reported_model.is_none()
    {
        return Ok(None);
    }
    let valid_text = |value: &str| !value.trim().is_empty() && !value.contains('\0');
    if header_valid
        && !record.ambiguous
        && let Some(stored) = record.settings
        && valid_text(&stored.cwd)
        && Path::new(&stored.cwd).is_absolute()
        && matches!(stored.mode.as_str(), "advisory" | "implementation")
        && provider
            .access
            .iter()
            .any(|(name, _)| *name == stored.access)
        && stored.model.as_deref().is_none_or(is_valid_model)
        && stored
            .effort
            .as_deref()
            .is_none_or(|effort| provider.efforts.contains(&effort))
        && record.reported_model.as_deref().is_none_or(is_valid_model)
    {
        return Ok(Some(RememberedStart {
            settings: Settings {
                cwd: PathBuf::from(stored.cwd),
                mode: stored.mode,
                access: stored.access,
                instructions: None,
                model: stored.model,
                effort: stored.effort,
            },
            reported_model: record.reported_model,
        }));
    }
    Err("session file contains unsupported or invalid settings")
}

/// SHA-256 lowercase hex encoding so arbitrary untrusted thread IDs map safely to fixed-length,
/// case-insensitive-filesystem-safe filenames without path separators or length overflows.
fn encode_thread_id(thread_id: &str) -> String {
    use std::fmt::Write;

    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(thread_id.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::encode_thread_id;

    #[test]
    fn encode_thread_id_matches_sha256_compatibility_vectors() {
        let fixed_vectors = [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "thread-αβγ-日本語-🦀",
                "2bb2c3afab11afb76befa3f970bd94ad0a5a55b52283602f2ec6051c487eafbc",
            ),
            (
                "paper-crane",
                "a5647388cdfcab6bbc8e90a7d46472ebd167c068defa24914868e0dd8198e8d9",
            ),
            (
                "eeb3a731-d53a-4444-8a6a-4d9d89e76073",
                "3cc308ff1050ae52863ad8ca87fb1ec9fbf09035a7a53d363ab7094a91d9ef59",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (input, expected) in fixed_vectors {
            assert_eq!(encode_thread_id(input), expected);
        }

        let padding_edges = [
            (
                55,
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                63,
                "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                119,
                "31eba51c313a5c08226adf18d4a359cfdfd8d2e816b13f4af952f7ea6584dcfb",
            ),
            (
                120,
                "2f3d335432c70b580af0e8e1b3674a7c020d683aa5f73aaaedfdc55af904c21c",
            ),
        ];
        for (len, expected) in padding_edges {
            assert_eq!(encode_thread_id(&"a".repeat(len)), expected);
        }
    }
}
