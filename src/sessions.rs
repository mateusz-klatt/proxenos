//! Persisted session settings for D19 continuity across server restarts and cache eviction.
//!
//! Files hold only versioned, validated start settings, an ambiguity marker for conflicting
//! duplicate thread IDs, the save timestamp, and the model the provider reported at start.
//! Prompts, instructions text, environment variables, `timeout_ms`, and provider answers are
//! never stored here.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
    state::write_atomic(&target, &encoded, None)?;
    Ok(if ambiguous {
        SaveOutcome::Ambiguous
    } else {
        SaveOutcome::Saved
    })
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

/// Pure-Rust SHA-256 hex encoding so arbitrary untrusted thread IDs map safely to fixed-length,
/// case-insensitive-filesystem-safe filenames without path separators or length overflows.
fn encode_thread_id(thread_id: &str) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let input = thread_id.as_bytes();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0_u32; 64];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, delta) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut hex = String::with_capacity(64);
    for word in state {
        use std::fmt::Write;
        let _ = write!(hex, "{word:08x}");
    }
    hex
}
