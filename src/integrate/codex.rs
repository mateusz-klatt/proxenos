//! Pointwise edits to Codex's user MCP configuration (D10).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml_edit::{Array, Document, Item, Table, TableLike, Value, value};

use super::{
    FileData, IntegrateError, MCP_SERVER_NAME, Manifest, ResolvedSettingsPath,
    check_settings_path_unchanged, check_settings_target_unchanged, io_err, load_manifest,
    manifest_path, read_optional, remove_if_exists, resolve_settings_target, sha256_hex,
    shared_created_dirs, write_manifest,
};
use crate::state;

pub(super) const CLIENT: &str = "codex";
const DISPLAY_NAME: &str = "Codex";
const SERVERS: &str = "mcp_servers";
const ARGS: &[&str] = &["mcp", "--client", "codex"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    command: String,
    args: Vec<String>,
}

impl Registration {
    fn is_valid(&self) -> bool {
        Path::new(&self.command).is_absolute() && self.args == ARGS
    }

    fn matches(&self, item: &Item) -> bool {
        let Some(table) = item.as_table_like() else {
            return false;
        };
        table.len() == 2
            && table.get("command").and_then(Item::as_str) == Some(&self.command)
            && table
                .get("args")
                .and_then(Item::as_array)
                .is_some_and(|args| {
                    args.iter()
                        .map(Value::as_str)
                        .eq(self.args.iter().map(|arg| Some(arg.as_str())))
                })
    }

    fn arguments(&self) -> Array {
        self.args.iter().cloned().map(Value::from).collect()
    }

    fn table(&self) -> Table {
        let mut table = Table::new();
        table.insert("command", value(&self.command));
        table.insert("args", value(self.arguments()));
        table
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    client: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
    binary_path: String,
    settings_path: String,
    settings_created: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    settings_is_symlink: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    settings_symlink_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    settings_written_sha256: Option<String>,
    entries: Registration,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_entries: Option<Registration>,
    created_dirs: Vec<String>,
}

impl Record {
    fn check_path(
        &self,
        path: &Path,
        path_string: &str,
        resolved: &ResolvedSettingsPath,
    ) -> Result<(), IntegrateError> {
        check_settings_path_unchanged(DISPLAY_NAME, &self.settings_path, path_string)?;
        check_settings_target_unchanged(
            self.settings_is_symlink,
            self.settings_symlink_target.as_deref(),
            resolved,
            path,
        )?;
        if self.settings_is_symlink && resolved.symlink_target.is_none() {
            return Err(IntegrateError::Message(format!(
                "Refusing to modify {}: settings symlink is missing.",
                path.display()
            )));
        }
        Ok(())
    }

    fn owns(&self, item: &Item) -> bool {
        self.entries.matches(item)
            || (self.pending
                && self
                    .previous_entries
                    .as_ref()
                    .is_some_and(|entry| entry.matches(item)))
    }
}

fn read_record(value: &JsonValue) -> Result<Record, IntegrateError> {
    let record: Record = serde_json::from_value(value.clone()).map_err(|error| {
        IntegrateError::Message(format!("Could not parse Codex integration record: {error}"))
    })?;
    if record.client != CLIENT
        || !record.entries.is_valid()
        || record.binary_path != record.entries.command
        || record.previous_entries.as_ref().is_some_and(|previous| {
            !record.pending || !previous.is_valid() || previous.command == record.entries.command
        })
    {
        return Err(IntegrateError::Message(
            "Codex integration manifest contains an invalid command or arguments.".to_owned(),
        ));
    }
    Ok(record)
}

fn manifest_record(manifest: &Manifest) -> Result<Option<Record>, IntegrateError> {
    manifest.clients.get(CLIENT).map(read_record).transpose()
}

fn backups_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("integrate").join("backups")
}

fn backup_file(state_dir: &Path) -> PathBuf {
    backups_dir(state_dir).join("codex-config.toml")
}

fn config_path() -> Result<PathBuf, IntegrateError> {
    state::codex_settings_path().map_err(IntegrateError::Message)
}

fn read_config(path: &Path) -> Result<Option<FileData>, IntegrateError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(IntegrateError::Message(format!(
            "Codex configuration {} is not a regular file.",
            path.display()
        )));
    }
    read_optional(path)
}

fn parse_config<'a>(
    path: &Path,
    data: Option<&'a FileData>,
) -> Result<Document<&'a str>, IntegrateError> {
    let raw = std::str::from_utf8(data.map_or(&[], |file| &file.bytes)).map_err(|error| {
        IntegrateError::Message(format!(
            "Codex configuration {} is not UTF-8: {error}",
            path.display()
        ))
    })?;
    Document::parse(raw).map_err(|error| {
        IntegrateError::Message(format!(
            "Could not parse Codex configuration {}: {}",
            path.display(),
            error.message()
        ))
    })
}

fn servers<'a>(
    doc: &'a Document<&str>,
    path: &Path,
) -> Result<Option<&'a dyn TableLike>, IntegrateError> {
    doc.as_table()
        .get(SERVERS)
        .map(|item| {
            item.as_table_like().ok_or_else(|| {
                IntegrateError::Message(format!(
                    "Key `{SERVERS}` in {} must be a TOML table.",
                    path.display()
                ))
            })
        })
        .transpose()
}

fn modified(path: &Path, operation: &str) -> IntegrateError {
    IntegrateError::Message(format!(
        "Refusing to {operation} {SERVERS}.{MCP_SERVER_NAME} in {}: entry was modified after integrate.",
        path.display()
    ))
}

fn insert_registration(doc: &Document<&str>, entry: &Registration) -> String {
    if doc
        .as_table()
        .get(SERVERS)
        .is_some_and(Item::is_inline_table)
    {
        let mut doc = doc.clone().into_mut();
        doc[SERVERS]
            .as_table_like_mut()
            .expect("inline table is table-like")
            .insert(MCP_SERVER_NAME, Item::Table(entry.table()));
        return doc.to_string();
    }
    // Append after existing trailing comments so another tool's closing marker stays with
    // its own block. Build the two values with the TOML encoder to escape paths correctly.
    let mut text = doc.raw().to_owned();
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str(eol);
    }
    use std::fmt::Write;
    write!(
        text,
        "[{SERVERS}.{MCP_SERVER_NAME}]{eol}command = {}{eol}args = {}{eol}",
        Value::from(&entry.command),
        entry.arguments(),
    )
    .expect("writing to a String succeeds");
    text
}

fn replace_command(doc: &Document<&str>, current: &Item, entry: &Registration) -> String {
    let command = current
        .as_table_like()
        .and_then(|table| table.get("command"))
        .and_then(Item::span)
        .expect("owned command has a source span");
    let mut text = doc.raw().to_owned();
    text.replace_range(command, &Value::from(&entry.command).to_string());
    text
}

fn remove_registration(doc: &Document<&str>, current: &Item) -> String {
    if let Some(table) = current.as_table().filter(|table| !table.is_dotted()) {
        // The managed table has precisely two scalar/array values. Span from its opening
        // bracket through the last value, leaving comments on either side where they were.
        let header = table.span().expect("managed table has a source span");
        let last = table
            .iter()
            .map(|(_, item)| item.span().expect("managed value has a source span").end)
            .max()
            .expect("managed table has entries");
        let raw = doc.raw();
        let start = raw[..header.start].rfind('\n').map_or(0, |index| index + 1);
        let end = raw[last..]
            .find('\n')
            .map_or(raw.len(), |index| last + index + 1);
        let mut text = raw.to_owned();
        text.replace_range(start..end, "");
        return text;
    }
    // An existing inline table must stay inline. The same edit also handles a registration
    // the user reformatted as dotted keys without changing its values.
    let mut doc = doc.clone().into_mut();
    doc[SERVERS]
        .as_table_like_mut()
        .expect("managed parent is table-like")
        .remove(MCP_SERVER_NAME);
    doc.to_string()
}

pub fn integrate_codex() -> Result<String, IntegrateError> {
    let state_dir = state::require_state_dir().map_err(IntegrateError::Message)?;
    let path = config_path()?;
    let path_string = state::path_string(&path).map_err(io_err(&path))?;
    let binary = state::current_executable().map_err(io_err("current executable"))?;
    let binary_path = state::path_string(&binary).map_err(io_err(&binary))?;
    let entry = Registration {
        command: binary_path.clone(),
        args: ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
    };

    let manifest_file = manifest_path(&state_dir);
    let _lock = state::lock_integration(&state_dir).map_err(io_err(&manifest_file))?;
    let mut manifest = load_manifest(&manifest_file)?;
    let existing = manifest_record(&manifest)?;
    let resolved = resolve_settings_target(&path)?;
    if let Some(record) = &existing {
        record.check_path(&path, &path_string, &resolved)?;
    }
    let data = read_config(&resolved.target)?;
    let doc = parse_config(&path, data.as_ref())?;
    let current = servers(&doc, &path)?.and_then(|servers| servers.get(MCP_SERVER_NAME));
    if let Some(current) = current {
        let Some(record) = &existing else {
            return Err(IntegrateError::Message(format!(
                "Refusing to take over existing {SERVERS}.{MCP_SERVER_NAME} in {}; it was not written by proxenos.",
                path.display()
            )));
        };
        if !record.owns(current) {
            return Err(modified(&path, "overwrite"));
        }
    }

    let previous_entries = current
        .filter(|current| !entry.matches(current))
        .and_then(|current| {
            existing.as_ref().map(|record| {
                if record.entries.matches(current) {
                    record.entries.clone()
                } else {
                    record
                        .previous_entries
                        .clone()
                        .expect("previous registration is owned")
                }
            })
        });
    let settings_created = existing
        .as_ref()
        .map_or(data.is_none(), |record| record.settings_created);
    let writing = match current {
        Some(current) if entry.matches(current) => None,
        Some(current) => Some(replace_command(&doc, current, &entry)),
        None => Some(insert_registration(&doc, &entry)),
    };

    let backup_dir = backups_dir(&state_dir);
    let backup = backup_file(&state_dir);
    let backup_string = state::path_string(&backup).map_err(io_err(&backup))?;
    let backup_exists = fs::symlink_metadata(&backup).is_ok();
    let recorded_backup = existing
        .as_ref()
        .and_then(|record| record.backup_path.as_deref());
    if backup_exists && recorded_backup != Some(&backup_string) {
        return Err(IntegrateError::Message(format!(
            "Refusing to use existing backup {}; it was not written by proxenos.",
            backup.display()
        )));
    }
    let need_backup = !backup_exists
        && data.is_some()
        && !settings_created
        && current.is_none()
        && existing.as_ref().is_none_or(|record| record.pending);
    let backup_path = (backup_exists || need_backup).then_some(backup_string);
    let previous_digest = existing
        .as_ref()
        .and_then(|record| record.settings_written_sha256.clone());
    let settings_written_sha256 = match &writing {
        None => previous_digest,
        Some(text) => {
            let digest = sha256_hex(text.as_bytes());
            (need_backup || (backup_exists && previous_digest.as_ref() == Some(&digest)))
                .then_some(digest)
        }
    };

    let integrate_dir = state_dir.join("integrate");
    let config_dir = path.parent().expect("Codex settings path has a parent");
    let mut created_dirs = existing
        .as_ref()
        .map(|record| record.created_dirs.clone())
        .unwrap_or_default();
    state::plan_private_dir_tree(&state_dir, &integrate_dir, &mut created_dirs)
        .map_err(io_err(&integrate_dir))?;
    state::plan_private_dir_tree(config_dir, config_dir, &mut created_dirs)
        .map_err(io_err(config_dir))?;
    if need_backup {
        state::plan_private_dir_tree(&state_dir, &backup_dir, &mut created_dirs)
            .map_err(io_err(&backup_dir))?;
    }
    let created_dirs = shared_created_dirs(&state_dir, config_dir, &manifest, created_dirs)?;

    let mut record = Record {
        client: CLIENT.to_owned(),
        pending: true,
        binary_path: binary_path.clone(),
        settings_path: path_string.clone(),
        settings_created,
        settings_is_symlink: resolved.symlink_target.is_some(),
        settings_symlink_target: resolved.symlink_target,
        backup_path,
        settings_written_sha256,
        entries: entry,
        previous_entries,
        created_dirs,
    };

    state::ensure_private_dir_tree(&state_dir, &integrate_dir).map_err(io_err(&integrate_dir))?;
    manifest.insert(CLIENT, &record);
    write_manifest(&manifest_file, &manifest)?;

    if need_backup {
        state::ensure_private_dir_tree(&state_dir, &backup_dir).map_err(io_err(&backup_dir))?;
        let source = data
            .as_ref()
            .expect("backup requires an existing configuration");
        state::write_atomic(&backup, &source.bytes, None).map_err(io_err(&backup))?;
    }
    if let Some(text) = &writing {
        state::ensure_private_dir_tree(config_dir, config_dir).map_err(io_err(config_dir))?;
        state::write_atomic(
            &resolved.target,
            text.as_bytes(),
            data.as_ref().and_then(|file| file.mode),
        )
        .map_err(io_err(&resolved.target))?;
    }

    #[cfg(feature = "test-tools")]
    if std::env::var_os("PROXENOS_TEST_FAIL_FINAL_MANIFEST").is_some() {
        return Err(IntegrateError::Message(
            "Injected failure before committing final integration manifest.".to_owned(),
        ));
    }

    record.pending = false;
    record.previous_entries = None;
    manifest.insert(CLIENT, &record);
    write_manifest(&manifest_file, &manifest)?;
    let status = match existing {
        None => "Integrated Proxenos with Codex.",
        Some(previous) if previous.pending || writing.is_some() => {
            "Updated Proxenos Codex integration."
        }
        Some(_) => "Proxenos Codex integration is up to date.",
    };
    Ok(format!(
        "{status}\n  settings: {path_string}\n  binary: {binary_path}\n"
    ))
}

pub fn remove_codex() -> Result<String, IntegrateError> {
    let state_dir = state::require_state_dir().map_err(IntegrateError::Message)?;
    let path = config_path()?;
    let manifest_file = manifest_path(&state_dir);
    let _lock = state::lock_integration(&state_dir).map_err(io_err(&manifest_file))?;
    let mut manifest = load_manifest(&manifest_file)?;
    let Some(record) = manifest_record(&manifest)? else {
        return Ok("Codex integration is not installed; nothing to remove.\n".to_owned());
    };
    let resolved = resolve_settings_target(&path)?;
    let path_string = state::path_string(&path).map_err(io_err(&path))?;
    record.check_path(&path, &path_string, &resolved)?;

    let backup = backup_file(&state_dir);
    let backup_string = state::path_string(&backup).map_err(io_err(&backup))?;
    let recorded_backup = record.backup_path.as_deref() == Some(&backup_string);
    if let Some(data) = read_config(&resolved.target)? {
        let doc = parse_config(&path, Some(&data))?;
        if let Some(current) =
            servers(&doc, &path)?.and_then(|servers| servers.get(MCP_SERVER_NAME))
        {
            if !record.owns(current) {
                return Err(modified(&path, "remove"));
            }
            let restored = if recorded_backup
                && record.settings_written_sha256.as_deref() == Some(&sha256_hex(&data.bytes))
            {
                read_config(&backup)?.map(|file| file.bytes)
            } else {
                None
            };
            let bytes = restored.unwrap_or_else(|| remove_registration(&doc, current).into_bytes());
            if record.settings_created
                && resolved.symlink_target.is_none()
                && bytes.iter().all(u8::is_ascii_whitespace)
            {
                remove_if_exists(&path)?;
            } else {
                state::write_atomic(&resolved.target, &bytes, data.mode)
                    .map_err(io_err(&resolved.target))?;
            }
        }
    }
    if recorded_backup {
        remove_if_exists(&backup)?;
    }
    manifest.clients.remove(CLIENT);
    if manifest.clients.is_empty() {
        remove_if_exists(&manifest_file)?;
    } else {
        write_manifest(&manifest_file, &manifest)?;
    }
    for dir in record.created_dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
    Ok("Removed Proxenos Codex integration.\n".to_owned())
}

pub fn read_codex_registration(
    value: &JsonValue,
) -> Result<(PathBuf, PathBuf, Vec<String>), IntegrateError> {
    let record = read_record(value)?;
    if record.pending {
        return Err(IntegrateError::Message(
            "Codex registration is pending. Run `proxenos integrate codex` to recover it."
                .to_owned(),
        ));
    }
    let path = config_path()?;
    let resolved = resolve_settings_target(&path)?;
    let path_string = state::path_string(&path).map_err(io_err(&path))?;
    record.check_path(&path, &path_string, &resolved)?;
    let data = read_config(&resolved.target)?;
    let doc = parse_config(&path, data.as_ref())?;
    let current = servers(&doc, &path)?
        .and_then(|servers| servers.get(MCP_SERVER_NAME))
        .ok_or_else(|| {
            IntegrateError::Message(format!(
                "No {SERVERS}.{MCP_SERVER_NAME} entry in {}.",
                path.display()
            ))
        })?;
    if !record.owns(current) {
        return Err(modified(&path, "use"));
    }
    let command = PathBuf::from(&record.entries.command);
    Ok((path, command, record.entries.args))
}
