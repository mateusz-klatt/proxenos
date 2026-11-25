//! Client registration management for `proxenos integrate` (D10).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::state;

pub const CLIENT_CLAUDE_CODE: &str = "claude-code";
pub const MARKETPLACE_NAME: &str = "proxenos";
pub const PLUGIN_NAME: &str = "proxenos";
pub const ENABLED_PLUGIN_KEY: &str = "proxenos@proxenos";
pub const MCP_SERVER_NAME: &str = "proxenos";
pub const MARKETPLACE_DIR_NAME: &str = "claude-marketplace";
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

const PLUGIN_DESCRIPTION: &str = "MCP delegation between coding-agent CLIs.";

#[derive(Debug)]
pub enum IntegrateError {
    Message(String),
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for IntegrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> IntegrateError {
    let path = path.into();
    move |source| IntegrateError::Io { path, source }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub clients: BTreeMap<String, ClientRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRecord {
    pub client: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    pub marketplace_dir: String,
    pub binary_path: String,
    pub settings_path: String,
    pub settings_created: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settings_is_symlink: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_symlink_target: Option<String>,
    pub created_extra_known_marketplaces: bool,
    pub created_enabled_plugins: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_written_sha256: Option<String>,
    pub entries: RecordedEntries,
    pub files: Vec<String>,
    pub created_dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedEntries {
    pub extra_known_marketplace: Value,
    pub enabled_plugin: Value,
}

pub fn manifest_path(state_dir: &Path) -> PathBuf {
    state_dir.join("integrate").join("manifest.json")
}

pub fn marketplace_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKETPLACE_DIR_NAME)
}

fn backup_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("integrate")
        .join("backups")
        .join("claude-code-settings.json")
}

struct FileData {
    bytes: Vec<u8>,
    mode: Option<u32>,
}

struct ResolvedSettingsPath {
    target: PathBuf,
    symlink_target: Option<String>,
}

fn resolve_settings_target(path: &Path) -> Result<ResolvedSettingsPath, IntegrateError> {
    if fs::read_link(path).is_ok() {
        let resolved = fs::canonicalize(path)
            .map(state::normalize_path)
            .map_err(|err| {
                IntegrateError::Message(format!(
                    "Refusing to modify Claude Code settings symlink {}: target cannot be resolved ({err}).",
                    path.display()
                ))
            })?;
        let target_str = state::path_string(&resolved).map_err(io_err(&resolved))?;
        return Ok(ResolvedSettingsPath {
            target: resolved,
            symlink_target: Some(target_str),
        });
    }
    Ok(ResolvedSettingsPath {
        target: path.to_path_buf(),
        symlink_target: None,
    })
}

fn check_settings_target_unchanged(
    record: &ClientRecord,
    resolved: &ResolvedSettingsPath,
    settings_path: &Path,
) -> Result<(), IntegrateError> {
    if fs::symlink_metadata(settings_path).is_ok()
        && (record.settings_is_symlink != resolved.symlink_target.is_some()
            || record.settings_symlink_target != resolved.symlink_target)
    {
        return Err(IntegrateError::Message(format!(
            "Refusing to modify {}: symlink target changed since integrate.",
            settings_path.display()
        )));
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<FileData>, IntegrateError> {
    match fs::read(path) {
        Ok(bytes) => {
            let meta = fs::metadata(path).map_err(io_err(path))?;
            Ok(Some(FileData {
                bytes,
                mode: state::file_mode(&meta),
            }))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IntegrateError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn load_manifest(path: &Path) -> Result<Manifest, IntegrateError> {
    let Some(data) = read_optional(path)? else {
        return Ok(Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            clients: BTreeMap::new(),
        });
    };
    let manifest: Manifest = serde_json::from_slice(&data.bytes).map_err(|err| {
        IntegrateError::Message(format!(
            "Could not parse integration manifest {}: {err}",
            path.display()
        ))
    })?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(IntegrateError::Message(format!(
            "Unsupported integration manifest schema version {} in {}.",
            manifest.schema_version,
            path.display()
        )));
    }
    Ok(manifest)
}

fn parse_settings(path: &Path, bytes: &[u8]) -> Result<Map<String, Value>, IntegrateError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| {
        IntegrateError::Message(format!(
            "Could not parse Claude Code settings {}: {err}",
            path.display()
        ))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        IntegrateError::Message(format!(
            "Claude Code settings {} must be a JSON object.",
            path.display()
        ))
    })
}

fn section_object<'a>(
    settings: &'a Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<&'a Map<String, Value>>, IntegrateError> {
    match settings.get(key) {
        None => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(IntegrateError::Message(format!(
            "Key `{key}` in {} must be a JSON object.",
            path.display()
        ))),
    }
}

fn check_entry_ownership(
    section: Option<&Map<String, Value>>,
    section_name: &str,
    key: &str,
    expected: Option<&Value>,
    settings_path: &Path,
) -> Result<(), IntegrateError> {
    let Some(current) = section.and_then(|map| map.get(key)) else {
        return Ok(());
    };
    match expected {
        None => Err(IntegrateError::Message(format!(
            "Refusing to take over existing {section_name}.{key} in {}; it was not written by proxenos.",
            settings_path.display()
        ))),
        Some(recorded) if current != recorded => Err(IntegrateError::Message(format!(
            "Refusing to overwrite {section_name}.{key} in {}; it was modified after integrate.",
            settings_path.display()
        ))),
        Some(_) => Ok(()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

fn write_json_atomic(
    path: &Path,
    value: &Value,
    preserve_mode: Option<u32>,
) -> Result<Vec<u8>, IntegrateError> {
    let mut text = serde_json::to_string_pretty(value).expect("JSON value serializes");
    text.push('\n');
    let bytes = text.into_bytes();
    state::write_atomic(path, &bytes, preserve_mode).map_err(io_err(path))?;
    Ok(bytes)
}

fn remove_if_exists(path: &Path) -> Result<(), IntegrateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(IntegrateError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn integrate_claude_code() -> Result<String, IntegrateError> {
    let state_dir = state::require_state_dir().map_err(IntegrateError::Message)?;
    let settings_path = state::claude_settings_path().map_err(IntegrateError::Message)?;
    let binary_path = state::current_executable().map_err(io_err("current executable"))?;
    let binary_str = state::path_string(&binary_path).map_err(io_err(&binary_path))?;
    let marketplace_path = marketplace_dir(&state_dir);
    let marketplace_str =
        state::path_string(&marketplace_path).map_err(io_err(&marketplace_path))?;
    let settings_str = state::path_string(&settings_path).map_err(io_err(&settings_path))?;
    let manifest_file = manifest_path(&state_dir);

    let mut manifest = load_manifest(&manifest_file)?;
    let existing = manifest.clients.get(CLIENT_CLAUDE_CODE).cloned();

    let marketplace_is_ours = existing
        .as_ref()
        .is_some_and(|record| record.marketplace_dir == marketplace_str);
    if fs::symlink_metadata(&marketplace_path).is_ok() && !marketplace_is_ours {
        return Err(IntegrateError::Message(format!(
            "Refusing to take over existing marketplace directory {}; it was not created by proxenos.",
            marketplace_path.display()
        )));
    }

    let resolved_settings = resolve_settings_target(&settings_path)?;
    if let Some(record) = &existing {
        check_settings_target_unchanged(record, &resolved_settings, &settings_path)?;
    }
    let (mut settings_map, orig_bytes, orig_mode) = match read_optional(&resolved_settings.target)?
    {
        Some(data) => (
            parse_settings(&settings_path, &data.bytes)?,
            Some(data.bytes),
            data.mode,
        ),
        None => (Map::new(), None, None),
    };

    let ekm = section_object(&settings_map, "extraKnownMarketplaces", &settings_path)?;
    let ep = section_object(&settings_map, "enabledPlugins", &settings_path)?;
    check_entry_ownership(
        ekm,
        "extraKnownMarketplaces",
        MARKETPLACE_NAME,
        existing
            .as_ref()
            .map(|r| &r.entries.extra_known_marketplace),
        &settings_path,
    )?;
    check_entry_ownership(
        ep,
        "enabledPlugins",
        ENABLED_PLUGIN_KEY,
        existing.as_ref().map(|r| &r.entries.enabled_plugin),
        &settings_path,
    )?;

    let desired_marketplace = json!({
        "source": {
            "source": "directory",
            "path": marketplace_str
        }
    });
    let desired_enabled = Value::Bool(true);

    let mut created_dirs = existing
        .as_ref()
        .map(|record| record.created_dirs.clone())
        .unwrap_or_default();

    let claude_dir = settings_path
        .parent()
        .expect("settings path has a parent directory");
    state::ensure_private_dir_tree(claude_dir, claude_dir, &mut created_dirs)
        .map_err(io_err(claude_dir))?;

    let integrate_dir = state_dir.join("integrate");
    state::ensure_private_dir_tree(&state_dir, &integrate_dir, &mut created_dirs)
        .map_err(io_err(&integrate_dir))?;

    let marketplace_meta_dir = marketplace_path.join(".claude-plugin");
    state::ensure_private_dir_tree(&state_dir, &marketplace_meta_dir, &mut created_dirs)
        .map_err(io_err(&marketplace_meta_dir))?;

    let plugin_dir = marketplace_path.join("plugins").join(PLUGIN_NAME);
    let plugin_meta_dir = plugin_dir.join(".claude-plugin");
    state::ensure_private_dir_tree(&state_dir, &plugin_meta_dir, &mut created_dirs)
        .map_err(io_err(&plugin_meta_dir))?;

    let bfile = backup_path(&state_dir);
    let (backup, took_backup) = if bfile.exists() {
        (
            Some(state::path_string(&bfile).map_err(io_err(&bfile))?),
            false,
        )
    } else if existing.is_none()
        && let Some(bytes) = orig_bytes.as_deref()
    {
        let backups_dir = integrate_dir.join("backups");
        state::ensure_private_dir_tree(&state_dir, &backups_dir, &mut created_dirs)
            .map_err(io_err(&backups_dir))?;
        state::write_atomic(&bfile, bytes, None).map_err(io_err(&bfile))?;
        (
            Some(state::path_string(&bfile).map_err(io_err(&bfile))?),
            true,
        )
    } else {
        (None, false)
    };

    let marketplace_json_path = marketplace_meta_dir.join("marketplace.json");
    let plugin_json_path = plugin_meta_dir.join("plugin.json");
    let mcp_json_path = plugin_dir.join(".mcp.json");

    let had_ekm = settings_map.contains_key("extraKnownMarketplaces");
    let had_ep = settings_map.contains_key("enabledPlugins");
    let created_ekm = existing
        .as_ref()
        .map_or(!had_ekm, |record| record.created_extra_known_marketplaces);
    let created_ep = existing
        .as_ref()
        .map_or(!had_ep, |record| record.created_enabled_plugins);
    let settings_created = existing
        .as_ref()
        .map_or(orig_bytes.is_none(), |record| record.settings_created);

    let needs_ekm = settings_map
        .get("extraKnownMarketplaces")
        .and_then(Value::as_object)
        .and_then(|map| map.get(MARKETPLACE_NAME))
        != Some(&desired_marketplace);
    let needs_ep = settings_map
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .and_then(|map| map.get(ENABLED_PLUGIN_KEY))
        != Some(&desired_enabled);
    let will_write_settings = orig_bytes.is_none() || needs_ekm || needs_ep;

    let files = vec![
        state::path_string(&marketplace_json_path).map_err(io_err(&marketplace_json_path))?,
        state::path_string(&plugin_json_path).map_err(io_err(&plugin_json_path))?,
        state::path_string(&mcp_json_path).map_err(io_err(&mcp_json_path))?,
    ];

    let preserved_sha256 = existing
        .as_ref()
        .and_then(|record| record.settings_written_sha256.clone());

    let mut record = ClientRecord {
        client: CLIENT_CLAUDE_CODE.to_owned(),
        pending: true,
        marketplace_dir: marketplace_str.clone(),
        binary_path: binary_str.clone(),
        settings_path: settings_str.clone(),
        settings_created,
        settings_is_symlink: resolved_settings.symlink_target.is_some(),
        settings_symlink_target: resolved_settings.symlink_target,
        created_extra_known_marketplaces: created_ekm,
        created_enabled_plugins: created_ep,
        backup_path: backup,
        settings_written_sha256: if will_write_settings {
            None
        } else {
            preserved_sha256.clone()
        },
        entries: RecordedEntries {
            extra_known_marketplace: desired_marketplace.clone(),
            enabled_plugin: desired_enabled.clone(),
        },
        files,
        created_dirs,
    };

    // Persist an intent manifest before modifying plugin files or settings.json so an interrupted
    // write never strands entries as foreign or hides them from `--remove`.
    manifest.schema_version = MANIFEST_SCHEMA_VERSION;
    manifest
        .clients
        .insert(CLIENT_CLAUDE_CODE.to_owned(), record.clone());
    let pending_value = serde_json::to_value(&manifest).expect("manifest serializes");
    write_json_atomic(&manifest_file, &pending_value, None)?;

    let marketplace_json = json!({
        "name": MARKETPLACE_NAME,
        "owner": {
            "name": "Proxenos"
        },
        "plugins": [
            {
                "name": PLUGIN_NAME,
                "description": PLUGIN_DESCRIPTION,
                "source": format!("./plugins/{PLUGIN_NAME}")
            }
        ]
    });
    let plugin_json = json!({
        "name": PLUGIN_NAME,
        "description": PLUGIN_DESCRIPTION,
        "version": env!("CARGO_PKG_VERSION"),
        "author": {
            "name": "Proxenos"
        }
    });
    let mcp_json = json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": binary_str,
                "args": ["mcp", "--client", "claude"]
            }
        }
    });

    write_json_atomic(&marketplace_json_path, &marketplace_json, None)?;
    write_json_atomic(&plugin_json_path, &plugin_json, None)?;
    write_json_atomic(&mcp_json_path, &mcp_json, None)?;

    // Byte-exact restore is only eligible when the bytes match what the backup-taking integrate
    // wrote; any later rewrite clears the digest so `--remove` edits semantically instead.
    let settings_written_sha256 = if will_write_settings {
        settings_map
            .entry("extraKnownMarketplaces".to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("extraKnownMarketplaces validated as object")
            .insert(MARKETPLACE_NAME.to_owned(), desired_marketplace);
        settings_map
            .entry("enabledPlugins".to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("enabledPlugins validated as object")
            .insert(ENABLED_PLUGIN_KEY.to_owned(), desired_enabled);
        let written = write_json_atomic(
            &resolved_settings.target,
            &Value::Object(settings_map),
            orig_mode,
        )?;
        took_backup.then(|| sha256_hex(&written))
    } else {
        preserved_sha256
    };

    let status_line = match &existing {
        None => "Integrated Proxenos with Claude Code.",
        Some(previous) if previous.pending || previous.binary_path != binary_str => {
            "Updated Proxenos Claude Code integration."
        }
        Some(_) => "Proxenos Claude Code integration is up to date.",
    };

    #[cfg(feature = "test-tools")]
    if std::env::var_os("PROXENOS_TEST_FAIL_FINAL_MANIFEST").is_some() {
        return Err(IntegrateError::Message(
            "Injected failure before committing final integration manifest.".to_owned(),
        ));
    }

    record.pending = false;
    record.settings_written_sha256 = settings_written_sha256;
    manifest
        .clients
        .insert(CLIENT_CLAUDE_CODE.to_owned(), record);
    let manifest_value = serde_json::to_value(&manifest).expect("manifest serializes");
    write_json_atomic(&manifest_file, &manifest_value, None)?;

    Ok(format!(
        "{status_line}\n  marketplace: {marketplace_str}\n  settings: {settings_str}\n  binary: {binary_str}\n"
    ))
}

fn remove_managed_entry(
    settings: &mut Map<String, Value>,
    section: &str,
    key: &str,
    expected: &Value,
    created_section: bool,
    path: &Path,
    refused: &mut Vec<String>,
) -> bool {
    let Some(map) = settings.get_mut(section).and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(current) = map.get(key) else {
        return false;
    };
    if current != expected {
        refused.push(format!(
            "Refused to remove {section}.{key} from {}: entry was modified after integrate.",
            path.display()
        ));
        return false;
    }
    map.shift_remove(key);
    if created_section && map.is_empty() {
        settings.shift_remove(section);
    }
    true
}

pub fn remove_claude_code() -> Result<String, IntegrateError> {
    let state_dir = state::require_state_dir().map_err(IntegrateError::Message)?;
    let manifest_file = manifest_path(&state_dir);
    let mut manifest = load_manifest(&manifest_file)?;
    let Some(record) = manifest.clients.get(CLIENT_CLAUDE_CODE).cloned() else {
        return Ok("Claude Code integration is not installed; nothing to remove.\n".to_owned());
    };

    let settings_path = Path::new(&record.settings_path);
    let resolved_settings = resolve_settings_target(settings_path)?;
    check_settings_target_unchanged(&record, &resolved_settings, settings_path)?;
    let mut refused = Vec::new();
    if let Some(data) = read_optional(&resolved_settings.target)? {
        let restore_bytes = if let (Some(expected), Some(backup_path)) = (
            record.settings_written_sha256.as_deref(),
            record.backup_path.as_deref(),
        ) && expected == sha256_hex(&data.bytes)
        {
            read_optional(Path::new(backup_path))?.map(|file| file.bytes)
        } else {
            None
        };
        if let Some(original_bytes) = restore_bytes {
            state::write_atomic(&resolved_settings.target, &original_bytes, data.mode)
                .map_err(io_err(&resolved_settings.target))?;
        } else {
            let mut settings_map = parse_settings(settings_path, &data.bytes)?;
            let _ = section_object(&settings_map, "extraKnownMarketplaces", settings_path)?;
            let _ = section_object(&settings_map, "enabledPlugins", settings_path)?;

            let removed_ekm = remove_managed_entry(
                &mut settings_map,
                "extraKnownMarketplaces",
                MARKETPLACE_NAME,
                &record.entries.extra_known_marketplace,
                record.created_extra_known_marketplaces,
                settings_path,
                &mut refused,
            );
            let removed_ep = remove_managed_entry(
                &mut settings_map,
                "enabledPlugins",
                ENABLED_PLUGIN_KEY,
                &record.entries.enabled_plugin,
                record.created_enabled_plugins,
                settings_path,
                &mut refused,
            );

            if removed_ekm || removed_ep {
                if record.settings_created
                    && resolved_settings.symlink_target.is_none()
                    && settings_map.is_empty()
                {
                    remove_if_exists(settings_path)?;
                } else {
                    write_json_atomic(
                        &resolved_settings.target,
                        &Value::Object(settings_map),
                        data.mode,
                    )?;
                }
            }
        }
    }

    for file in &record.files {
        remove_if_exists(Path::new(file))?;
    }
    if let Some(backup) = &record.backup_path {
        remove_if_exists(Path::new(backup))?;
    }

    manifest.clients.remove(CLIENT_CLAUDE_CODE);
    if manifest.clients.is_empty() {
        remove_if_exists(&manifest_file)?;
    } else {
        let manifest_value = serde_json::to_value(&manifest).expect("manifest serializes");
        write_json_atomic(&manifest_file, &manifest_value, None)?;
    }

    for dir in record.created_dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }

    if !refused.is_empty() {
        return Err(IntegrateError::Message(refused.join("\n")));
    }

    Ok("Removed Proxenos Claude Code integration.\n".to_owned())
}
