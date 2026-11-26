use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use toml_edit::{DocumentMut, Item, value};
use uuid::Uuid;

use super::{Sandbox, absolute_path_string, find_check, proxenos_bin, read_json, try_file_symlink};

const INSTALL: &[&str] = &["integrate", "codex"];
const REMOVE: &[&str] = &["integrate", "codex", "--remove"];
const DOCTOR: &[&str] = &["doctor", "--json"];
const CLAUDE_INSTALL: &[&str] = &["integrate", "claude-code"];
const CLAUDE_REMOVE: &[&str] = &["integrate", "claude-code", "--remove"];
const ARGS: [&str; 3] = ["mcp", "--client", "codex"];
const LOCK_FILE: &str = ".proxenos-integrate.lock";
const LOCK_SENTINEL: &[u8] = b"lock contents must survive all client operations";

fn command(sb: &Sandbox, args: &[&str]) -> Command {
    let mut cmd = sb.command(args);
    cmd.env_remove("CODEX_HOME");
    cmd
}

fn run(sb: &Sandbox, args: &[&str]) -> Output {
    command(sb, args).output().unwrap()
}

fn config_path(sb: &Sandbox) -> PathBuf {
    sb.home.join(".codex/config.toml")
}

fn backup_path(sb: &Sandbox) -> PathBuf {
    sb.state_dir().join("integrate/backups/codex-config.toml")
}

fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

fn write_config(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read_config(path: &Path) -> DocumentMut {
    fs::read_to_string(path).unwrap().parse().unwrap()
}

fn server<'a>(doc: &'a DocumentMut, name: &str) -> Option<&'a dyn toml_edit::TableLike> {
    doc.get("mcp_servers")?
        .as_table_like()?
        .get(name)?
        .as_table_like()
}

fn assert_registered(path: &Path, binary: &Path) {
    let doc = read_config(path);
    let server = server(&doc, "proxenos").expect("Proxenos MCP registration");
    let expected = absolute_path_string(binary);
    assert_eq!(
        server.get("command").and_then(Item::as_str),
        Some(expected.as_str())
    );
    let args = server.get("args").and_then(Item::as_array).unwrap();
    assert_eq!(
        args.iter()
            .map(|item| item.as_str().unwrap())
            .collect::<Vec<_>>(),
        ARGS
    );
}

#[track_caller]
fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "process status: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn report(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap()
}

fn non_provider_checks(report: &Value) -> Vec<&str> {
    let names: Vec<_> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"cli.codex"));
    for name in &names {
        assert_eq!(
            names.iter().filter(|other| *other == name).count(),
            1,
            "duplicated check {name}"
        );
    }
    names
        .into_iter()
        .filter(|name| !name.starts_with("cli."))
        .collect()
}

#[test]
fn fresh_registration_round_trips_through_doctor_and_remove() {
    let sb = Sandbox::with_name(&format!("proxenos Codex árvíz {}", Uuid::new_v4()));
    let path = config_path(&sb);
    let install = command(&sb, INSTALL)
        .env("CODEX_HOME", "")
        .output()
        .unwrap();
    assert_success(&install);
    assert_registered(&path, proxenos_bin());

    let written = fs::read(&path).unwrap();
    let manifest = read_json(&sb.manifest_path());
    assert_eq!(manifest["schema_version"], 1);
    assert!(manifest["clients"]["codex"].is_object());
    assert!(manifest["clients"].get("claude-code").is_none());
    assert_success(&run(&sb, INSTALL));
    assert_eq!(fs::read(&path).unwrap(), written);
    assert_eq!(read_json(&sb.manifest_path()), manifest);
    assert_success(&run(&sb, CLAUDE_REMOVE));
    assert_eq!(fs::read(&path).unwrap(), written);
    assert_eq!(read_json(&sb.manifest_path()), manifest);

    let out = run(&sb, DOCTOR);
    assert_success(&out);
    let report = report(&out);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(
        non_provider_checks(&report),
        [
            "codex.registration",
            "state_dir.permissions",
            "codex.handshake"
        ]
    );
    assert_eq!(find_check(&report, "codex.registration")["status"], "ok");
    let handshake = find_check(&report, "codex.handshake");
    assert_eq!(handshake["details"]["args"], serde_json::json!(ARGS));
    assert_eq!(handshake["details"]["server_name"], "proxenos");

    assert_success(&run(&sb, REMOVE));
    assert!(!path.exists());
    assert!(!sb.manifest_path().exists());
    assert_success(&run(&sb, REMOVE));
}

#[test]
fn remove_without_state_leaves_foreign_codex_registration_untouched() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let original = "[mcp_servers.proxenos]\ncommand = 'user-owned-launcher'\n";
    write_config(&path, original);
    assert!(!sb.state_dir().exists());

    assert_success(&run(&sb, REMOVE));
    assert!(!sb.state_dir().exists());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn registration_preserves_comments_and_neighboring_tables() {
    for existing in [
        "[mcp_servers.iris]\ncommand = 'keep-iris' # launcher note\nargs = ['mcp']\n",
        "mcp_servers = { iris = { command = 'keep-iris', args = ['mcp'] } } # inline note\n",
        "mcp_servers.iris.command = 'keep-iris' # dotted note\nmcp_servers.iris.args = ['mcp']\n",
    ] {
        let sb = Sandbox::new();
        let path = config_path(&sb);
        let original = format!(
            "# personal configuration\nmodel = 'keep-model'\n\n# >>> iris >>>\n{existing}# <<< iris <<<\n"
        );
        write_config(&path, &original);
        assert_success(&run(&sb, INSTALL));
        assert_registered(&path, proxenos_bin());
        assert_neighbors_preserved(&path);
        assert_success(&run(&sb, REMOVE));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        assert_success(&run(&sb, INSTALL));
        let mut edited = read_config(&path);
        edited["user_edit"] = value(true);
        fs::write(&path, edited.to_string()).unwrap();
        assert_success(&run(&sb, REMOVE));

        let doc = read_config(&path);
        assert_eq!(doc["user_edit"].as_bool(), Some(true));
        assert!(server(&doc, "proxenos").is_none());
        assert_neighbors_preserved(&path);
    }
}

fn assert_neighbors_preserved(path: &Path) {
    let text = fs::read_to_string(path).unwrap();
    let doc = read_config(path);
    assert_eq!(doc["model"].as_str(), Some("keep-model"));
    assert_eq!(
        server(&doc, "iris")
            .unwrap()
            .get("command")
            .and_then(Item::as_str),
        Some("keep-iris")
    );
    for marker in [
        "# personal configuration",
        "# >>> iris >>>",
        "# <<< iris <<<",
        " note",
    ] {
        assert!(text.contains(marker), "missing {marker:?} in {text}");
    }
}

#[test]
fn inline_registration_changes_only_its_own_bytes_in_crlf_document() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let original = concat!(
        "# Codex settings\r\n",
        "model = 'original-model' # keep model comment\r\n",
        "\r\n",
        "mcp_servers = { iris = { command = 'keep-iris#literal', args = ['serve'] }, ",
        "sidecar = { command = 'keep-sidecar' }  \t } # keep inline comment\r\n",
        "\r\n",
        "# Unrelated section  \r\n",
        "[ui]\r\n",
        "show_notifications = true # keep UI comment\r\n",
    );
    write_config(&path, original);
    assert_success(&run(&sb, INSTALL));
    assert_registered(&path, proxenos_bin());

    let installed = fs::read_to_string(&path).unwrap();
    assert_only_insertion(original, &installed);

    // A raw edit models the user changing another setting without normalizing CRLF.
    let edited = installed.replacen("'original-model'", "'edited-model'", 1);
    assert_ne!(edited, installed);
    fs::write(&path, edited).unwrap();
    assert_success(&run(&sb, REMOVE));
    let expected = original.replacen("'original-model'", "'edited-model'", 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
}

fn assert_only_insertion(original: &str, installed: &str) {
    let original_bytes = original.as_bytes();
    let installed_bytes = installed.as_bytes();
    let added = installed_bytes
        .len()
        .checked_sub(original_bytes.len())
        .unwrap();
    assert!(added > 0);
    let insertion = installed_bytes
        .iter()
        .zip(original_bytes)
        .position(|(new, old)| new != old)
        .expect("the inline registration was inserted within the document");
    assert_eq!(
        &installed_bytes[insertion + added..],
        &original_bytes[insertion..],
        "the installer must preserve all original bytes: {installed:?}"
    );
}

#[test]
fn inline_registration_preserves_empty_parent_and_existing_trailing_comma() {
    for table in [
        "mcp_servers = { \t } # keep empty table\r\n",
        concat!(
            "mcp_servers = {\r\n",
            "  iris = { command = 'keep-iris' }, # keep trailing comma\r\n",
            "} # keep closing comment\r\n",
        ),
    ] {
        let sb = Sandbox::new();
        let path = config_path(&sb);
        let original = format!("model = 'before'\r\n{table}");
        write_config(&path, &original);
        assert_success(&run(&sb, INSTALL));
        assert_registered(&path, proxenos_bin());
        let installed = fs::read_to_string(&path).unwrap();
        assert_only_insertion(&original, &installed);

        let edited = installed.replacen("model = 'before'", "model = 'after'", 1);
        fs::write(&path, edited).unwrap();
        assert_success(&run(&sb, REMOVE));
        let expected = format!("model = 'after'\r\n{table}");
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    }
}

#[test]
fn inline_removal_preserves_comments_and_interleaved_dotted_members() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let before = "mcp_servers = {\r\n  iris.command = 'keep-iris', # First server\r\n";
    let neighbor = "sidecar.command = 'keep-sidecar'";
    let after = "} # keep closing comment\r\n";
    write_config(&path, &format!("{before}  {neighbor}\r\n{after}"));
    assert_success(&run(&sb, INSTALL));

    let launcher = toml_edit::Value::from(absolute_path_string(proxenos_bin()));
    let owned_command = format!("proxenos . command = {launcher},");
    let owned_args = "proxenos . args = ['mcp', '--client', 'codex']";
    let edited = format!(
        concat!(
            "{before}",
            "  {owned_command} # Keep command comment\r\n",
            "  {neighbor}, # Interleaved server\r\n",
            "  {owned_args} # Keep arguments comment\r\n",
            "{after}",
        ),
        before = before,
        owned_command = owned_command,
        neighbor = neighbor,
        owned_args = owned_args,
        after = after,
    );
    let expected = edited
        .replacen(&owned_command, "", 1)
        .replacen(&format!("{neighbor},"), neighbor, 1)
        .replacen(owned_args, "", 1);
    fs::write(&path, edited).unwrap();
    assert_registered(&path, proxenos_bin());
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    assert!(server(&read_config(&path), "proxenos").is_none());
}

#[test]
fn removal_preserves_crlf_and_comments_in_reformatted_dotted_registration() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let before = "# First server\r\nmcp_servers.iris.command = 'keep-iris#literal'\r\n";
    let middle = "# Interleaved server  \r\nmcp_servers.sidecar.command = 'keep-sidecar'\r\n";
    let after = "# Unrelated section\r\n[ui]\r\nshow_notifications = true\r\n";
    write_config(&path, &format!("{before}{middle}{after}"));
    assert_success(&run(&sb, INSTALL));

    let launcher = toml_edit::Value::from(absolute_path_string(proxenos_bin()));
    let edited = format!(
        concat!(
            "{before}",
            "mcp_servers.proxenos.command = {launcher} # Keep command comment\r\n",
            "{middle}",
            "mcp_servers.proxenos.args = [ # Keep opening comment\r\n",
            "  'mcp', # Keep argument comment  \r\n",
            "  # Keep standalone comment\t\r\n",
            "  '--client',\r\n",
            "  'codex',\r\n",
            "] # Keep closing comment\r\n",
            "{after}",
        ),
        before = before,
        launcher = launcher,
        middle = middle,
        after = after,
    );
    fs::write(&path, edited).unwrap();
    assert_registered(&path, proxenos_bin());
    assert_success(&run(&sb, REMOVE));

    let expected = format!(
        concat!(
            "{before}",
            "# Keep command comment\r\n",
            "{middle}",
            "# Keep opening comment\r\n",
            "  # Keep argument comment  \r\n",
            "  # Keep standalone comment\t\r\n",
            "# Keep closing comment\r\n",
            "{after}",
        ),
        before = before,
        middle = middle,
        after = after,
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    assert!(server(&read_config(&path), "proxenos").is_none());
}

#[test]
fn removal_preserves_comments_on_an_inline_member_under_a_regular_table() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let original = "[mcp_servers]\r\niris = { command = 'keep-iris' } # keep neighbor\r\n";
    write_config(&path, original);
    assert_success(&run(&sb, INSTALL));

    let command = toml_edit::Value::from(absolute_path_string(proxenos_bin()));
    let edited = format!(
        "{original}proxenos = {{ command = {command}, args = ['mcp', '--client', 'codex'] }} # keep user comment\r\n"
    );
    fs::write(&path, edited).unwrap();
    assert_registered(&path, proxenos_bin());
    assert_success(&run(&sb, REMOVE));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{original}# keep user comment\r\n")
    );
    assert!(server(&read_config(&path), "proxenos").is_none());
}

#[test]
fn removal_preserves_user_comments_within_managed_toml_table_and_arguments() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let binary = sb.hard_link_binary("proxenos # part of executable path");
    let original = "# First neighbor\r\n[mcp_servers.iris]\r\ncommand = 'keep-iris#literal'\r\n";
    let next = "# Next neighbor\r\n[mcp_servers.sidecar]\r\ncommand = 'keep-sidecar'\r\n";
    write_config(&path, original);
    let install = sb
        .command_for(&binary, INSTALL)
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    assert_success(&install);

    // Reformat the owned entry without changing either of its values.
    let launcher = toml_edit::Value::from(absolute_path_string(&binary));
    let edited = format!(
        concat!(
            "{original}",
            "# Before the managed table\r\n",
            "[mcp_servers.proxenos] # At the table header\t\r\n",
            "# Between the header and command  \r\n",
            "command = {launcher} # At the command\r\n",
            "args = [ # At the array opening\r\n",
            "  'mcp', # At the first argument  \r\n",
            "  # Between arguments\t\r\n",
            "  '--client',\r\n",
            "  'codex',\r\n",
            "] # At the array closing\r\n",
            "# After the managed table\r\n",
            "{next}",
        ),
        original = original,
        launcher = launcher,
        next = next,
    );
    fs::write(&path, edited).unwrap();
    assert_registered(&path, &binary);
    assert_success(&run(&sb, REMOVE));

    let remaining = fs::read_to_string(&path).unwrap();
    assert!(remaining.starts_with(original), "{remaining:?}");
    assert!(remaining.ends_with(next), "{remaining:?}");
    let mut comments = remaining.as_str();
    for comment in [
        "# Before the managed table\r\n",
        "# At the table header\t\r\n",
        "# Between the header and command  \r\n",
        "# At the command\r\n",
        "# At the array opening\r\n",
        "  # At the first argument  \r\n",
        "  # Between arguments\t\r\n",
        "# At the array closing\r\n",
        "# After the managed table\r\n",
    ] {
        let (_, rest) = comments
            .split_once(comment)
            .unwrap_or_else(|| panic!("missing comment {comment:?} in {remaining:?}"));
        comments = rest;
    }
    assert!(!remaining.contains("# part of executable path"));
    let parsed = read_config(&path);
    assert!(server(&parsed, "proxenos").is_none());
    assert!(server(&parsed, "iris").is_some());
    assert!(server(&parsed, "sidecar").is_some());
}

#[test]
fn config_without_final_newline_is_preserved_and_restored_byte_for_byte() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let original = "# personal configuration\nmodel = 'keep-model' # final comment";
    write_config(&path, original);

    assert_success(&run(&sb, INSTALL));
    let installed = fs::read_to_string(&path).unwrap();
    assert!(
        installed.starts_with(&format!("{original}\n")),
        "{installed}"
    );
    assert_registered(&path, proxenos_bin());
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn unowned_backup_is_preserved_and_prevents_taking_ownership() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let backup = backup_path(&sb);
    let original = "model = 'keep-model'\n";
    let foreign = "# backup created outside Proxenos\n";
    write_config(&path, original);
    write_config(&backup, foreign);

    let out = run(&sb, INSTALL);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Refusing to use existing backup"));
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert_eq!(fs::read_to_string(&backup).unwrap(), foreign);
    assert!(!sb.manifest_path().exists());
}

#[test]
fn removing_registration_after_user_deletion_cleans_up_only_owned_state() {
    let original = "model = 'original'\n";
    for edited in [None, Some("model = 'edited' # keep the user's change\n")] {
        let sb = Sandbox::new();
        let path = config_path(&sb);
        let backup = backup_path(&sb);
        write_config(&path, original);
        assert_success(&run(&sb, INSTALL));
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        if let Some(text) = edited {
            fs::write(&path, text).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        assert_success(&run(&sb, REMOVE));
        if let Some(text) = edited {
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        } else {
            assert!(!path.exists());
        }
        assert!(!backup.exists());
        assert!(!sb.manifest_path().exists());
    }
}

#[test]
fn refuses_foreign_like_for_like_and_changed_managed_registration() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    assert_success(&run(&sb, INSTALL));
    let generated = fs::read_to_string(&path).unwrap();
    assert_success(&run(&sb, REMOVE));
    write_config(&path, &generated);

    let foreign = run(&sb, INSTALL);
    assert_eq!(foreign.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("proxenos"));
    assert_eq!(fs::read_to_string(&path).unwrap(), generated);
    assert!(!sb.manifest_path().exists());
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&path).unwrap(), generated);

    fs::remove_file(&path).unwrap();
    assert_success(&run(&sb, INSTALL));
    let manifest_before = fs::read(sb.manifest_path()).unwrap();
    let mut changed = read_config(&path);
    changed["mcp_servers"]["proxenos"]["enabled"] = value(false);
    for edited in [
        changed.to_string(),
        "mcp_servers.proxenos = 42\n".to_owned(),
    ] {
        fs::write(&path, &edited).unwrap();
        for args in [INSTALL, REMOVE] {
            let out = run(&sb, args);
            assert_eq!(out.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&out.stderr).contains("modified after integrate"));
            assert_eq!(fs::read_to_string(&path).unwrap(), edited);
            assert_eq!(fs::read(sb.manifest_path()).unwrap(), manifest_before);
        }
        assert_broken_registration(&sb);
    }
}

#[test]
fn old_manifest_and_claude_only_report_survive_codex_install_and_remove() {
    let sb = Sandbox::new();
    assert_success(&run(&sb, &["integrate", "claude-code"]));
    let legacy = read_json(&sb.manifest_path());
    let claude_settings = fs::read(sb.settings_path()).unwrap();
    let old_report = run(&sb, DOCTOR);
    assert_success(&old_report);
    assert_eq!(
        non_provider_checks(&report(&old_report)),
        [
            "claude-code.registration",
            "state_dir.permissions",
            "claude-code.handshake"
        ]
    );

    assert_success(&run(&sb, INSTALL));
    let both_manifest = read_json(&sb.manifest_path());
    assert_eq!(
        both_manifest["clients"]["claude-code"],
        legacy["clients"]["claude-code"]
    );
    assert!(both_manifest["clients"]["codex"].is_object());
    assert_eq!(fs::read(sb.settings_path()).unwrap(), claude_settings);

    let with_both = run(&sb, DOCTOR);
    assert_success(&with_both);
    let both_report = report(&with_both);
    assert_eq!(
        non_provider_checks(&both_report),
        [
            "claude-code.registration",
            "codex.registration",
            "state_dir.permissions",
            "claude-code.handshake",
            "codex.handshake"
        ]
    );
    assert_eq!(
        find_check(&both_report, "claude-code.handshake")["details"]["args"],
        serde_json::json!(["mcp", "--client", "claude"])
    );
    assert_eq!(
        find_check(&both_report, "codex.handshake")["details"]["args"],
        serde_json::json!(ARGS)
    );

    assert_success(&run(&sb, REMOVE));
    assert_eq!(read_json(&sb.manifest_path()), legacy);
    assert_eq!(fs::read(sb.settings_path()).unwrap(), claude_settings);

    fs::remove_file(sb.manifest_path()).unwrap();
    let untracked = run(&sb, DOCTOR);
    assert_success(&untracked);
    assert_eq!(report(&untracked), report(&old_report));
}

#[test]
fn concurrent_client_operations_keep_both_manifest_records_consistent() {
    let sb = Sandbox::new();
    let concurrently = |claude_args: &[&str], codex_args: &[&str]| {
        let launch = |args: &[&str]| {
            command(&sb, args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let claude = launch(claude_args);
        let codex = launch(codex_args);
        let claude = claude.wait_with_output().unwrap();
        let codex = codex.wait_with_output().unwrap();
        assert_success(&claude);
        assert_success(&codex);
    };

    concurrently(&["integrate", "claude-code"], INSTALL);
    let manifest = read_json(&sb.manifest_path());
    assert!(manifest["clients"]["claude-code"].is_object());
    assert!(manifest["clients"]["codex"].is_object());
    assert_registered(&config_path(&sb), proxenos_bin());
    assert!(sb.settings_path().is_file());

    concurrently(&["integrate", "claude-code", "--remove"], REMOVE);
    assert!(!sb.manifest_path().exists());
    assert!(!sb.state_dir().exists());
    assert!(sb.state_dir().with_file_name(LOCK_FILE).is_file());
    assert!(!sb.settings_path().exists());
    assert!(!config_path(&sb).exists());
}

fn assert_parent_denies_file_creation(parent: &Path) {
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(parent.join("permission-probe"));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
}

fn exercise_both_clients_with_internal_lock(sb: &Sandbox) {
    let state = sb.state_dir();
    let parent = state.parent().unwrap();
    let parent_entries = directory_entries(parent);
    let lock = state.join(LOCK_FILE);
    let adjacent = parent.join(LOCK_FILE);
    assert!(!lock.exists());
    assert!(!adjacent.exists());

    assert_success(&run(sb, CLAUDE_INSTALL));
    assert!(lock.is_file());
    fs::write(&lock, LOCK_SENTINEL).unwrap();
    #[cfg(unix)]
    let lock_identity = || {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(&lock).unwrap();
        (metadata.dev(), metadata.ino())
    };
    #[cfg(unix)]
    let identity = lock_identity();
    let verify_lock = || {
        assert_eq!(fs::read(&lock).unwrap(), LOCK_SENTINEL);
        assert!(!adjacent.exists());
        #[cfg(unix)]
        assert_eq!(lock_identity(), identity);
    };
    assert_success(&run(sb, INSTALL));
    verify_lock();

    let moved = sb.hard_link_binary("updated registration binary");
    for args in [CLAUDE_INSTALL, INSTALL] {
        let updated = sb
            .command_for(&moved, args)
            .env_remove("CODEX_HOME")
            .output()
            .unwrap();
        assert_success(&updated);
        verify_lock();
    }
    assert_registered(&config_path(sb), &moved);
    let claude_mcp = sb.marketplace_dir().join("plugins/proxenos/.mcp.json");
    assert_eq!(
        read_json(&claude_mcp)["mcpServers"]["proxenos"]["command"],
        absolute_path_string(&moved)
    );

    assert_success(&run(sb, CLAUDE_REMOVE));
    verify_lock();
    assert_success(&run(sb, REMOVE));
    verify_lock();
    assert_eq!(directory_entries(parent), parent_entries);
    assert_eq!(
        directory_entries(&state),
        [std::ffi::OsString::from(LOCK_FILE)]
    );
    assert!(!config_path(sb).exists());
    assert!(!sb.settings_path().exists());
}

#[cfg(unix)]
#[test]
fn both_clients_use_the_same_lock_inside_state_with_read_only_parent() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    struct RestorePermissions<'a>(&'a Path, fs::Permissions);

    impl Drop for RestorePermissions<'_> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0, self.1.clone());
        }
    }

    let sb = Sandbox::new();
    let state = sb.state_dir();
    let parent = state.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
    let restore = RestorePermissions(parent, fs::metadata(parent).unwrap().permissions());
    fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert_parent_denies_file_creation(parent);
    exercise_both_clients_with_internal_lock(&sb);

    // Making the parent writable again must not switch away from the existing lock.
    drop(restore);
    assert_success(&run(&sb, INSTALL));
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read(state.join(LOCK_FILE)).unwrap(), LOCK_SENTINEL);
    assert!(!parent.join(LOCK_FILE).exists());
}

#[cfg(windows)]
#[test]
fn both_clients_use_internal_lock_when_parent_acl_denies_file_creation() {
    struct RemoveDenyRule<'a> {
        parent: &'a Path,
        sid: &'a str,
    }

    impl Drop for RemoveDenyRule<'_> {
        fn drop(&mut self) {
            let restored = Command::new("icacls.exe")
                .arg(self.parent)
                .args(["/remove:d", self.sid])
                .output();
            if !std::thread::panicking() {
                assert_success(&restored.expect("remove the test-only parent ACL rule"));
            }
        }
    }

    let sb = Sandbox::new();
    let state = sb.state_dir();
    let parent = state.parent().unwrap();
    fs::create_dir(&state).unwrap();
    let user = Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert_success(&user);
    let user = String::from_utf8_lossy(&user.stdout);
    let sid = user
        .split('"')
        .find(|field| field.starts_with("S-1-"))
        .unwrap();
    let sid = format!("*{sid}");
    let _restore = RemoveDenyRule { parent, sid: &sid };
    // WD denies creating files in this directory. Without inheritance flags, the existing
    // state directory stays writable and no other path is affected.
    let denied = Command::new("icacls.exe")
        .arg(parent)
        .arg("/deny")
        .arg(format!("{sid}:(WD)"))
        .output()
        .unwrap();
    assert_success(&denied);
    assert_parent_denies_file_creation(parent);
    exercise_both_clients_with_internal_lock(&sb);
}

#[test]
fn shared_settings_directory_survives_until_both_integrations_are_removed() {
    for installs in [[CLAUDE_INSTALL, INSTALL], [INSTALL, CLAUDE_INSTALL]] {
        for claude_removed_first in [true, false] {
            let sb = Sandbox::new();
            let codex_config = config_path(&sb);
            let directory = codex_config.parent().unwrap();
            let claude_config = directory.join("settings.json");
            let invoke = |args: &[&str]| {
                command(&sb, args)
                    .env("CLAUDE_CONFIG_DIR", directory)
                    .output()
                    .unwrap()
            };
            assert!(!directory.exists());
            for args in installs {
                assert_success(&invoke(args));
            }
            assert_registered(&codex_config, proxenos_bin());
            assert!(claude_config.is_file());

            let (first_removal, last_removal) = if claude_removed_first {
                (CLAUDE_REMOVE, REMOVE)
            } else {
                (REMOVE, CLAUDE_REMOVE)
            };
            assert_success(&invoke(first_removal));
            assert!(directory.is_dir());
            assert_eq!(codex_config.is_file(), claude_removed_first);
            assert_eq!(claude_config.is_file(), !claude_removed_first);
            let out = invoke(DOCTOR);
            assert_success(&out);
            let client = if claude_removed_first {
                "codex"
            } else {
                "claude-code"
            };
            assert_eq!(
                find_check(&report(&out), &format!("{client}.registration"))["status"],
                "ok"
            );

            assert_success(&invoke(last_removal));
            assert!(!directory.exists());
            assert!(!sb.manifest_path().exists());
        }
    }
}

#[test]
fn shared_backup_directory_is_removed_after_the_last_client() {
    for installs in [[CLAUDE_INSTALL, INSTALL], [INSTALL, CLAUDE_INSTALL]] {
        let sb = Sandbox::new();
        let codex = config_path(&sb);
        let claude = sb.settings_path();
        let codex_original = "# user configuration\nmodel = 'keep'\n";
        let claude_original = "{\n  \"theme\": \"dark\"\n}\n";
        write_config(&codex, codex_original);
        write_config(&claude, claude_original);
        for args in installs {
            assert_success(&run(&sb, args));
        }
        assert_eq!(
            fs::read_to_string(backup_path(&sb)).unwrap(),
            codex_original
        );
        assert_eq!(
            fs::read_to_string(sb.backup_path()).unwrap(),
            claude_original
        );

        let (first, last, remaining_backup) = if installs[0] == CLAUDE_INSTALL {
            (CLAUDE_REMOVE, REMOVE, backup_path(&sb))
        } else {
            (REMOVE, CLAUDE_REMOVE, sb.backup_path())
        };
        assert_success(&run(&sb, first));
        assert!(remaining_backup.is_file());
        assert_success(&run(&sb, last));
        assert_eq!(fs::read_to_string(&codex).unwrap(), codex_original);
        assert_eq!(fs::read_to_string(&claude).unwrap(), claude_original);
        assert!(!sb.state_dir().join("integrate").join("backups").exists());
        assert!(!sb.state_dir().exists());
    }
}

fn assert_broken_registration(sb: &Sandbox) {
    let out = run(sb, DOCTOR);
    assert_eq!(out.status.code(), Some(1));
    let broken = report(&out);
    assert_eq!(find_check(&broken, "codex.registration")["status"], "fail");
    let handshake = find_check(&broken, "codex.handshake");
    assert_eq!(handshake["status"], "fail");
    assert!(
        handshake["message"]
            .as_str()
            .unwrap()
            .contains("registration")
    );
}

#[test]
fn doctor_identifies_missing_malformed_and_changed_config_without_launching() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    assert_success(&run(&sb, INSTALL));
    let valid = fs::read_to_string(&path).unwrap();
    let manifest = fs::read(sb.manifest_path()).unwrap();
    fs::remove_file(&path).unwrap();
    assert_broken_registration(&sb);
    fs::create_dir(&path).unwrap();
    assert_broken_registration(&sb);
    fs::remove_dir(&path).unwrap();
    for invalid in [
        &b"[unclosed\n"[..],
        &b"mcp_servers = false\n"[..],
        &b"\xff"[..],
    ] {
        fs::write(&path, invalid).unwrap();
        assert_broken_registration(&sb);
        for args in [INSTALL, REMOVE] {
            assert_eq!(run(&sb, args).status.code(), Some(1));
            assert_eq!(fs::read(&path).unwrap(), invalid);
            assert_eq!(fs::read(sb.manifest_path()).unwrap(), manifest);
        }
    }

    let mut changed: DocumentMut = valid.parse().unwrap();
    changed["mcp_servers"]["proxenos"]["args"] =
        "['mcp', '--client', 'claude']".parse::<Item>().unwrap();
    fs::write(&path, changed.to_string()).unwrap();
    assert_broken_registration(&sb);

    fs::write(&path, valid).unwrap();
    fs::write(sb.manifest_path(), "{broken").unwrap();
    let out = run(&sb, DOCTOR);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        find_check(&report(&out), "integration.manifest")["status"],
        "fail"
    );
}

#[test]
fn tampered_manifest_cannot_take_ownership_of_changed_configuration() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    assert_success(&run(&sb, INSTALL));
    let mut doc = read_config(&path);
    doc["mcp_servers"]["proxenos"]["args"] =
        "['mcp', '--client', 'claude']".parse::<Item>().unwrap();
    let changed = doc.to_string();
    fs::write(&path, &changed).unwrap();
    let mut manifest = read_json(&sb.manifest_path());
    manifest["clients"]["codex"]["entries"]["args"] =
        serde_json::json!(["mcp", "--client", "claude"]);
    let manifest = serde_json::to_string_pretty(&manifest).unwrap();
    fs::write(sb.manifest_path(), &manifest).unwrap();

    assert_broken_registration(&sb);
    for args in [INSTALL, REMOVE] {
        assert_eq!(run(&sb, args).status.code(), Some(1));
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        assert_eq!(fs::read_to_string(sb.manifest_path()).unwrap(), manifest);
    }
}

#[test]
fn malformed_client_records_refuse_install_and_remove_without_modifications() {
    let sb = Sandbox::new();
    assert_success(&run(&sb, &["integrate", "claude-code"]));
    assert_success(&run(&sb, INSTALL));
    let path = config_path(&sb);
    let codex_settings = fs::read(&path).unwrap();
    let claude_settings = fs::read(sb.settings_path()).unwrap();
    let valid_manifest = read_json(&sb.manifest_path());

    for (client, label) in [("codex", "Codex"), ("claude-code", "claude-code")] {
        let mut malformed = valid_manifest.clone();
        malformed["clients"][client]["settings_path"] = Value::Bool(false);
        let malformed = serde_json::to_vec(&malformed).unwrap();
        fs::write(sb.manifest_path(), &malformed).unwrap();
        for args in [
            &["integrate", client][..],
            &["integrate", client, "--remove"][..],
        ] {
            let out = run(&sb, args);
            assert_eq!(out.status.code(), Some(1));
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains(&format!("Could not parse {label} integration record")),
                "{stderr}"
            );
            assert_eq!(fs::read(&path).unwrap(), codex_settings);
            assert_eq!(fs::read(sb.settings_path()).unwrap(), claude_settings);
            assert_eq!(fs::read(sb.manifest_path()).unwrap(), malformed);
        }
    }
}

#[test]
fn codex_home_is_resolved_and_must_still_name_the_registered_config() {
    let sb = Sandbox::new();
    let alternate = sb.root.join("alternate Codex home");
    let target = alternate.join("config.toml");
    let install = || {
        command(&sb, INSTALL)
            .current_dir(&sb.root)
            .env("CODEX_HOME", "alternate Codex home")
            .output()
            .unwrap()
    };

    let missing = install();
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("CODEX_HOME"));
    assert!(!sb.manifest_path().exists());
    fs::write(&alternate, "not a directory").unwrap();
    assert_eq!(install().status.code(), Some(1));
    fs::remove_file(&alternate).unwrap();
    fs::create_dir(&alternate).unwrap();

    assert_success(&install());
    assert_registered(&target, proxenos_bin());
    assert!(!config_path(&sb).exists());
    let with_override = command(&sb, DOCTOR)
        .env("CODEX_HOME", &alternate)
        .output()
        .unwrap();
    assert_success(&with_override);
    assert_broken_registration(&sb);
    for args in [INSTALL, REMOVE] {
        let out = run(&sb, args);
        assert_eq!(out.status.code(), Some(1));
        assert!(!config_path(&sb).exists());
        assert_registered(&target, proxenos_bin());
    }
    let remove = command(&sb, REMOVE)
        .env("CODEX_HOME", &alternate)
        .output()
        .unwrap();
    assert_success(&remove);
    assert!(alternate.is_dir());
    assert!(!target.exists());
}

/// A Windows host without symlink privilege cannot exercise directory links.
fn try_directory_symlink(target: &Path, link: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map(|()| true)
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(1314) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn unlink_directory_symlink(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return fs::remove_file(path);
    #[cfg(windows)]
    return fs::remove_dir(path);
}

#[test]
fn default_profile_can_be_created_and_reused_under_aliased_home() {
    let sb = Sandbox::new();
    let home_alias = sb.root.join("home-alias");
    if !try_directory_symlink(&sb.home, &home_alias).unwrap() {
        return;
    }
    let invoke = |args: &[&str]| {
        command(&sb, args)
            .env("HOME", &home_alias)
            .env("USERPROFILE", &home_alias)
            .output()
            .unwrap()
    };
    let config = config_path(&sb);
    assert!(!config.parent().unwrap().exists());
    assert_success(&invoke(INSTALL));
    assert_registered(&config, proxenos_bin());
    let original_record = read_json(&sb.manifest_path());
    assert_success(&invoke(INSTALL));
    assert_eq!(read_json(&sb.manifest_path()), original_record);
    assert_success(&invoke(DOCTOR));
    assert_success(&invoke(REMOVE));
    assert!(!config.parent().unwrap().exists());
    assert!(fs::symlink_metadata(&home_alias).unwrap().is_symlink());
}

#[test]
fn retargeted_default_codex_directory_is_refused_even_with_identical_config() {
    let sb = Sandbox::new();
    let codex_config = config_path(&sb);
    let link = codex_config.parent().unwrap();
    let original_target = sb.root.join("original codex profile");
    let new_target = sb.root.join("different codex profile");
    let original_config = original_target.join("config.toml");
    let new_config = new_target.join("config.toml");
    let original = "# original user configuration\nmodel = 'keep'\n";
    write_config(&original_config, original);
    fs::create_dir(&new_target).unwrap();
    if !try_directory_symlink(&original_target, link).unwrap() {
        return;
    }

    assert_success(&run(&sb, INSTALL));
    assert_registered(&original_config, proxenos_bin());
    assert_success(&run(&sb, DOCTOR));
    let registered = fs::read(&original_config).unwrap();
    let manifest = fs::read(sb.manifest_path()).unwrap();
    fs::write(&new_config, &registered).unwrap();
    unlink_directory_symlink(link).unwrap();
    assert!(try_directory_symlink(&new_target, link).unwrap());

    for args in [INSTALL, REMOVE] {
        assert_eq!(run(&sb, args).status.code(), Some(1));
    }
    assert_broken_registration(&sb);
    assert_eq!(fs::read(&original_config).unwrap(), registered);
    assert_eq!(fs::read(&new_config).unwrap(), registered);
    assert_eq!(fs::read(sb.manifest_path()).unwrap(), manifest);

    unlink_directory_symlink(link).unwrap();
    assert!(try_directory_symlink(&original_target, link).unwrap());
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&original_config).unwrap(), original);
    assert_eq!(fs::read(&new_config).unwrap(), registered);
    assert!(fs::symlink_metadata(link).unwrap().is_symlink());
}

#[test]
fn codex_config_symlink_can_be_removed_only_with_original_target() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let target = sb.root.join("codex-config.toml");
    let other = sb.root.join("other-codex-config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "# user-owned link target\nmodel = 'keep'\n";
    fs::write(&target, original).unwrap();
    fs::write(&other, "model = 'other'\n").unwrap();
    if !try_file_symlink(&target, &path).unwrap() {
        return;
    }

    assert_success(&run(&sb, INSTALL));
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert_registered(&target, proxenos_bin());
    fs::remove_file(&path).unwrap();
    let registered = fs::read(&target).unwrap();
    let manifest = fs::read(sb.manifest_path()).unwrap();
    for args in [INSTALL, REMOVE] {
        let out = run(&sb, args);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("settings symlink is missing"));
        assert!(!path.exists());
        assert_eq!(fs::read(&target).unwrap(), registered);
        assert_eq!(fs::read(sb.manifest_path()).unwrap(), manifest);
    }
    assert_broken_registration(&sb);

    assert!(try_file_symlink(&other, &path).unwrap());
    assert_eq!(run(&sb, REMOVE).status.code(), Some(1));
    assert_eq!(fs::read_to_string(&other).unwrap(), "model = 'other'\n");
    assert_broken_registration(&sb);

    fs::remove_file(&path).unwrap();
    assert!(try_file_symlink(&target, &path).unwrap());
    assert_success(&run(&sb, REMOVE));
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), original);
}

#[test]
fn integration_refuses_special_file_at_shared_lock_path() {
    let sb = Sandbox::new();
    let lock = sb.state_dir().with_file_name(LOCK_FILE);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    #[cfg(unix)]
    let diagnostic = {
        assert_success(&Command::new("mkfifo").arg(&lock).output().unwrap());
        "integration lock is not a regular file"
    };
    #[cfg(windows)]
    let (diagnostic, link_target) = {
        let target = sb.root.join("user-owned file");
        fs::write(&target, "leave this alone").unwrap();
        if !try_file_symlink(&target, &lock).unwrap() {
            return;
        }
        (
            "integration lock is not a regular file or is a reparse point",
            target,
        )
    };

    let out = run(&sb, INSTALL);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(diagnostic),
        "{out:?}"
    );
    assert!(!config_path(&sb).exists());
    assert!(!sb.manifest_path().exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        assert!(fs::symlink_metadata(&lock).unwrap().file_type().is_fifo());
    }
    #[cfg(windows)]
    {
        assert!(fs::symlink_metadata(&lock).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(&link_target).unwrap(),
            "leave this alone"
        );
    }
}

#[cfg(feature = "test-tools")]
#[test]
fn pending_codex_install_can_be_completed_or_removed() {
    for retry in [true, false] {
        let sb = Sandbox::new();
        let path = config_path(&sb);
        let original = "# original\nmodel = 'keep'\n";
        write_config(&path, original);
        let failed = command(&sb, INSTALL)
            .env("PROXENOS_TEST_FAIL_FINAL_MANIFEST", "1")
            .output()
            .unwrap();
        assert_eq!(failed.status.code(), Some(1));
        assert_eq!(
            read_json(&sb.manifest_path())["clients"]["codex"]["pending"],
            true
        );
        assert_registered(&path, proxenos_bin());
        assert_broken_registration(&sb);

        if retry {
            assert_success(&run(&sb, INSTALL));
            assert_ne!(
                read_json(&sb.manifest_path())["clients"]["codex"]["pending"],
                true
            );
            assert_success(&run(&sb, DOCTOR));
        }
        assert_success(&run(&sb, REMOVE));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!sb.manifest_path().exists());
    }
}

#[cfg(feature = "test-tools")]
#[test]
fn pending_install_takes_the_original_backup_on_retry() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let backup = backup_path(&sb);
    let original = "# keep this user config\nmodel = 'keep'\n";
    write_config(&path, original);
    let failed = command(&sb, INSTALL)
        .env("PROXENOS_TEST_FAIL_FINAL_MANIFEST", "1")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        read_json(&sb.manifest_path())["clients"]["codex"]["pending"],
        true
    );
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);

    // Recreate the state after recording intent and before writing the backup or config.
    fs::write(&path, original).unwrap();
    fs::remove_file(&backup).unwrap();
    assert_success(&run(&sb, INSTALL));
    assert_registered(&path, proxenos_bin());
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    assert_success(&run(&sb, REMOVE));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(!backup.exists());
    assert!(!sb.manifest_path().exists());
}

#[cfg(feature = "test-tools")]
#[test]
fn pending_binary_relocation_recovers_before_the_new_command_was_written() {
    let sb = Sandbox::new();
    let path = config_path(&sb);
    let moved = sb.hard_link_binary("new proxenos ü");
    assert_success(&run(&sb, INSTALL));
    let original = fs::read(&path).unwrap();
    let relocate = || {
        let mut cmd = sb.command_for(&moved, INSTALL);
        cmd.env_remove("CODEX_HOME");
        cmd
    };
    let failed = relocate()
        .env("PROXENOS_TEST_FAIL_FINAL_MANIFEST", "1")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_registered(&path, &moved);

    // Restore the on-disk state that a crash after recording the pending manifest and before
    // updating config would have left. The original command is still owned by this operation.
    fs::write(&path, original).unwrap();
    assert_success(&relocate().output().unwrap());
    assert_registered(&path, &moved);
    assert_success(&run(&sb, DOCTOR));
    assert_success(&run(&sb, REMOVE));
    assert!(!path.exists());
}

#[cfg(feature = "test-tools")]
#[test]
fn codex_handshake_excludes_its_own_provider_but_doctor_probes_it() {
    let sb = Sandbox::new();
    let bin_dir = sb.root.join("stub-bin");
    fs::create_dir(&bin_dir).unwrap();
    let stub = env!("CARGO_BIN_EXE_proxenos-stub");
    for name in ["codex", "agy"] {
        fs::hard_link(
            stub,
            bin_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
    }
    assert_success(&run(&sb, INSTALL));

    let out = command(&sb, DOCTOR)
        .env("PATH", &bin_dir)
        .env("PROXENOS_STUB_DIR", &sb.root)
        .output()
        .unwrap();
    assert_success(&out);
    let report = report(&out);
    assert_eq!(find_check(&report, "cli.codex")["status"], "ok");
    let handshake = find_check(&report, "codex.handshake");
    assert_eq!(handshake["status"], "ok");
    let tools = handshake["details"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool == "antigravity"));
    assert!(
        tools
            .iter()
            .all(|tool| tool != "codex" && tool != "codex-reply")
    );
}
