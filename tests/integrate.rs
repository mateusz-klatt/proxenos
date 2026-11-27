//! Behavioural tests for `proxenos integrate claude-code [--remove]` and `proxenos doctor` (D10).

#[path = "integrate/codex.rs"]
mod codex;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use uuid::Uuid;

fn proxenos_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_proxenos"))
}

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    local_app_data: PathBuf,
    absent_path: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        Self::with_name(&format!("proxenos-int-{}", Uuid::new_v4()))
    }

    fn with_name(name: &str) -> Self {
        let base = proxenos_bin()
            .parent()
            .expect("binary has parent dir")
            .join(name);
        let home = base.join("home");
        let local_app_data = base.join("localappdata");
        let absent_path = base.join("absent-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&local_app_data).unwrap();
        Self {
            root: base,
            home,
            local_app_data,
            absent_path,
        }
    }

    fn settings_path(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }

    fn state_dir(&self) -> PathBuf {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.home.join(".local").join("state").join("proxenos")
        }
        #[cfg(target_os = "macos")]
        {
            self.home
                .join("Library")
                .join("Application Support")
                .join("proxenos")
        }
        #[cfg(windows)]
        {
            self.local_app_data.join("proxenos")
        }
    }

    fn marketplace_dir(&self) -> PathBuf {
        self.state_dir().join("claude-marketplace")
    }

    fn manifest_path(&self) -> PathBuf {
        self.state_dir().join("integrate").join("manifest.json")
    }

    fn backup_path(&self) -> PathBuf {
        self.state_dir()
            .join("integrate")
            .join("backups")
            .join("claude-code-settings.json")
    }

    fn command_for(&self, bin: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(bin);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("PROXENOS_")
                || name.starts_with("CLAUDE_DELEGATOR_")
                || name == "CODEX_DELEGATOR_CODEX_BIN"
            {
                command.env_remove(key);
            }
        }
        command
            .env_remove("XDG_STATE_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", &self.local_app_data)
            .env("APPDATA", &self.local_app_data)
            .env("PATH", &self.absent_path)
            .env("TOKIO_WORKER_THREADS", "2")
            .args(args);
        command
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_for(proxenos_bin(), args)
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("proxenos binary runs")
    }

    fn hard_link_binary(&self, rel_dir: &str) -> PathBuf {
        let dir = self.root.join(rel_dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(format!("proxenos{}", std::env::consts::EXE_SUFFIX));
        fs::hard_link(proxenos_bin(), &dest).unwrap_or_else(|error| {
            panic!(
                "hard-linking {} to {} failed: {error}",
                proxenos_bin().display(),
                dest.display()
            )
        });
        dest
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn strip_windows_verbatim(path: PathBuf) -> String {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return rest.to_owned();
        }
    }
    path.to_str().unwrap().to_owned()
}

fn absolute_path_string(path: &Path) -> String {
    strip_windows_verbatim(std::path::absolute(path).unwrap())
}

fn canonical_path_string(path: &Path) -> String {
    strip_windows_verbatim(fs::canonicalize(path).unwrap())
}

/// Creates a file symlink. Returns `Ok(false)` on Windows when the OS denies
/// `SeCreateSymbolicLinkPrivilege` (error 1314) on an unprivileged host.
fn try_file_symlink(target: &Path, link: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map(|()| true)
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => Ok(true),
            Err(err) if err.raw_os_error() == Some(1314) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed reading {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("invalid JSON in {}: {err}\n{text}", path.display()))
}

fn find_check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing check {name} in report: {report}"))
}

#[test]
fn fresh_integrate_creates_marketplace_settings_and_manifest() {
    let sb = Sandbox::new();
    let out = sb.run(&["integrate", "claude-code"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Integrated Proxenos with Claude Code."),
        "{stdout}"
    );
    assert!(out.stderr.is_empty());

    let expected_bin = absolute_path_string(proxenos_bin());
    let expected_marketplace = sb.marketplace_dir().to_str().unwrap().to_owned();

    let marketplace = read_json(
        &sb.marketplace_dir()
            .join(".claude-plugin")
            .join("marketplace.json"),
    );
    assert_eq!(marketplace["name"], "proxenos");
    assert_eq!(marketplace["owner"]["name"], "Proxenos");
    assert_eq!(marketplace["plugins"][0]["name"], "proxenos");
    assert_eq!(marketplace["plugins"][0]["source"], "./plugins/proxenos");
    assert!(marketplace["plugins"][0]["description"].is_string());

    let plugin = read_json(
        &sb.marketplace_dir()
            .join("plugins")
            .join("proxenos")
            .join(".claude-plugin")
            .join("plugin.json"),
    );
    assert_eq!(plugin["name"], "proxenos");
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(plugin["author"]["name"], "Proxenos");
    assert!(plugin["description"].is_string());

    let mcp = read_json(
        &sb.marketplace_dir()
            .join("plugins")
            .join("proxenos")
            .join(".mcp.json"),
    );
    assert_eq!(mcp["mcpServers"]["proxenos"]["command"], expected_bin);
    assert_eq!(
        mcp["mcpServers"]["proxenos"]["args"],
        json!(["mcp", "--client", "claude"])
    );

    let settings = read_json(&sb.settings_path());
    assert_eq!(
        settings["extraKnownMarketplaces"]["proxenos"],
        json!({
            "source": {
                "source": "directory",
                "path": expected_marketplace
            }
        })
    );
    assert_eq!(settings["enabledPlugins"]["proxenos@proxenos"], true);

    let manifest = read_json(&sb.manifest_path());
    assert_eq!(manifest["schema_version"], 1);
    let rec = &manifest["clients"]["claude-code"];
    assert_eq!(rec["client"], "claude-code");
    assert_eq!(rec["binary_path"], expected_bin);
    assert_eq!(rec["marketplace_dir"], expected_marketplace);
    assert_eq!(rec["settings_created"], true);
    assert!(rec.get("backup_path").is_none());
    assert_eq!(rec["files"].as_array().unwrap().len(), 3);
    assert!(!sb.backup_path().exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let state_mode = fs::metadata(sb.state_dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(state_mode, 0o700);
        for file in [
            sb.settings_path(),
            sb.manifest_path(),
            sb.marketplace_dir()
                .join(".claude-plugin")
                .join("marketplace.json"),
            sb.marketplace_dir()
                .join("plugins")
                .join("proxenos")
                .join(".mcp.json"),
        ] {
            let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "unexpected mode on {}", file.display());
        }
    }
}

#[test]
fn idempotent_rerun_keeps_registration_and_original_backup() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    let original = "{\n  \"theme\": \"dark\"\n}\n";
    fs::write(sb.settings_path(), original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(sb.settings_path(), fs::Permissions::from_mode(0o644)).unwrap();
    }

    let first = sb.run(&["integrate", "claude-code"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let settings_mode = fs::metadata(sb.settings_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(settings_mode, 0o644);
        let backup_mode = fs::metadata(sb.backup_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(backup_mode, 0o600);
    }

    let after_first = fs::read_to_string(sb.settings_path()).unwrap();
    // Deleting a generated plugin subdir and re-running heals it without duplicating created_dirs.
    let plugin_meta_dir = sb
        .marketplace_dir()
        .join("plugins")
        .join("proxenos")
        .join(".claude-plugin");
    fs::remove_dir_all(&plugin_meta_dir).unwrap();
    let dirs_before = read_json(&sb.manifest_path())["clients"]["claude-code"]["created_dirs"]
        .as_array()
        .unwrap()
        .len();

    let second = sb.run(&["integrate", "claude-code"]);
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("up to date"), "{stdout}");
    assert!(plugin_meta_dir.join("plugin.json").is_file());
    assert_eq!(
        read_json(&sb.manifest_path())["clients"]["claude-code"]["created_dirs"]
            .as_array()
            .unwrap()
            .len(),
        dirs_before
    );
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), after_first);
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);

    // Editing an unrelated key and re-running keeps the original backup.
    let mut edited = read_json(&sb.settings_path());
    edited["theme"] = json!("light");
    fs::write(
        sb.settings_path(),
        format!("{}\n", serde_json::to_string_pretty(&edited).unwrap()),
    )
    .unwrap();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);

    // Even if a backup file already exists from a previous integrate that was never removed
    // (manifest missing and keys cleared), a fresh integrate must keep that first backup.
    fs::remove_file(sb.manifest_path()).unwrap();
    fs::remove_dir_all(sb.marketplace_dir()).unwrap();
    let modified_unrelated = "{\n  \"theme\": \"light\"\n}\n";
    fs::write(sb.settings_path(), modified_unrelated).unwrap();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);

    // After a full --remove the backup is deleted, so a subsequent integrate takes a fresh one.
    assert!(
        sb.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    assert!(!sb.backup_path().exists());
    let fresh_settings = "{\n  \"theme\": \"solarized\"\n}\n";
    fs::write(sb.settings_path(), fresh_settings).unwrap();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    assert_eq!(
        fs::read_to_string(sb.backup_path()).unwrap(),
        fresh_settings
    );
}

#[test]
fn binary_path_change_updates_mcp_json_and_manifest() {
    let sb = Sandbox::new();
    let bin_a = sb.hard_link_binary("bin-a");
    let bin_b = sb.hard_link_binary("bin-b");

    let first = sb
        .command_for(&bin_a, &["integrate", "claude-code"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let mcp_path = sb
        .marketplace_dir()
        .join("plugins")
        .join("proxenos")
        .join(".mcp.json");
    assert_eq!(
        read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
        absolute_path_string(&bin_a)
    );

    let second = sb
        .command_for(&bin_b, &["integrate", "claude-code"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("Updated Proxenos"), "{stdout}");
    assert_eq!(
        read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
        absolute_path_string(&bin_b)
    );
    assert_eq!(
        read_json(&sb.manifest_path())["clients"]["claude-code"]["binary_path"],
        absolute_path_string(&bin_b)
    );
}

#[test]
fn refuses_foreign_settings_entries_without_modifying_anything() {
    let cases = [
        (
            "extraKnownMarketplaces.proxenos",
            json!({
                "extraKnownMarketplaces": {
                    "proxenos": {
                        "source": { "source": "directory", "path": "/foreign/marketplace" }
                    }
                }
            }),
        ),
        (
            "enabledPlugins.proxenos@proxenos",
            json!({
                "enabledPlugins": {
                    "proxenos@proxenos": true
                }
            }),
        ),
    ];

    for (expected_mention, content) in cases {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
        let raw = format!("{}\n", serde_json::to_string_pretty(&content).unwrap());
        fs::write(sb.settings_path(), &raw).unwrap();

        let out = sb.run(&["integrate", "claude-code"]);
        assert_eq!(out.status.code(), Some(1), "case: {expected_mention}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Refusing") && stderr.contains(expected_mention),
            "stderr: {stderr}"
        );
        assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), raw);
        assert!(!sb.marketplace_dir().exists());
        assert!(!sb.manifest_path().exists());
    }
}

#[test]
fn refuses_foreign_marketplace_directory_without_modifying_anything() {
    let sb = Sandbox::new();
    let foreign = sb.marketplace_dir();
    fs::create_dir_all(&foreign).unwrap();
    let marker = foreign.join("custom.txt");
    fs::write(&marker, "foreign content").unwrap();

    let out = sb.run(&["integrate", "claude-code"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("marketplace directory"), "{stderr}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "foreign content");
    assert!(!sb.settings_path().exists());
    assert!(!sb.manifest_path().exists());
}

#[test]
fn refuses_modified_entries_on_rerun_and_invalid_config_shapes() {
    // Modified entry on rerun.
    let sb = Sandbox::new();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    let mut settings = read_json(&sb.settings_path());
    settings["extraKnownMarketplaces"]["proxenos"]["source"]["path"] = json!("/user/override");
    let modified = serde_json::to_string_pretty(&settings).unwrap();
    fs::write(sb.settings_path(), &modified).unwrap();

    let out = sb.run(&["integrate", "claude-code"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("modified after integrate"));
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), modified);

    // Invalid JSON, non-object root, non-object section, directory as settings.json,
    // corrupt manifest, unsupported manifest schema version.
    let bad_settings: &[(&str, &str)] = &[
        ("not-json", "Could not parse"),
        ("[]", "must be a JSON object"),
        (
            r#"{"extraKnownMarketplaces": "wrong"}"#,
            "must be a JSON object",
        ),
    ];
    for (body, needle) in bad_settings {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
        fs::write(sb.settings_path(), body).unwrap();
        let out = sb.run(&["integrate", "claude-code"]);
        assert_eq!(out.status.code(), Some(1), "body: {body}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(needle),
            "body: {body}, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // settings.json is a directory (exercises non-NotFound read I/O error).
    let sb_dir = Sandbox::new();
    fs::create_dir_all(sb_dir.settings_path()).unwrap();
    let out = sb_dir.run(&["integrate", "claude-code"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&sb_dir.settings_path().display().to_string())
    );

    // Corrupt manifest JSON and unsupported schema version.
    for (manifest_body, needle) in [
        ("{broken", "Could not parse integration manifest"),
        (
            r#"{"schema_version": 99, "clients": {}}"#,
            "Unsupported integration manifest schema version 99",
        ),
    ] {
        let sb_m = Sandbox::new();
        fs::create_dir_all(sb_m.manifest_path().parent().unwrap()).unwrap();
        fs::write(sb_m.manifest_path(), manifest_body).unwrap();
        let out = sb_m.run(&["integrate", "claude-code"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains(needle));
    }
}

#[test]
fn remove_cleans_up_fresh_install_and_second_remove_is_idempotent() {
    let sb = Sandbox::new();
    let never_installed = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(never_installed.status.success());
    assert!(String::from_utf8_lossy(&never_installed.stdout).contains("nothing to remove"));
    assert!(!sb.state_dir().exists());
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let first = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Removed Proxenos"));
    assert!(!sb.settings_path().exists());
    assert!(!sb.home.join(".claude").exists());
    assert!(!sb.marketplace_dir().exists());
    assert!(!sb.manifest_path().exists());
    assert!(!sb.state_dir().exists());

    let second = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("nothing to remove"));

    // Remove also cleans up marketplace and manifest cleanly when settings.json was already deleted.
    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    fs::remove_file(sb.settings_path()).unwrap();
    let without_settings = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        without_settings.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&without_settings.stderr)
    );
    assert!(!sb.marketplace_dir().exists());
    assert!(!sb.manifest_path().exists());
    assert!(!sb.state_dir().exists());
}

#[test]
fn remove_preserves_non_empty_dirs_other_manifest_clients_and_missing_keys() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), "{\"keep\": true}\n").unwrap();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    // Place a user file in marketplace_dir so it is not empty.
    let user_file = sb.marketplace_dir().join("user-note.txt");
    fs::write(&user_file, "keep me").unwrap();

    // Add another client record in manifest.json to verify remove touches only claude-code.
    let mut manifest = read_json(&sb.manifest_path());
    manifest["clients"]["other-client"] = json!({
        "client": "other-client",
        "marketplace_dir": "/other",
        "binary_path": "/other/bin",
        "settings_path": "/other/settings.json",
        "settings_created": false,
        "created_extra_known_marketplaces": false,
        "created_enabled_plugins": false,
        "entries": {
            "extra_known_marketplace": {},
            "enabled_plugin": true
        },
        "files": [],
        "created_dirs": []
    });
    fs::write(
        sb.manifest_path(),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Manually remove one section and one key from settings.json before --remove,
    // and add a new unrelated key that must survive (backup must not be restored over it).
    let mut settings = read_json(&sb.settings_path());
    settings
        .as_object_mut()
        .unwrap()
        .shift_remove("enabledPlugins");
    settings["extraKnownMarketplaces"]
        .as_object_mut()
        .unwrap()
        .shift_remove("proxenos");
    settings["addedLater"] = json!("survived");
    fs::write(
        sb.settings_path(),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let out = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        user_file.exists(),
        "non-empty dir and user file must remain"
    );
    assert!(
        !sb.marketplace_dir()
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
    );
    assert!(!sb.backup_path().exists());

    let remaining_manifest = read_json(&sb.manifest_path());
    assert!(remaining_manifest["clients"].get("claude-code").is_none());
    assert!(remaining_manifest["clients"].get("other-client").is_some());

    let remaining_settings = read_json(&sb.settings_path());
    assert_eq!(remaining_settings["keep"], true);
    assert_eq!(remaining_settings["addedLater"], "survived");
}

#[test]
fn remove_refuses_user_modified_entries_and_reports_them() {
    // Case 1: user points extraKnownMarketplaces.proxenos elsewhere.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), "{\"userKey\": 42}\n").unwrap();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let mut settings = read_json(&sb.settings_path());
    let custom_entry = json!({
        "source": {
            "source": "directory",
            "path": "/custom/proxenos-fork"
        }
    });
    settings["extraKnownMarketplaces"]["proxenos"] = custom_entry.clone();
    fs::write(
        sb.settings_path(),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let out = sb.run(&["integrate", "claude-code", "--remove"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("extraKnownMarketplaces.proxenos") && stderr.contains("modified"),
        "stderr: {stderr}"
    );
    let after = read_json(&sb.settings_path());
    assert_eq!(after["extraKnownMarketplaces"]["proxenos"], custom_entry);
    assert_eq!(after["userKey"], 42);
    assert!(
        after.get("enabledPlugins").is_none(),
        "unmodified enabledPlugins entry should have been removed: {after}"
    );
    assert!(!sb.marketplace_dir().exists());

    // Case 2: user modifies enabledPlugins."proxenos@proxenos" to false.
    let sb2 = Sandbox::new();
    assert!(sb2.run(&["integrate", "claude-code"]).status.success());
    let mut settings2 = read_json(&sb2.settings_path());
    settings2["enabledPlugins"]["proxenos@proxenos"] = json!(false);
    fs::write(
        sb2.settings_path(),
        serde_json::to_string_pretty(&settings2).unwrap(),
    )
    .unwrap();

    let out2 = sb2.run(&["integrate", "claude-code", "--remove"]);
    assert_eq!(out2.status.code(), Some(1));
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr2.contains("enabledPlugins.proxenos@proxenos") && stderr2.contains("modified"),
        "stderr: {stderr2}"
    );
    let after2 = read_json(&sb2.settings_path());
    assert_eq!(after2["enabledPlugins"]["proxenos@proxenos"], false);
    assert!(after2.get("extraKnownMarketplaces").is_none());
}

#[test]
fn unrelated_settings_keys_and_order_are_preserved_across_integrate_and_remove() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    let original_raw = r#"{
  "zeta": "last-alphabetically-first-in-file",
  "alpha": {
    "z_nested": [1, true, null, "café"],
    "a_nested": 3.5
  },
  "extraKnownMarketplaces": {
    "iris": {
      "source": {
        "source": "directory",
        "path": "/example/iris"
      }
    }
  },
  "middle": { "keep": "exact" },
  "enabledPlugins": {
    "iris@iris": true
  },
  "omega": false
}
"#;
    fs::write(sb.settings_path(), original_raw).unwrap();
    let original_val: Value = serde_json::from_str(original_raw).unwrap();

    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    let after_integrate = read_json(&sb.settings_path());
    let keys: Vec<&str> = after_integrate
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "zeta",
            "alpha",
            "extraKnownMarketplaces",
            "middle",
            "enabledPlugins",
            "omega"
        ]
    );
    let nested_keys: Vec<&str> = after_integrate["alpha"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(nested_keys, ["z_nested", "a_nested"]);
    for k in ["zeta", "alpha", "middle", "omega"] {
        assert_eq!(after_integrate[k], original_val[k]);
    }
    assert_eq!(
        after_integrate["extraKnownMarketplaces"]["iris"],
        original_val["extraKnownMarketplaces"]["iris"]
    );
    assert_eq!(
        after_integrate["enabledPlugins"]["iris@iris"],
        original_val["enabledPlugins"]["iris@iris"]
    );

    // Add a key after integrate so --remove must take the semantic edit path (not restore the
    // backup) while preserving both sibling keys inside the managed sections and overall key order.
    let mut with_later_edit = after_integrate.clone();
    with_later_edit["addedAfter"] = json!("kept");
    fs::write(
        sb.settings_path(),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&with_later_edit).unwrap()
        ),
    )
    .unwrap();

    assert!(
        sb.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    let after_remove = read_json(&sb.settings_path());
    let mut expected_after_remove = original_val.clone();
    expected_after_remove["addedAfter"] = json!("kept");
    assert_eq!(after_remove, expected_after_remove);
    let removed_keys: Vec<&str> = after_remove
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        removed_keys,
        [
            "zeta",
            "alpha",
            "extraKnownMarketplaces",
            "middle",
            "enabledPlugins",
            "omega",
            "addedAfter"
        ]
    );
}

#[test]
fn compact_settings_restores_original_bytes_when_unchanged_and_edits_semantically_when_modified() {
    // Variant 1: compact single-line JSON unchanged between integrate and --remove -> exact byte
    // restoration from backup.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    let compact = r#"{"zeta":1,"alpha":[true,"café"]}"#;
    fs::write(sb.settings_path(), compact).unwrap();

    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    let manifest = read_json(&sb.manifest_path());
    assert!(
        manifest["clients"]["claude-code"]["settings_written_sha256"]
            .as_str()
            .is_some_and(|hex| hex.len() == 64)
    );
    assert!(
        sb.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), compact);

    // Variant 2: user edits an unrelated key after integrate -> semantic edit preserves user edit.
    let sb2 = Sandbox::new();
    fs::create_dir_all(sb2.settings_path().parent().unwrap()).unwrap();
    fs::write(sb2.settings_path(), compact).unwrap();

    assert!(sb2.run(&["integrate", "claude-code"]).status.success());
    let mut edited = read_json(&sb2.settings_path());
    edited["userAdded"] = json!("kept");
    fs::write(
        sb2.settings_path(),
        serde_json::to_string_pretty(&edited).unwrap(),
    )
    .unwrap();

    assert!(
        sb2.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    let after = read_json(&sb2.settings_path());
    assert_eq!(after["zeta"], 1);
    assert_eq!(after["alpha"], json!([true, "café"]));
    assert_eq!(after["userAdded"], "kept");
    assert!(after.get("extraKnownMarketplaces").is_none());
    assert!(after.get("enabledPlugins").is_none());
}

#[test]
fn paths_with_spaces_and_non_ascii_work_end_to_end() {
    let sb = Sandbox::with_name(&format!("proxenos space café ąčę {}", Uuid::new_v4()));
    let bin = sb.hard_link_binary("bin dir café ąčę");
    #[cfg(not(target_os = "macos"))]
    let custom_state = sb.root.join("state dir café ąčę");
    #[cfg(target_os = "macos")]
    let custom_home = sb.root.join("home dir café ąčę");

    let run_with_env = |args: &[&str]| {
        let mut cmd = sb.command_for(&bin, args);
        #[cfg(all(unix, not(target_os = "macos")))]
        cmd.env("XDG_STATE_HOME", &custom_state);
        #[cfg(target_os = "macos")]
        cmd.env("HOME", &custom_home);
        #[cfg(windows)]
        cmd.env("LOCALAPPDATA", &custom_state);
        cmd.output().expect("proxenos binary runs")
    };

    let int_out = run_with_env(&["integrate", "claude-code"]);
    assert!(
        int_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&int_out.stderr)
    );

    #[cfg(not(target_os = "macos"))]
    let (expected_marketplace, settings_path) = (
        custom_state.join("proxenos").join("claude-marketplace"),
        sb.settings_path(),
    );
    #[cfg(target_os = "macos")]
    let (expected_marketplace, settings_path) = (
        custom_home
            .join("Library")
            .join("Application Support")
            .join("proxenos")
            .join("claude-marketplace"),
        custom_home.join(".claude").join("settings.json"),
    );
    let settings = read_json(&settings_path);
    assert_eq!(
        settings["extraKnownMarketplaces"]["proxenos"]["source"]["path"],
        expected_marketplace.to_str().unwrap()
    );
    let mcp = read_json(
        &expected_marketplace
            .join("plugins")
            .join("proxenos")
            .join(".mcp.json"),
    );
    assert_eq!(
        mcp["mcpServers"]["proxenos"]["command"],
        absolute_path_string(&bin)
    );

    let doc_out = run_with_env(&["doctor", "--json"]);
    assert!(
        doc_out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&doc_out.stdout),
        String::from_utf8_lossy(&doc_out.stderr)
    );
    let report: Value = serde_json::from_slice(&doc_out.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(
        find_check(&report, "claude-code.registration")["status"],
        "ok"
    );
    assert_eq!(
        find_check(&report, "claude-code.handshake")["details"]["server_name"],
        "proxenos"
    );

    let rm_out = run_with_env(&["integrate", "claude-code", "--remove"]);
    assert!(rm_out.status.success());
    assert!(!expected_marketplace.exists());
}

#[test]
fn atomic_write_and_remove_clean_up_on_directory_collision() {
    let sb = Sandbox::new();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let target = sb
        .marketplace_dir()
        .join(".claude-plugin")
        .join("marketplace.json");
    fs::remove_file(&target).unwrap();
    fs::create_dir(&target).unwrap();

    let rerun = sb.run(&["integrate", "claude-code"]);
    assert_eq!(rerun.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rerun.stderr).contains("marketplace.json"));
    let leftover: Vec<_> = fs::read_dir(target.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".proxenos.tmp.")
        })
        .collect();
    assert!(
        leftover.is_empty(),
        "atomic write left temporary files behind: {leftover:?}"
    );

    let remove = sb.run(&["integrate", "claude-code", "--remove"]);
    assert_eq!(remove.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("marketplace.json"));
}

#[test]
fn missing_home_and_state_environment_variables_fail_cleanly() {
    let sb = Sandbox::new();
    for args in [
        &["integrate", "claude-code"][..],
        &["integrate", "claude-code", "--remove"][..],
        &["doctor", "--json"][..],
    ] {
        let mut cmd = sb.command(args);
        cmd.env("HOME", "relative/path")
            .env("USERPROFILE", "relative/path")
            .env("LOCALAPPDATA", "relative/path");
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1), "args: {args:?}");
    }
}

#[test]
fn doctor_fails_when_not_registered_or_when_registration_is_corrupt() {
    // 1. Not registered (no settings.json, no state dir): text output exercises fail + warn labels.
    let sb = Sandbox::new();
    let text_out = sb.run(&["doctor"]);
    assert_eq!(text_out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(text.contains("[fail] claude-code.registration:"), "{text}");
    assert!(text.contains("[warn] state_dir.permissions:"), "{text}");
    assert!(text.contains("[fail] claude-code.handshake:"), "{text}");

    // 2. Empty settings.json (valid JSON but not registered).
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), "{}\n").unwrap();
    let empty_out = sb.run(&["doctor", "--json"]);
    assert_eq!(empty_out.status.code(), Some(1));
    let empty_report: Value = serde_json::from_slice(&empty_out.stdout).unwrap();
    assert_eq!(empty_report["ok"], false);
    assert_eq!(
        find_check(&empty_report, "claude-code.registration")["status"],
        "fail"
    );
    assert!(
        find_check(&empty_report, "claude-code.registration")["message"]
            .as_str()
            .unwrap()
            .contains("not registered and enabled")
    );

    // 3. Unparseable settings.json.
    fs::write(sb.settings_path(), "{bad json").unwrap();
    let bad_out = sb.run(&["doctor", "--json"]);
    assert_eq!(bad_out.status.code(), Some(1));
    let bad_report: Value = serde_json::from_slice(&bad_out.stdout).unwrap();
    assert!(
        find_check(&bad_report, "claude-code.registration")["message"]
            .as_str()
            .unwrap()
            .contains("Could not parse")
    );

    // 4. Corrupt marketplace.json and corrupt .mcp.json.
    let sb2 = Sandbox::new();
    assert!(sb2.run(&["integrate", "claude-code"]).status.success());
    let mp_path = sb2
        .marketplace_dir()
        .join(".claude-plugin")
        .join("marketplace.json");
    let mp_backup = fs::read(&mp_path).unwrap();
    fs::write(&mp_path, "{\"name\": \"wrong\", \"plugins\": []}\n").unwrap();
    let bad_mp = sb2.run(&["doctor", "--json"]);
    assert_eq!(bad_mp.status.code(), Some(1));
    let bad_mp_report: Value = serde_json::from_slice(&bad_mp.stdout).unwrap();
    assert!(
        find_check(&bad_mp_report, "claude-code.registration")["message"]
            .as_str()
            .unwrap()
            .contains("Invalid marketplace manifest")
    );
    fs::write(&mp_path, mp_backup).unwrap();

    let mcp_path = sb2
        .marketplace_dir()
        .join("plugins")
        .join("proxenos")
        .join(".mcp.json");
    fs::write(&mcp_path, "{\"mcpServers\": {\"proxenos\": {}}}\n").unwrap();
    let bad_mcp = sb2.run(&["doctor", "--json"]);
    assert_eq!(bad_mcp.status.code(), Some(1));
    let bad_mcp_report: Value = serde_json::from_slice(&bad_mcp.stdout).unwrap();
    assert!(
        find_check(&bad_mcp_report, "claude-code.registration")["message"]
            .as_str()
            .unwrap()
            .contains("Invalid plugin or MCP server definition")
    );
}

#[test]
fn doctor_fails_when_registered_binary_is_missing_or_unlaunchable() {
    let sb = Sandbox::new();
    let bin = sb.hard_link_binary("ephemeral-bin");
    let int_out = sb
        .command_for(&bin, &["integrate", "claude-code"])
        .output()
        .unwrap();
    assert!(int_out.status.success());

    // Delete the registered binary -> registration fails.
    fs::remove_file(&bin).unwrap();
    let missing_out = sb.run(&["doctor", "--json"]);
    assert_eq!(missing_out.status.code(), Some(1));
    let missing_report: Value = serde_json::from_slice(&missing_out.stdout).unwrap();
    assert_eq!(missing_report["ok"], false);
    assert_eq!(
        find_check(&missing_report, "claude-code.registration")["status"],
        "fail"
    );
    let missing_msg = find_check(&missing_report, "claude-code.registration")["message"]
        .as_str()
        .unwrap();
    assert!(
        missing_msg.contains("does not exist")
            && missing_msg.contains("proxenos integrate claude-code"),
        "{missing_report}"
    );
    assert_eq!(
        find_check(&missing_report, "claude-code.handshake")["status"],
        "fail"
    );

    // Replace with an empty non-executable file -> registration succeeds, handshake spawn fails.
    fs::write(&bin, []).unwrap();
    let broken_out = sb.run(&["doctor", "--json"]);
    assert_eq!(broken_out.status.code(), Some(1));
    let broken_report: Value = serde_json::from_slice(&broken_out.stdout).unwrap();
    assert_eq!(
        find_check(&broken_report, "claude-code.registration")["status"],
        "ok"
    );
    assert_eq!(
        find_check(&broken_report, "claude-code.handshake")["status"],
        "fail"
    );
    assert!(
        find_check(&broken_report, "claude-code.handshake")["message"]
            .as_str()
            .unwrap()
            .contains("I/O error")
    );
}

#[test]
fn doctor_json_shape_and_real_binary_handshake_and_usage_errors() {
    let sb = Sandbox::new();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let out = sb.run(&["doctor", "--json"]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["ok"], true);

    let checks = report["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "claude-code.registration",
            "state_dir.permissions",
            "cli.codex",
            "cli.antigravity",
            "cli.kimi",
            "cli.claude",
            "cli.copilot",
            "cli.grok",
            "claude-code.handshake"
        ]
    );
    for check in checks {
        assert!(matches!(
            check["status"].as_str(),
            Some("ok" | "warn" | "fail")
        ));
        assert!(check["message"].as_str().is_some_and(|s| !s.is_empty()));
    }

    let hs = find_check(&report, "claude-code.handshake");
    assert_eq!(hs["details"]["server_name"], "proxenos");
    assert_eq!(hs["details"]["server_version"], env!("CARGO_PKG_VERSION"));
    assert!(hs["details"]["tools"].is_array());

    // Text output succeeds too.
    let text_out = sb.run(&["doctor"]);
    assert!(text_out.status.success());
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(text.contains("[ok] claude-code.registration:"), "{text}");

    // Unknown integrate / doctor invocations exit with usage code 2.
    for bad in [
        &["integrate"][..],
        &["integrate", "other"][..],
        &["integrate", "claude-code", "--unknown"][..],
        &["doctor", "--unknown"][..],
    ] {
        let out = sb.run(bad);
        assert_eq!(out.status.code(), Some(2), "args: {bad:?}");
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn settings_symlink_is_preserved_across_integrate_and_remove_and_dangling_link_is_refused() {
    let sb = Sandbox::new();
    let dotfiles = sb.root.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("claude-settings.json");
    let original = "{\n  \"theme\": \"dark\",\n  \"nested\": {\n    \"keep\": true\n  }\n}\n";
    fs::write(&target, original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    }
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    if !try_file_symlink(&target, &sb.settings_path()).unwrap() {
        // Unprivileged Windows host without SeCreateSymbolicLinkPrivilege: exercise both the
        // resolved-link and dangling-link paths in resolve_settings_target via directory junctions,
        // which Windows allows without elevated privileges.
        #[cfg(windows)]
        {
            let mk_junction = |dest: &Path, link: &Path| {
                let status = Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(link)
                    .arg(dest)
                    .status()
                    .expect("cmd mklink /J runs");
                assert!(status.success(), "mklink /J failed: {status}");
            };
            mk_junction(&dotfiles, &sb.settings_path());
            let dir_refused = sb.run(&["integrate", "claude-code"]);
            assert_eq!(dir_refused.status.code(), Some(1));

            let sb_dangling = Sandbox::new();
            let gone = sb_dangling.root.join("gone-dir");
            fs::create_dir_all(&gone).unwrap();
            fs::create_dir_all(sb_dangling.settings_path().parent().unwrap()).unwrap();
            mk_junction(&gone, &sb_dangling.settings_path());
            fs::remove_dir(&gone).unwrap();
            let dangling_refused = sb_dangling.run(&["integrate", "claude-code"]);
            assert_eq!(dangling_refused.status.code(), Some(1));
            assert!(
                String::from_utf8_lossy(&dangling_refused.stderr).contains("cannot be resolved")
            );
        }
        return;
    }

    let out = sb.run(&["integrate", "claude-code"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::symlink_metadata(sb.settings_path())
            .unwrap()
            .file_type()
            .is_symlink(),
        "settings.json symlink was replaced by a regular file"
    );
    let target_after = read_json(&target);
    assert_eq!(target_after["theme"], "dark");
    assert_eq!(target_after["nested"]["keep"], true);
    assert_eq!(
        target_after["extraKnownMarketplaces"]["proxenos"]["source"]["path"],
        sb.marketplace_dir().to_str().unwrap()
    );
    assert_eq!(target_after["enabledPlugins"]["proxenos@proxenos"], true);
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    let manifest = read_json(&sb.manifest_path());
    let rec = &manifest["clients"]["claude-code"];
    assert_eq!(rec["settings_is_symlink"], true);
    assert_eq!(
        rec["settings_symlink_target"],
        canonical_path_string(&target)
    );

    let rm = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        rm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(
        fs::symlink_metadata(sb.settings_path())
            .unwrap()
            .file_type()
            .is_symlink(),
        "settings.json symlink was broken by --remove"
    );
    let target_restored = read_json(&target);
    assert_eq!(
        target_restored,
        serde_json::from_str::<Value>(original).unwrap()
    );
    assert!(!sb.backup_path().exists());

    // Dangling symlink must be refused with nothing changed.
    let sb_dangling = Sandbox::new();
    let missing_target = sb_dangling.root.join("dotfiles").join("missing.json");
    fs::create_dir_all(sb_dangling.settings_path().parent().unwrap()).unwrap();
    assert!(try_file_symlink(&missing_target, &sb_dangling.settings_path()).unwrap());

    let refused = sb_dangling.run(&["integrate", "claude-code"]);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("symlink") && stderr.contains("cannot be resolved"),
        "stderr: {stderr}"
    );
    assert!(
        fs::symlink_metadata(sb_dangling.settings_path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!sb_dangling.marketplace_dir().exists());
    assert!(!sb_dangling.manifest_path().exists());
}

#[test]
fn symlinked_binary_keeps_link_path_and_doctor_warns_on_path_mismatch() {
    #[cfg(windows)]
    if let Some(child_path) = std::env::var_os("PROXENOS_TEST_WIN_BARE_FALLBACK_PATH") {
        let out = Command::new("proxenos.exe")
            .args(["integrate", "claude-code"])
            .env_remove("PROXENOS_TEST_WIN_BARE_FALLBACK_PATH")
            .env("PATH", child_path)
            .output()
            .expect("bare proxenos.exe launches via helper PATH");
        assert!(
            out.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }

    let sb = Sandbox::new();
    let target_bin = sb.hard_link_binary("cellar/0.1.0/bin");
    let link_dir = sb.root.join("brew-bin");
    let path_link_dir = sb.root.join("path-bin");
    fs::create_dir_all(&link_dir).unwrap();
    fs::create_dir_all(&path_link_dir).unwrap();
    let link_bin = link_dir.join(format!("proxenos{}", std::env::consts::EXE_SUFFIX));
    let path_link_bin = path_link_dir.join(format!("proxenos{}", std::env::consts::EXE_SUFFIX));
    let mcp_path = sb
        .marketplace_dir()
        .join("plugins")
        .join("proxenos")
        .join(".mcp.json");

    if try_file_symlink(&target_bin, &link_bin).unwrap() {
        assert!(try_file_symlink(&target_bin, &path_link_bin).unwrap());

        // 1. Invoked directly via symlink path -> records symlink path, not versioned target.
        let by_path = sb
            .command_for(&link_bin, &["integrate", "claude-code"])
            .output()
            .unwrap();
        assert!(
            by_path.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&by_path.stderr)
        );
        let expected_link = absolute_path_string(&link_bin);
        assert_eq!(
            read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
            expected_link
        );
        assert_eq!(
            read_json(&sb.manifest_path())["clients"]["claude-code"]["binary_path"],
            expected_link
        );

        // Doctor with matching symlink on PATH reports registration ok.
        let mut ok_doc = sb.command_for(&link_bin, &["doctor", "--json"]);
        ok_doc.env("PATH", &link_dir);
        let ok_out = ok_doc.output().unwrap();
        assert!(
            ok_out.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&ok_out.stdout),
            String::from_utf8_lossy(&ok_out.stderr)
        );
        let ok_report: Value = serde_json::from_slice(&ok_out.stdout).unwrap();
        assert_eq!(
            find_check(&ok_report, "claude-code.registration")["status"],
            "ok",
            "{ok_report}"
        );

        // 2. Invoked by bare name `proxenos` resolved via PATH -> records PATH symlink path.
        let mut by_name = sb.command_for(Path::new("proxenos"), &["integrate", "claude-code"]);
        by_name.env("PATH", &path_link_dir);
        let by_name_out = by_name.output().unwrap();
        assert!(
            by_name_out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&by_name_out.stderr)
        );
        let expected_path_link = absolute_path_string(&path_link_bin);
        assert_eq!(
            read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
            expected_path_link
        );
        assert_eq!(
            read_json(&sb.manifest_path())["clients"]["claude-code"]["binary_path"],
            expected_path_link
        );
    } else {
        // Unprivileged Windows fallback without symlink privilege: still test bare-name PATH
        // lookup and matching-PATH doctor check.
        let path_bin = sb.hard_link_binary("path-bin");
        let mut by_name = sb.command_for(Path::new("proxenos"), &["integrate", "claude-code"]);
        by_name.env("PATH", &path_link_dir);
        let by_name_out = by_name.output().unwrap();
        assert!(
            by_name_out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&by_name_out.stderr)
        );
        assert_eq!(
            read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
            absolute_path_string(&path_bin)
        );

        let mut ok_doc = sb.command_for(&path_bin, &["doctor", "--json"]);
        ok_doc.env("PATH", &path_link_dir);
        let ok_out = ok_doc.output().unwrap();
        assert!(ok_out.status.success());
        let ok_report: Value = serde_json::from_slice(&ok_out.stdout).unwrap();
        assert_eq!(
            find_check(&ok_report, "claude-code.registration")["status"],
            "ok",
            "{ok_report}"
        );
    }

    // 3. Bare argv[0] when PATH has no matching entry falls back to current_exe().
    #[cfg(unix)]
    let (fallback_out, expected_fallback) = {
        use std::os::unix::process::CommandExt;
        let mut cmd = sb.command_for(&target_bin, &["integrate", "claude-code"]);
        cmd.arg0("proxenos");
        (cmd.output().unwrap(), canonical_path_string(&target_bin))
    };
    #[cfg(windows)]
    let (fallback_out, expected_fallback) = {
        // Re-exec this test binary with its own PATH set to target_bin's directory so the
        // intermediate process resolves bare `proxenos.exe` from its own PATH (independent of
        // Cargo's environment), while spawning the child with an absent PATH to exercise the
        // current_exe() fallback.
        let mut helper = sb.command_for(
            &std::env::current_exe().unwrap(),
            &[
                "--exact",
                "symlinked_binary_keeps_link_path_and_doctor_warns_on_path_mismatch",
                "--nocapture",
            ],
        );
        helper
            .env("PATH", target_bin.parent().unwrap())
            .env("PROXENOS_TEST_WIN_BARE_FALLBACK_PATH", &sb.absent_path);
        (helper.output().unwrap(), absolute_path_string(&target_bin))
    };
    assert!(
        fallback_out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&fallback_out.stdout),
        String::from_utf8_lossy(&fallback_out.stderr)
    );
    assert_eq!(
        read_json(&mcp_path)["mcpServers"]["proxenos"]["command"],
        expected_fallback
    );

    // 4. Doctor warns (exits 0) when `proxenos` on PATH resolves to a different binary.
    let other_bin = sb.hard_link_binary("cellar/0.2.0/bin");
    let mut mismatch_cmd = sb.command(&["doctor", "--json"]);
    mismatch_cmd.env("PATH", other_bin.parent().unwrap());
    let mismatch_out = mismatch_cmd.output().unwrap();
    assert!(
        mismatch_out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&mismatch_out.stdout),
        String::from_utf8_lossy(&mismatch_out.stderr)
    );
    let mismatch_report: Value = serde_json::from_slice(&mismatch_out.stdout).unwrap();
    assert_eq!(mismatch_report["ok"], true, "{mismatch_report}");
    let reg = find_check(&mismatch_report, "claude-code.registration");
    assert_eq!(reg["status"], "warn", "{mismatch_report}");
    let msg = reg["message"].as_str().unwrap();
    assert!(
        msg.contains("on PATH resolves to") && msg.contains("proxenos integrate claude-code"),
        "{mismatch_report}"
    );
    assert_eq!(
        reg["details"]["path_resolved"],
        canonical_path_string(&other_bin),
        "{mismatch_report}"
    );
    assert_eq!(
        find_check(&mismatch_report, "claude-code.handshake")["details"]["server_name"],
        "proxenos",
        "{mismatch_report}"
    );
}

#[test]
fn healing_rerun_clears_byte_restore_and_preserves_later_user_edits() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), "{\n  \"theme\": \"dark\"\n}\n").unwrap();

    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    // User edits an unrelated key, adds a new key, and deletes one managed entry.
    let mut settings = read_json(&sb.settings_path());
    settings["theme"] = json!("light");
    settings["userAdded"] = json!("keep-me");
    settings["enabledPlugins"]
        .as_object_mut()
        .unwrap()
        .shift_remove("proxenos@proxenos");
    fs::write(
        sb.settings_path(),
        format!("{}\n", serde_json::to_string_pretty(&settings).unwrap()),
    )
    .unwrap();

    // Rerun heals the missing managed entry and drops byte-exact restore eligibility.
    let heal = sb.run(&["integrate", "claude-code"]);
    assert!(
        heal.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&heal.stderr)
    );
    let manifest = read_json(&sb.manifest_path());
    assert!(
        manifest["clients"]["claude-code"]
            .get("settings_written_sha256")
            .is_none(),
        "healing rerun must clear byte-restore digest: {manifest}"
    );

    // --remove must edit semantically so both user edits survive (not overwrite with stale backup).
    let rm = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        rm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    let after = read_json(&sb.settings_path());
    assert_eq!(after["theme"], "light", "{after}");
    assert_eq!(after["userAdded"], "keep-me", "{after}");
    assert!(after.get("extraKnownMarketplaces").is_none(), "{after}");
    assert!(after.get("enabledPlugins").is_none(), "{after}");
}

#[test]
fn retargeted_settings_symlink_is_refused_on_rerun_and_remove() {
    let sb = Sandbox::new();
    let dotfiles = sb.root.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let target_a = dotfiles.join("a.json");
    let target_b = dotfiles.join("b.json");
    let orig_a = "{\n  \"origin\": \"A\"\n}\n";
    let orig_b = "{\n  \"origin\": \"B\"\n}\n";
    fs::write(&target_a, orig_a).unwrap();
    fs::write(&target_b, orig_b).unwrap();
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    if !try_file_symlink(&target_a, &sb.settings_path()).unwrap() {
        // Unprivileged Windows fallback: replace regular settings.json with a directory junction
        // after integrate so rerun and --remove still exercise check_settings_target_unchanged.
        #[cfg(windows)]
        {
            fs::write(sb.settings_path(), orig_a).unwrap();
            assert!(sb.run(&["integrate", "claude-code"]).status.success());
            fs::remove_file(sb.settings_path()).unwrap();
            let status = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(sb.settings_path())
                .arg(&dotfiles)
                .status()
                .expect("cmd mklink /J runs");
            assert!(status.success(), "mklink /J failed: {status}");
            let rerun = sb.run(&["integrate", "claude-code"]);
            assert_eq!(rerun.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&rerun.stderr).contains("symlink target changed"));
            let rm = sb.run(&["integrate", "claude-code", "--remove"]);
            assert_eq!(rm.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&rm.stderr).contains("symlink target changed"));
        }
        return;
    }

    assert!(sb.run(&["integrate", "claude-code"]).status.success());
    let a_after_integrate = fs::read_to_string(&target_a).unwrap();

    // Retarget the symlink from A to B.
    fs::remove_file(sb.settings_path()).unwrap();
    assert!(try_file_symlink(&target_b, &sb.settings_path()).unwrap());

    // Rerun must refuse and leave both A and B untouched.
    let rerun = sb.run(&["integrate", "claude-code"]);
    assert_eq!(rerun.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rerun.stderr).contains("symlink target changed"),
        "stderr: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read_to_string(&target_b).unwrap(), orig_b);
    assert_eq!(fs::read_to_string(&target_a).unwrap(), a_after_integrate);

    // --remove must also refuse and never overwrite B with A's backup.
    let rm = sb.run(&["integrate", "claude-code", "--remove"]);
    assert_eq!(rm.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rm.stderr).contains("symlink target changed"),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert_eq!(fs::read_to_string(&target_b).unwrap(), orig_b);
    assert_eq!(fs::read_to_string(&target_a).unwrap(), a_after_integrate);
    assert!(sb.manifest_path().exists());
}

#[test]
fn non_executable_file_on_path_is_ignored_by_doctor() {
    let sb = Sandbox::new();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let decoy_dir = sb.root.join("decoy-path");
    fs::create_dir_all(&decoy_dir).unwrap();
    // On Unix: mode 0644 (non-executable). On Windows: no PATHEXT extension.
    let decoy = decoy_dir.join("proxenos");
    fs::write(&decoy, "not an executable").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let mut cmd = sb.command(&["doctor", "--json"]);
    cmd.env("PATH", &decoy_dir);
    let out = cmd.output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(out.status.success(), "{report}");
    assert_eq!(
        find_check(&report, "claude-code.registration")["status"],
        "ok",
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_parent_directory_propagates_fsync_open_error() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    let claude_dir = sb.home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    // 0300 allows creating temp files and renaming inside the directory, but denies O_RDONLY
    // open for the post-rename directory fsync.
    fs::set_permissions(&claude_dir, fs::Permissions::from_mode(0o300)).unwrap();

    let out = sb.run(&["integrate", "claude-code"]);
    // Restore permissions before assertions so Sandbox::drop always cleans up.
    fs::set_permissions(&claude_dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("settings.json"), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn doctor_checks_unix_state_directory_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    assert!(sb.run(&["integrate", "claude-code"]).status.success());

    let ok_out = sb.run(&["doctor", "--json"]);
    assert!(ok_out.status.success());
    let ok_report: Value = serde_json::from_slice(&ok_out.stdout).unwrap();
    let state_check = find_check(&ok_report, "state_dir.permissions");
    assert_eq!(state_check["status"], "ok");
    assert_eq!(state_check["details"]["mode"], "0700");

    // Broadening permissions to 0755 must fail doctor.
    fs::set_permissions(sb.state_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    let wide_out = sb.run(&["doctor", "--json"]);
    assert_eq!(wide_out.status.code(), Some(1));
    let wide_report: Value = serde_json::from_slice(&wide_out.stdout).unwrap();
    let wide_check = find_check(&wide_report, "state_dir.permissions");
    assert_eq!(wide_check["status"], "fail");
    assert_eq!(wide_check["details"]["mode"], "0755");

    // Narrowing permissions to 0500 (not exact 0700 per D19) must also fail doctor.
    fs::set_permissions(sb.state_dir(), fs::Permissions::from_mode(0o500)).unwrap();
    let ro_out = sb.run(&["doctor", "--json"]);
    fs::set_permissions(sb.state_dir(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(ro_out.status.code(), Some(1));
    let ro_report: Value = serde_json::from_slice(&ro_out.stdout).unwrap();
    let ro_check = find_check(&ro_report, "state_dir.permissions");
    assert_eq!(ro_check["status"], "fail", "{ro_report}");
    assert_eq!(ro_check["details"]["mode"], "0500", "{ro_report}");

    // A symlinked state directory (even pointing to a 0700 directory) must fail doctor.
    // Note: testing a root-owned 0700 directory is not feasible in unprivileged tests.
    let sb_link = Sandbox::new();
    let real_state = sb_link.root.join("real-state");
    fs::create_dir_all(&real_state).unwrap();
    fs::set_permissions(&real_state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(sb_link.state_dir().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real_state, sb_link.state_dir()).unwrap();
    let link_out = sb_link.run(&["doctor", "--json"]);
    assert_eq!(link_out.status.code(), Some(1));
    let link_report: Value = serde_json::from_slice(&link_out.stdout).unwrap();
    let link_check = find_check(&link_report, "state_dir.permissions");
    assert_eq!(link_check["status"], "fail", "{link_report}");
    assert!(
        link_check["message"]
            .as_str()
            .unwrap()
            .contains("not a private directory"),
        "{link_report}"
    );

    // Non-directory parent for the platform state-directory env var exercises non-NotFound
    // symlink_metadata error (ENOTDIR).
    let file_parent = sb.root.join("not-a-dir");
    fs::write(&file_parent, "file").unwrap();
    let mut cmd = sb.command(&["doctor", "--json"]);
    #[cfg(not(target_os = "macos"))]
    cmd.env("XDG_STATE_HOME", &file_parent);
    #[cfg(target_os = "macos")]
    cmd.env("HOME", &file_parent);
    let err_out = cmd.output().unwrap();
    assert_eq!(err_out.status.code(), Some(1));
    let err_report: Value = serde_json::from_slice(&err_out.stdout).unwrap();
    let err_check = find_check(&err_report, "state_dir.permissions");
    assert_eq!(err_check["status"], "fail", "{err_report}");
    assert!(
        err_check["message"]
            .as_str()
            .unwrap()
            .contains("Could not inspect state directory"),
        "{err_report}"
    );
}

#[cfg(windows)]
#[test]
fn doctor_checks_windows_state_directory_reparse_point_and_errors() {
    let sb = Sandbox::new();
    let real_state = sb.root.join("real-state");
    fs::create_dir_all(&real_state).unwrap();
    fs::create_dir_all(sb.state_dir().parent().unwrap()).unwrap();
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(sb.state_dir())
        .arg(&real_state)
        .status()
        .expect("cmd mklink /J runs");
    assert!(status.success(), "mklink /J failed: {status}");

    let junction_out = sb.run(&["doctor", "--json"]);
    assert_eq!(junction_out.status.code(), Some(1));
    let junction_report: Value = serde_json::from_slice(&junction_out.stdout).unwrap();
    let junction_check = find_check(&junction_report, "state_dir.permissions");
    assert_eq!(junction_check["status"], "fail", "{junction_report}");
    assert!(
        junction_check["message"]
            .as_str()
            .unwrap()
            .contains("reparse point"),
        "{junction_report}"
    );

    let mut invalid_cmd = sb.command(&["doctor", "--json"]);
    invalid_cmd.env("LOCALAPPDATA", sb.root.join("bad<name"));
    let invalid_out = invalid_cmd.output().unwrap();
    assert_eq!(invalid_out.status.code(), Some(1));
    let invalid_report: Value = serde_json::from_slice(&invalid_out.stdout).unwrap();
    let invalid_check = find_check(&invalid_report, "state_dir.permissions");
    assert_eq!(invalid_check["status"], "fail", "{invalid_report}");
    assert!(
        invalid_check["message"]
            .as_str()
            .unwrap()
            .contains("Could not inspect state directory"),
        "{invalid_report}"
    );
}

#[cfg(unix)]
#[test]
fn failed_integrate_on_unwritable_integrate_dir_recovers_on_rerun_and_remove() {
    use std::os::unix::fs::PermissionsExt;

    // Variant 1: existing settings.json + unwritable <state>/integrate (mode 0500).
    let sb = Sandbox::new();
    let original = "{\n  \"theme\": \"dark\"\n}\n";
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), original).unwrap();
    let integrate_dir = sb.state_dir().join("integrate");
    fs::create_dir_all(&integrate_dir).unwrap();
    fs::set_permissions(&integrate_dir, fs::Permissions::from_mode(0o500)).unwrap();

    let failed = sb.run(&["integrate", "claude-code"]);
    fs::set_permissions(&integrate_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        !sb.marketplace_dir().exists(),
        "failed integrate must not leave an unowned marketplace directory"
    );
    assert!(!sb.manifest_path().exists());
    assert!(!sb.backup_path().exists());
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), original);

    let rerun = sb.run(&["integrate", "claude-code"]);
    assert!(
        rerun.status.success(),
        "rerun after restoring permissions failed: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);
    assert!(
        read_json(&sb.manifest_path())["clients"]["claude-code"]
            .get("pending")
            .is_none()
    );

    let rm = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        rm.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), original);
    assert!(!sb.marketplace_dir().exists());
    assert!(!sb.manifest_path().exists());
    assert!(!sb.backup_path().exists());

    // Variant 2: absent ~/.claude + unwritable <state>/integrate (mode 0500) must not orphan
    // ~/.claude or claude-marketplace.
    let sb2 = Sandbox::new();
    let integrate_dir2 = sb2.state_dir().join("integrate");
    fs::create_dir_all(&integrate_dir2).unwrap();
    fs::set_permissions(&integrate_dir2, fs::Permissions::from_mode(0o500)).unwrap();

    let failed2 = sb2.run(&["integrate", "claude-code"]);
    fs::set_permissions(&integrate_dir2, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(failed2.status.code(), Some(1));
    assert!(!sb2.home.join(".claude").exists());
    assert!(!sb2.marketplace_dir().exists());
    assert!(!sb2.manifest_path().exists());

    assert!(sb2.run(&["integrate", "claude-code"]).status.success());
    assert!(
        sb2.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    assert!(!sb2.home.join(".claude").exists());
    assert!(!sb2.marketplace_dir().exists());
    assert!(!sb2.manifest_path().exists());
}

#[test]
fn failed_backup_after_pending_manifest_recovers_on_rerun_and_remove() {
    // Variant 1: pending manifest succeeds, backup dir creation fails because a file occupies
    // <state>/integrate/backups. Clearing the blocker lets rerun take the backup and succeed.
    let sb = Sandbox::new();
    let original = "{\n  \"theme\": \"dark\"\n}\n";
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), original).unwrap();
    let integrate_dir = sb.state_dir().join("integrate");
    fs::create_dir_all(&integrate_dir).unwrap();
    let backups_blocker = integrate_dir.join("backups");
    fs::write(&backups_blocker, "blocker").unwrap();

    let failed = sb.run(&["integrate", "claude-code"]);
    assert_eq!(failed.status.code(), Some(1));
    let pending_manifest = read_json(&sb.manifest_path());
    assert_eq!(pending_manifest["clients"]["claude-code"]["pending"], true);
    assert!(!sb.marketplace_dir().exists());
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), original);

    fs::remove_file(&backups_blocker).unwrap();
    let rerun = sb.run(&["integrate", "claude-code"]);
    assert!(
        rerun.status.success(),
        "rerun after clearing backup blocker failed: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read_to_string(sb.backup_path()).unwrap(), original);
    let committed = read_json(&sb.manifest_path());
    assert!(committed["clients"]["claude-code"].get("pending").is_none());
    assert!(
        committed["clients"]["claude-code"]["settings_written_sha256"]
            .as_str()
            .is_some_and(|hex| hex.len() == 64)
    );

    let rm = sb.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        rm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert_eq!(fs::read_to_string(sb.settings_path()).unwrap(), original);
    assert!(!sb.marketplace_dir().exists());
    assert!(!sb.manifest_path().exists());
    assert!(!sb.backup_path().exists());

    // Variant 2: --remove directly after a failed backup cleans up the pending manifest cleanly.
    let sb2 = Sandbox::new();
    fs::create_dir_all(sb2.settings_path().parent().unwrap()).unwrap();
    fs::write(sb2.settings_path(), original).unwrap();
    let integrate_dir2 = sb2.state_dir().join("integrate");
    fs::create_dir_all(&integrate_dir2).unwrap();
    let backups_blocker2 = integrate_dir2.join("backups");
    fs::write(&backups_blocker2, "blocker").unwrap();

    assert_eq!(
        sb2.run(&["integrate", "claude-code"]).status.code(),
        Some(1)
    );
    assert_eq!(
        read_json(&sb2.manifest_path())["clients"]["claude-code"]["pending"],
        true
    );

    fs::remove_file(&backups_blocker2).unwrap();
    let rm2 = sb2.run(&["integrate", "claude-code", "--remove"]);
    assert!(
        rm2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm2.stderr)
    );
    assert_eq!(fs::read_to_string(sb2.settings_path()).unwrap(), original);
    assert!(!sb2.manifest_path().exists());
    assert!(!sb2.marketplace_dir().exists());
}

#[test]
fn changed_settings_path_under_same_state_dir_is_refused_on_rerun_and_remove() {
    // Part 1: On Linux (shared XDG_STATE_HOME with different HOME) and Windows (shared
    // LOCALAPPDATA with different USERPROFILE), switching homes under the same state directory
    // must be refused on both rerun and --remove without modifying either file or the manifest.
    // On macOS, state_dir derives from HOME, so a home switch cannot share state_dir; Part 2
    // exercises the same path-mismatch refusal via CLAUDE_CONFIG_DIR on every platform including
    // macOS.
    #[cfg(not(target_os = "macos"))]
    {
        let sb = Sandbox::new();
        let shared_state = sb.root.join("shared-state");
        let h1 = sb.root.join("h1");
        let h2 = sb.root.join("h2");
        let s1 = h1.join(".claude").join("settings.json");
        let s2 = h2.join(".claude").join("settings.json");
        let orig1 = "{\n  \"profile\": \"h1\"\n}\n";
        let orig2 = "{\n  \"profile\": \"h2\"\n}\n";
        fs::create_dir_all(s1.parent().unwrap()).unwrap();
        fs::create_dir_all(s2.parent().unwrap()).unwrap();
        fs::write(&s1, orig1).unwrap();
        fs::write(&s2, orig2).unwrap();

        let run_home = |home: &Path, args: &[&str]| {
            let mut cmd = sb.command(args);
            cmd.env("HOME", home).env("USERPROFILE", home);
            #[cfg(all(unix, not(target_os = "macos")))]
            cmd.env("XDG_STATE_HOME", &shared_state);
            #[cfg(windows)]
            cmd.env("LOCALAPPDATA", &shared_state);
            cmd.output().expect("proxenos binary runs")
        };

        let first = run_home(&h1, &["integrate", "claude-code"]);
        assert!(
            first.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let s1_after = fs::read_to_string(&s1).unwrap();
        let manifest_path = shared_state
            .join("proxenos")
            .join("integrate")
            .join("manifest.json");
        let backup_path = shared_state
            .join("proxenos")
            .join("integrate")
            .join("backups")
            .join("claude-code-settings.json");
        let manifest_before = fs::read_to_string(&manifest_path).unwrap();

        let rerun = run_home(&h2, &["integrate", "claude-code"]);
        assert_eq!(rerun.status.code(), Some(1));
        let rerun_err = String::from_utf8_lossy(&rerun.stderr);
        assert!(
            rerun_err.contains("settings path changed"),
            "stderr: {rerun_err}"
        );
        assert_eq!(fs::read_to_string(&s1).unwrap(), s1_after);
        assert_eq!(fs::read_to_string(&s2).unwrap(), orig2);
        assert_eq!(fs::read_to_string(&manifest_path).unwrap(), manifest_before);
        assert_eq!(fs::read_to_string(&backup_path).unwrap(), orig1);

        let rm_wrong = run_home(&h2, &["integrate", "claude-code", "--remove"]);
        assert_eq!(rm_wrong.status.code(), Some(1));
        let rm_err = String::from_utf8_lossy(&rm_wrong.stderr);
        assert!(rm_err.contains("settings path changed"), "stderr: {rm_err}");
        assert_eq!(fs::read_to_string(&s1).unwrap(), s1_after);
        assert_eq!(fs::read_to_string(&s2).unwrap(), orig2);
        assert_eq!(fs::read_to_string(&manifest_path).unwrap(), manifest_before);
        assert_eq!(fs::read_to_string(&backup_path).unwrap(), orig1);

        let rm_right = run_home(&h1, &["integrate", "claude-code", "--remove"]);
        assert!(
            rm_right.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&rm_right.stderr)
        );
        assert_eq!(fs::read_to_string(&s1).unwrap(), orig1);
        assert_eq!(fs::read_to_string(&s2).unwrap(), orig2);
        assert!(!manifest_path.exists());
    }

    // Part 2: Changing CLAUDE_CONFIG_DIR under the same state directory is refused on every
    // platform (including macOS).
    let sb = Sandbox::new();
    let cfg_a = sb.root.join("cfg-a");
    let cfg_b = sb.root.join("cfg-b");
    let sa = cfg_a.join("settings.json");
    let sb_file = cfg_b.join("settings.json");
    let orig_a = "{\n  \"profile\": \"A\"\n}\n";
    let orig_b = "{\n  \"profile\": \"B\"\n}\n";
    fs::create_dir_all(&cfg_a).unwrap();
    fs::create_dir_all(&cfg_b).unwrap();
    fs::write(&sa, orig_a).unwrap();
    fs::write(&sb_file, orig_b).unwrap();

    let run_cfg = |cfg: &Path, args: &[&str]| {
        let mut cmd = sb.command(args);
        cmd.env("CLAUDE_CONFIG_DIR", cfg);
        cmd.output().expect("proxenos binary runs")
    };

    assert!(
        run_cfg(&cfg_a, &["integrate", "claude-code"])
            .status
            .success()
    );
    let sa_after = fs::read_to_string(&sa).unwrap();
    let manifest_before = fs::read_to_string(sb.manifest_path()).unwrap();

    let rerun_b = run_cfg(&cfg_b, &["integrate", "claude-code"]);
    assert_eq!(rerun_b.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rerun_b.stderr).contains("settings path changed"),
        "stderr: {}",
        String::from_utf8_lossy(&rerun_b.stderr)
    );
    assert_eq!(fs::read_to_string(&sa).unwrap(), sa_after);
    assert_eq!(fs::read_to_string(&sb_file).unwrap(), orig_b);
    assert_eq!(
        fs::read_to_string(sb.manifest_path()).unwrap(),
        manifest_before
    );

    let rm_b = run_cfg(&cfg_b, &["integrate", "claude-code", "--remove"]);
    assert_eq!(rm_b.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rm_b.stderr).contains("settings path changed"),
        "stderr: {}",
        String::from_utf8_lossy(&rm_b.stderr)
    );
    assert_eq!(fs::read_to_string(&sa).unwrap(), sa_after);
    assert_eq!(fs::read_to_string(&sb_file).unwrap(), orig_b);
    assert_eq!(
        fs::read_to_string(sb.manifest_path()).unwrap(),
        manifest_before
    );

    assert!(
        run_cfg(&cfg_a, &["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&sa).unwrap(), orig_a);
    assert_eq!(fs::read_to_string(&sb_file).unwrap(), orig_b);
    assert!(!sb.manifest_path().exists());
}

#[test]
fn claude_config_dir_controls_settings_location_for_integrate_and_doctor() {
    let sb = Sandbox::new();
    let default_original = "{\n  \"scope\": \"default\"\n}\n";
    fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
    fs::write(sb.settings_path(), default_original).unwrap();

    // 1. Set CLAUDE_CONFIG_DIR to an absolute path: integrate writes <CLAUDE_CONFIG_DIR>/settings.json
    // directly and leaves ~/.claude/settings.json untouched.
    let custom_dir = sb.root.join("claude-work");
    let custom_settings = custom_dir.join("settings.json");
    let custom_original = "{\n  \"scope\": \"work\"\n}\n";
    fs::create_dir_all(&custom_dir).unwrap();
    fs::write(&custom_settings, custom_original).unwrap();

    let mut int_custom = sb.command(&["integrate", "claude-code"]);
    int_custom.env("CLAUDE_CONFIG_DIR", &custom_dir);
    let int_out = int_custom.output().unwrap();
    assert!(
        int_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&int_out.stderr)
    );
    assert_eq!(
        fs::read_to_string(sb.settings_path()).unwrap(),
        default_original,
        "default ~/.claude/settings.json must stay untouched when CLAUDE_CONFIG_DIR is set"
    );
    let written_custom = read_json(&custom_settings);
    assert_eq!(written_custom["scope"], "work");
    assert_eq!(written_custom["enabledPlugins"]["proxenos@proxenos"], true);
    assert_eq!(
        written_custom["extraKnownMarketplaces"]["proxenos"]["source"]["path"],
        sb.marketplace_dir().to_str().unwrap()
    );
    assert_eq!(
        read_json(&sb.manifest_path())["clients"]["claude-code"]["settings_path"],
        custom_settings.to_str().unwrap()
    );

    // Doctor honours CLAUDE_CONFIG_DIR when set...
    let mut doc_custom = sb.command(&["doctor", "--json"]);
    doc_custom.env("CLAUDE_CONFIG_DIR", &custom_dir);
    let doc_custom_out = doc_custom.output().unwrap();
    let doc_custom_report: Value = serde_json::from_slice(&doc_custom_out.stdout).unwrap();
    assert!(doc_custom_out.status.success(), "{doc_custom_report}");
    let reg = find_check(&doc_custom_report, "claude-code.registration");
    assert_eq!(reg["status"], "ok", "{doc_custom_report}");
    assert_eq!(
        reg["details"]["settings_path"],
        custom_settings.to_str().unwrap()
    );

    // ...and looks in ~/.claude/settings.json when unset, failing because it is not registered there.
    let doc_unset_out = sb.run(&["doctor", "--json"]);
    assert_eq!(doc_unset_out.status.code(), Some(1));
    let doc_unset_report: Value = serde_json::from_slice(&doc_unset_out.stdout).unwrap();
    assert_eq!(
        find_check(&doc_unset_report, "claude-code.registration")["status"],
        "fail",
        "{doc_unset_report}"
    );

    // Removing with CLAUDE_CONFIG_DIR restores <CLAUDE_CONFIG_DIR>/settings.json cleanly.
    let mut rm_custom = sb.command(&["integrate", "claude-code", "--remove"]);
    rm_custom.env("CLAUDE_CONFIG_DIR", &custom_dir);
    let rm_custom_out = rm_custom.output().unwrap();
    assert!(
        rm_custom_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rm_custom_out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&custom_settings).unwrap(),
        custom_original
    );
    assert_eq!(
        fs::read_to_string(sb.settings_path()).unwrap(),
        default_original
    );

    // 2. Empty CLAUDE_CONFIG_DIR is treated as unset and uses ~/.claude/settings.json.
    let mut int_empty = sb.command(&["integrate", "claude-code"]);
    int_empty.env("CLAUDE_CONFIG_DIR", "");
    assert!(int_empty.output().unwrap().status.success());
    assert_eq!(
        read_json(&sb.settings_path())["enabledPlugins"]["proxenos@proxenos"],
        true
    );
    let doc_default = sb.run(&["doctor", "--json"]);
    assert!(doc_default.status.success());
    let doc_default_report: Value = serde_json::from_slice(&doc_default.stdout).unwrap();
    assert_eq!(
        find_check(&doc_default_report, "claude-code.registration")["details"]["settings_path"],
        sb.settings_path().to_str().unwrap()
    );
    assert!(
        sb.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(sb.settings_path()).unwrap(),
        default_original
    );

    // 3. Relative or `..`-containing CLAUDE_CONFIG_DIR is refused by integrate, --remove, and doctor.
    let dotdot = sb.root.join("sub").join("..").join("cfg");
    let invalid_values = [PathBuf::from("relative/claude"), dotdot];
    for invalid in &invalid_values {
        for args in [
            &["integrate", "claude-code"][..],
            &["integrate", "claude-code", "--remove"][..],
        ] {
            let mut cmd = sb.command(args);
            cmd.env("CLAUDE_CONFIG_DIR", invalid);
            let out = cmd.output().unwrap();
            assert_eq!(out.status.code(), Some(1), "invalid: {invalid:?}, {args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("CLAUDE_CONFIG_DIR"),
                "stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let mut doc_bad = sb.command(&["doctor", "--json"]);
        doc_bad.env("CLAUDE_CONFIG_DIR", invalid);
        let doc_bad_out = doc_bad.output().unwrap();
        assert_eq!(doc_bad_out.status.code(), Some(1));
        let doc_bad_report: Value = serde_json::from_slice(&doc_bad_out.stdout).unwrap();
        let reg = find_check(&doc_bad_report, "claude-code.registration");
        assert_eq!(reg["status"], "fail", "{doc_bad_report}");
        assert!(
            reg["message"]
                .as_str()
                .unwrap()
                .contains("CLAUDE_CONFIG_DIR"),
            "{doc_bad_report}"
        );
    }
    assert_eq!(
        fs::read_to_string(sb.settings_path()).unwrap(),
        default_original
    );
}

#[cfg(feature = "test-tools")]
mod with_stubs {
    use super::*;
    use std::time::{Duration, Instant};

    fn stub_bin() -> &'static Path {
        Path::new(env!("CARGO_BIN_EXE_proxenos-stub"))
    }

    /// Temporary directory outside `target/llvm-cov-target` for LLVM_PROFILE_FILE of children
    /// that a test deliberately kills by signal, so a kill during profile startup or flush cannot
    /// leave a corrupt `.profraw` in `cargo-llvm-cov`'s merge directory (especially on macOS).
    struct KilledProfileDir(PathBuf);

    impl KilledProfileDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("proxenos-killed-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn pattern(&self) -> PathBuf {
            self.0.join("killed-%p.profraw")
        }
    }

    impl Drop for KilledProfileDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn install_stub(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        let dest = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        fs::hard_link(stub_bin(), &dest).unwrap_or_else(|error| {
            panic!(
                "hard-linking {} to {} failed: {error}",
                stub_bin().display(),
                dest.display()
            )
        });
    }

    #[test]
    fn doctor_is_all_ok_after_integrate_with_stub_providers_on_path() {
        let sb = Sandbox::new();
        let bin_dir = sb.root.join("stub-bin");
        install_stub(&bin_dir, "codex");
        install_stub(&bin_dir, "agy");
        install_stub(&bin_dir, "kimi");
        fs::write(
            sb.root.join("kimi.version"),
            json!({"stdout": "2.8.0\n"}).to_string(),
        )
        .unwrap();
        install_stub(&bin_dir, "grok");
        fs::write(
            sb.root.join("grok.version"),
            json!({"stdout": "grok 1.0.92 (b06756fd6256)\n"}).to_string(),
        )
        .unwrap();
        install_stub(&bin_dir, "copilot");
        fs::write(
            sb.root.join("copilot.version"),
            json!({"stdout": "GitHub Copilot CLI 1.0.116.\nRun 'copilot update' to check for updates.\n"}).to_string(),
        )
        .unwrap();
        install_stub(&bin_dir, "claude");
        fs::write(
            sb.root.join("claude.version"),
            json!({"stdout": "2.1.332 (Claude Code)\n"}).to_string(),
        )
        .unwrap();
        fs::write(
            sb.root.join("codex.version"),
            json!({"stdout": "codex 0.50.0\n"}).to_string(),
        )
        .unwrap();
        fs::write(
            sb.root.join("agy.version"),
            json!({"stderr": "agy 2.1.0\n"}).to_string(),
        )
        .unwrap();

        assert!(sb.run(&["integrate", "claude-code"]).status.success());

        let mut cmd = sb.command(&["doctor", "--json"]);
        cmd.env("PATH", &bin_dir).env("PROXENOS_STUB_DIR", &sb.root);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["ok"], true, "{report}");
        for check in report["checks"].as_array().unwrap() {
            assert_eq!(check["status"], "ok", "check failed: {check}");
        }
        assert_eq!(
            find_check(&report, "cli.codex")["details"]["version"],
            "codex 0.50.0"
        );
        assert_eq!(
            find_check(&report, "cli.antigravity")["details"]["version"],
            "agy 2.1.0"
        );
        assert_eq!(
            find_check(&report, "cli.kimi")["details"]["version"],
            "2.8.0"
        );
        assert_eq!(
            find_check(&report, "cli.claude")["details"]["version"],
            "2.1.332 (Claude Code)"
        );
        assert_eq!(
            find_check(&report, "cli.copilot")["details"]["version"],
            "GitHub Copilot CLI 1.0.116."
        );
        assert_eq!(
            find_check(&report, "cli.grok")["details"]["version"],
            "grok 1.0.92 (b06756fd6256)"
        );
        let tools: Vec<&str> = find_check(&report, "claude-code.handshake")["details"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        for expected in [
            "codex",
            "codex-reply",
            "antigravity",
            "antigravity-reply",
            "copilot",
            "copilot-reply",
            "grok",
            "grok-reply",
        ] {
            assert!(
                tools.contains(&expected),
                "missing tool {expected} in {tools:?}"
            );
        }

        // Also check human-readable text output when everything is ok.
        let mut text_cmd = sb.command(&["doctor"]);
        text_cmd
            .env("PATH", &bin_dir)
            .env("PROXENOS_STUB_DIR", &sb.root);
        let text_out = text_cmd.output().unwrap();
        assert!(text_out.status.success());
        let text = String::from_utf8_lossy(&text_out.stdout);
        assert!(
            text.contains("[ok] cli.codex: codex codex 0.50.0"),
            "{text}"
        );
        assert!(text.contains("[ok] claude-code.handshake:"), "{text}");
    }

    #[test]
    fn doctor_fails_when_a_discovered_provider_cli_fails_version_check() {
        let sb = Sandbox::new();
        let bin_dir = sb.root.join("stub-bin");
        install_stub(&bin_dir, "codex");
        fs::write(
            sb.root.join("codex.version"),
            json!({"code": 1, "stderr": "broken binary"}).to_string(),
        )
        .unwrap();

        assert!(sb.run(&["integrate", "claude-code"]).status.success());

        let mut cmd = sb.command(&["doctor", "--json"]);
        cmd.env("PATH", &bin_dir).env("PROXENOS_STUB_DIR", &sb.root);
        let out = cmd.output().unwrap();
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(1), "{report}");
        assert_eq!(report["ok"], false, "{report}");
        assert_eq!(
            find_check(&report, "cli.codex")["status"],
            "fail",
            "{report}"
        );
        assert_eq!(
            find_check(&report, "cli.antigravity")["status"],
            "warn",
            "{report}"
        );
    }

    #[test]
    fn doctor_handshake_handles_protocol_errors_empty_tools_and_kills_hung_child() {
        let sb = Sandbox::new();
        assert!(sb.run(&["integrate", "claude-code"]).status.success());
        let mcp_path = sb
            .marketplace_dir()
            .join("plugins")
            .join("proxenos")
            .join(".mcp.json");
        let stub = stub_bin().to_str().unwrap().to_owned();

        let set_stub_args = |args: Value| {
            let mcp = json!({
                "mcpServers": {
                    "proxenos": {
                        "command": stub,
                        "args": args
                    }
                }
            });
            fs::write(&mcp_path, serde_json::to_string_pretty(&mcp).unwrap()).unwrap();
        };

        // 1. Notification before response, then empty tools list -> warn (exit 0).
        // Trailing "stdin" keeps the stub's stdin pipe open until doctor drops it after reading
        // responses, preventing a fast-exit EPIPE race while doctor writes follow-up frames.
        let note = json!({"jsonrpc":"2.0","method":"notifications/progress"}).to_string();
        let init_ok = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "stub-mcp", "version": "0.1.0" }
            }
        })
        .to_string();
        let list_empty = json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}}).to_string();
        set_stub_args(json!([
            format!("out:{note}"),
            format!("out:{init_ok}"),
            format!("out:{list_empty}"),
            "stdin"
        ]));
        let empty_out = sb.run(&["doctor", "--json"]);
        let empty_report: Value = serde_json::from_slice(&empty_out.stdout).unwrap();
        assert!(
            empty_out.status.success(),
            "report: {empty_report}\nstderr: {}",
            String::from_utf8_lossy(&empty_out.stderr)
        );
        assert_eq!(
            find_check(&empty_report, "claude-code.handshake")["status"],
            "warn",
            "{empty_report}"
        );
        let empty_msg = find_check(&empty_report, "claude-code.handshake")["message"]
            .as_str()
            .unwrap();
        assert!(
            empty_msg.contains("no provider CLI found on PATH")
                && empty_msg.contains("codex")
                && empty_msg.contains("agy"),
            "{empty_report}"
        );

        // 1b. Same 0-tools stub response when a provider CLI *is* on PATH -> warns without
        // claiming no provider CLI was found. Hard-linking the proxenos binary under the name
        // `codex` gives a self-contained `--version` responder without setting PROXENOS_STUB_DIR
        // (which would put the MCP stub into replay mode).
        let stub_dir = sb.root.join("one-provider");
        fs::create_dir_all(&stub_dir).unwrap();
        fs::hard_link(
            proxenos_bin(),
            stub_dir.join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        let mut with_cli = sb.command(&["doctor", "--json"]);
        with_cli.env("PATH", &stub_dir);
        let with_cli_out = with_cli.output().unwrap();
        let with_cli_report: Value = serde_json::from_slice(&with_cli_out.stdout).unwrap();
        assert!(with_cli_out.status.success(), "{with_cli_report}");
        let with_cli_msg = find_check(&with_cli_report, "claude-code.handshake")["message"]
            .as_str()
            .unwrap();
        assert_eq!(
            find_check(&with_cli_report, "claude-code.handshake")["status"],
            "warn",
            "{with_cli_report}"
        );
        assert!(
            with_cli_msg.contains("0 tools") && !with_cli_msg.contains("no provider CLI"),
            "{with_cli_report}"
        );

        // 1c. After tools/list, doctor closes stdin and drains enough shutdown output to let
        // the server exit on its own. The stub writes more than a pipe can buffer and creates
        // the marker only after the write completes.
        let list_one =
            json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"codex"}]}}).to_string();
        let eof_marker = sb.root.join("exited-cleanly-on-eof");
        set_stub_args(json!([
            format!("out:{init_ok}"),
            format!("out:{list_one}"),
            "stdin",
            "flood:2097152",
            "sleep:150",
            format!("mkdir:{}", eof_marker.display()),
            "exit:0"
        ]));
        let clean_out = sb.run(&["doctor", "--json"]);
        let clean_report: Value = serde_json::from_slice(&clean_out.stdout).unwrap();
        assert!(clean_out.status.success(), "{clean_report}");
        assert_eq!(
            find_check(&clean_report, "claude-code.handshake")["status"],
            "ok",
            "{clean_report}"
        );
        assert!(
            eof_marker.is_dir(),
            "server did not finish cleanly on stdin EOF without signal: {clean_report}"
        );

        // 2. Closed stdout before answering (while stdin stays open).
        set_stub_args(json!(["close-output", "stdin"]));
        let eof_out = sb.run(&["doctor", "--json"]);
        let eof_report: Value = serde_json::from_slice(&eof_out.stdout).unwrap();
        assert_eq!(eof_out.status.code(), Some(1), "{eof_report}");
        assert!(
            find_check(&eof_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("closed stdout"),
            "{eof_report}"
        );

        // 3. Invalid JSON on stdout.
        set_stub_args(json!(["out:not-json", "stdin"]));
        let bad_json_out = sb.run(&["doctor", "--json"]);
        let bad_json_report: Value = serde_json::from_slice(&bad_json_out.stdout).unwrap();
        assert_eq!(bad_json_out.status.code(), Some(1), "{bad_json_report}");
        assert!(
            find_check(&bad_json_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("invalid JSON"),
            "{bad_json_report}"
        );

        // 4. JSON-RPC error response.
        let rpc_err =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad"}}).to_string();
        set_stub_args(json!([format!("out:{rpc_err}"), "stdin"]));
        let err_out = sb.run(&["doctor", "--json"]);
        let err_report: Value = serde_json::from_slice(&err_out.stdout).unwrap();
        assert_eq!(err_out.status.code(), Some(1), "{err_report}");
        assert!(
            find_check(&err_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("returned error"),
            "{err_report}"
        );

        // 5. Missing `tools` array in tools/list response.
        let bad_list = json!({"jsonrpc":"2.0","id":2,"result":{}}).to_string();
        set_stub_args(json!([
            format!("out:{init_ok}"),
            format!("out:{bad_list}"),
            "stdin"
        ]));
        let bad_list_out = sb.run(&["doctor", "--json"]);
        let bad_list_report: Value = serde_json::from_slice(&bad_list_out.stdout).unwrap();
        assert_eq!(bad_list_out.status.code(), Some(1), "{bad_list_report}");
        assert!(
            find_check(&bad_list_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("tools array"),
            "{bad_list_report}"
        );

        // 6a. An already-expired deadline stops before spawning the handshake child.
        let never_file = sb.root.join("never.lock");
        set_stub_args(json!([format!("hold:{}", never_file.display())]));
        let mut zero_cmd = sb.command(&["doctor", "--json"]);
        zero_cmd.env("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS", "0");
        let zero_out = zero_cmd.output().unwrap();
        let zero_report: Value = serde_json::from_slice(&zero_out.stdout).unwrap();
        assert_eq!(zero_out.status.code(), Some(1), "{zero_report}");
        assert!(
            find_check(&zero_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("deadline"),
            "{zero_report}"
        );
        assert!(
            !never_file.exists(),
            "expired handshake deadline must skip spawn: {zero_report}"
        );

        // 6b. Hung server hits deadline and is killed and reaped.
        let killed_prof = KilledProfileDir::new();
        let lock_file = sb.root.join("hung.lock");
        set_stub_args(json!([
            format!("hold:{}", lock_file.display()),
            "sleep:60000"
        ]));
        let started = Instant::now();
        let mut cmd = sb.command(&["doctor", "--json"]);
        // Deliberately kills an instrumented child after a short deadline; redirect its profile
        // outside target/llvm-cov-target so a kill during startup or exit flush cannot leave a
        // corrupt .profraw on macOS. Doctor's timeout and stop paths remain covered by 6c below.
        cmd.env("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS", "250")
            .env("LLVM_PROFILE_FILE", killed_prof.pattern());
        let timed_out = cmd.output().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "doctor waited past handshake deadline: {:?}",
            started.elapsed()
        );
        let timed_report: Value = serde_json::from_slice(&timed_out.stdout).unwrap();
        assert_eq!(timed_out.status.code(), Some(1), "{timed_report}");
        assert!(
            find_check(&timed_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("deadline"),
            "{timed_report}"
        );
        // If the child held the lock, verify it was killed and the lock is now free.
        if lock_file.exists() {
            let file = fs::File::options().write(true).open(&lock_file).unwrap();
            file.try_lock()
                .expect("hung handshake child was killed and released its lock");
        }

        // 6c. Handshake spawns under D6 platform isolation so descendants (grandchild ignoring
        // SIGTERM on Unix / inside Job Object on Windows) are killed too when the deadline hits.
        // The parent exits cleanly after waiting for `grandchild.lock.ready`, leaving only the
        // silent grandchild holding stdout open until the deadline fires. Placing `gc_lock` in
        // `killed_prof` keeps the killed grandchild's `.profraw` outside `target/llvm-cov-target`
        // while doctor and the cleanly-exiting parent still record coverage normally.
        let gc_lock = killed_prof.0.join("grandchild.lock");
        set_stub_args(json!([
            format!("quiet-grandchild:{}", gc_lock.display()),
            "exit:0"
        ]));
        let gc_started = Instant::now();
        let mut gc_cmd = sb.command(&["doctor", "--json"]);
        gc_cmd.env("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS", "1500");
        let gc_out = gc_cmd.output().unwrap();
        assert!(
            gc_started.elapsed() < Duration::from_secs(10),
            "doctor waited on grandchild: {:?}",
            gc_started.elapsed()
        );
        let gc_report: Value = serde_json::from_slice(&gc_out.stdout).unwrap();
        assert_eq!(gc_out.status.code(), Some(1), "{gc_report}");
        assert!(
            find_check(&gc_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("deadline"),
            "{gc_report}"
        );
        assert!(
            killed_prof.0.join("grandchild.lock.ready").exists(),
            "grandchild never started: {gc_report}"
        );
        let gc_file = fs::File::options().write(true).open(&gc_lock).unwrap();
        let lock_deadline = Instant::now() + Duration::from_secs(2);
        while gc_file.try_lock().is_err() {
            assert!(
                Instant::now() < lock_deadline,
                "grandchild survived handshake cleanup: {gc_report}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        // 6d. Server that answers tools/list cleanly but ignores stdin EOF is bounded by the
        // handshake deadline during the clean-exit wait and then stopped via D6 stop + finish.
        let eof_hang_lock = sb.root.join("eof-hang.lock");
        set_stub_args(json!([
            format!("out:{init_ok}"),
            format!("out:{list_one}"),
            format!("hold:{}", eof_hang_lock.display()),
            "sleep:60000"
        ]));
        let eof_hang_started = Instant::now();
        let mut eof_hang_cmd = sb.command(&["doctor", "--json"]);
        // Deliberately kills an instrumented child that ignores stdin EOF; keep its profile out
        // of target/llvm-cov-target for the same reason as 6b.
        eof_hang_cmd
            .env("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS", "400")
            .env("LLVM_PROFILE_FILE", killed_prof.pattern());
        let eof_hang_out = eof_hang_cmd.output().unwrap();
        assert!(
            eof_hang_started.elapsed() < Duration::from_secs(10),
            "doctor waited indefinitely on server ignoring EOF: {:?}",
            eof_hang_started.elapsed()
        );
        let eof_hang_report: Value = serde_json::from_slice(&eof_hang_out.stdout).unwrap();
        assert!(eof_hang_out.status.success(), "{eof_hang_report}");
        assert_eq!(
            find_check(&eof_hang_report, "claude-code.handshake")["status"],
            "ok",
            "{eof_hang_report}"
        );
        if eof_hang_lock.exists() {
            let file = fs::File::options()
                .write(true)
                .open(&eof_hang_lock)
                .unwrap();
            file.try_lock()
                .expect("server ignoring EOF was stopped and released its lock");
        }

        // 7. Response line larger than 1 MiB fails handshake cleanly.
        set_stub_args(json!(["flood:1100000", "stdin"]));
        let flood_out = sb.run(&["doctor", "--json"]);
        let flood_report: Value = serde_json::from_slice(&flood_out.stdout).unwrap();
        assert_eq!(flood_out.status.code(), Some(1), "{flood_report}");
        assert!(
            find_check(&flood_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("exceeded 1048576 bytes"),
            "{flood_report}"
        );

        // 8a. Cleanup failure after a valid exchange turns the handshake into a failure.
        set_stub_args(json!([
            format!("out:{init_ok}"),
            format!("out:{list_one}"),
            "stdin"
        ]));
        let mut cleanup_ok_cmd = sb.command(&["doctor", "--json"]);
        cleanup_ok_cmd.env("PROXENOS_TEST_FAIL_HANDSHAKE_CLEANUP", "1");
        let cleanup_ok_out = cleanup_ok_cmd.output().unwrap();
        let cleanup_ok_report: Value = serde_json::from_slice(&cleanup_ok_out.stdout).unwrap();
        assert_eq!(cleanup_ok_out.status.code(), Some(1), "{cleanup_ok_report}");
        assert_eq!(cleanup_ok_report["ok"], false, "{cleanup_ok_report}");
        let cleanup_ok_hs = find_check(&cleanup_ok_report, "claude-code.handshake");
        assert_eq!(cleanup_ok_hs["status"], "fail", "{cleanup_ok_report}");
        let cleanup_ok_msg = cleanup_ok_hs["message"].as_str().unwrap();
        assert!(
            cleanup_ok_msg.contains("cleanup failed")
                && cleanup_ok_msg.contains("may still be running"),
            "{cleanup_ok_report}"
        );

        // 8b. Cleanup failure after an already-failed exchange preserves the original error and
        // appends the cleanup failure.
        set_stub_args(json!(["out:not-json", "stdin"]));
        let mut cleanup_err_cmd = sb.command(&["doctor", "--json"]);
        cleanup_err_cmd.env("PROXENOS_TEST_FAIL_HANDSHAKE_CLEANUP", "1");
        let cleanup_err_out = cleanup_err_cmd.output().unwrap();
        let cleanup_err_report: Value = serde_json::from_slice(&cleanup_err_out.stdout).unwrap();
        assert_eq!(
            cleanup_err_out.status.code(),
            Some(1),
            "{cleanup_err_report}"
        );
        assert_eq!(cleanup_err_report["ok"], false, "{cleanup_err_report}");
        let cleanup_err_hs = find_check(&cleanup_err_report, "claude-code.handshake");
        assert_eq!(cleanup_err_hs["status"], "fail", "{cleanup_err_report}");
        let cleanup_err_msg = cleanup_err_hs["message"].as_str().unwrap();
        assert!(
            cleanup_err_msg.contains("invalid JSON")
                && cleanup_err_msg.contains("cleanup also failed")
                && cleanup_err_msg.contains("may still be running"),
            "{cleanup_err_report}"
        );
    }

    #[test]
    fn failed_final_manifest_commit_is_removable_and_recoverable() {
        // Variant A: final manifest commit fails -> --remove still recognizes pending manifest and
        // cleans up settings, plugin files, backup, manifest, and empty dirs.
        let sb = Sandbox::new();
        fs::create_dir_all(sb.settings_path().parent().unwrap()).unwrap();
        fs::write(sb.settings_path(), "{\n  \"keep\": 1\n}\n").unwrap();

        let mut fail_cmd = sb.command(&["integrate", "claude-code"]);
        fail_cmd.env("PROXENOS_TEST_FAIL_FINAL_MANIFEST", "1");
        let failed = fail_cmd.output().unwrap();
        assert_eq!(failed.status.code(), Some(1));
        assert_eq!(
            read_json(&sb.manifest_path())["clients"]["claude-code"]["pending"],
            true
        );

        let rm = sb.run(&["integrate", "claude-code", "--remove"]);
        assert!(
            rm.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&rm.stderr)
        );
        let after_rm = read_json(&sb.settings_path());
        assert_eq!(after_rm, json!({ "keep": 1 }));
        assert!(!sb.marketplace_dir().exists());
        assert!(!sb.manifest_path().exists());
        assert!(!sb.backup_path().exists());

        // Variant B: final manifest commit fails -> subsequent integrate recognizes pending
        // manifest as ours (not foreign) and commits pending = false.
        let sb2 = Sandbox::new();
        let mut fail_cmd2 = sb2.command(&["integrate", "claude-code"]);
        fail_cmd2.env("PROXENOS_TEST_FAIL_FINAL_MANIFEST", "1");
        assert_eq!(fail_cmd2.output().unwrap().status.code(), Some(1));
        assert_eq!(
            read_json(&sb2.manifest_path())["clients"]["claude-code"]["pending"],
            true
        );

        let recover = sb2.run(&["integrate", "claude-code"]);
        assert!(
            recover.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&recover.stderr)
        );
        let manifest2 = read_json(&sb2.manifest_path());
        assert!(
            manifest2["clients"]["claude-code"].get("pending").is_none(),
            "pending flag should be cleared after recovery: {manifest2}"
        );
        let doc = sb2.run(&["doctor", "--json"]);
        let report: Value = serde_json::from_slice(&doc.stdout).unwrap();
        assert!(doc.status.success(), "{report}");
    }

    #[cfg(windows)]
    #[test]
    fn doctor_fails_on_unsupported_windows_batch_launcher() {
        let sb = Sandbox::new();
        let bin_dir = sb.root.join("stub-bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("codex.cmd"), "rem unsupported wrapper\r\n").unwrap();

        assert!(sb.run(&["integrate", "claude-code"]).status.success());

        let mut cmd = sb.command(&["doctor", "--json"]);
        cmd.env("PATH", &bin_dir);
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(find_check(&report, "cli.codex")["status"], "fail");
        assert!(
            find_check(&report, "cli.codex")["message"]
                .as_str()
                .unwrap()
                .contains("Cannot follow batch launcher")
        );
    }

    #[test]
    fn integrate_sessions_and_doctor_share_one_state_directory() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio;

        let sb = Sandbox::new();
        let bin_dir = sb.root.join("stub-bin");
        install_stub(&bin_dir, "codex");
        fs::write(
            sb.root.join("codex.version"),
            json!({"stdout": "codex 0.50.0\n"}).to_string(),
        )
        .unwrap();
        fs::write(
            sb.root.join("codex.steps"),
            json!([{"stdout": include_str!("fixtures/codex/finished.jsonl")}]).to_string(),
        )
        .unwrap();
        let cwd = sb.root.join("workspace");
        fs::create_dir_all(&cwd).unwrap();

        #[cfg(all(unix, not(target_os = "macos")))]
        let (xdg_state, expected_state_dir) = {
            let xdg = sb.root.join("custom-xdg-state");
            (xdg.clone(), xdg.join("proxenos"))
        };
        #[cfg(target_os = "macos")]
        let (ignored_xdg, expected_state_dir) = (
            sb.root.join("ignored-xdg-state"),
            sb.home
                .join("Library")
                .join("Application Support")
                .join("proxenos"),
        );
        #[cfg(windows)]
        let expected_state_dir = sb.local_app_data.join("proxenos");

        let configure_env = |cmd: &mut Command| {
            cmd.env("PATH", &bin_dir).env("PROXENOS_STUB_DIR", &sb.root);
            #[cfg(all(unix, not(target_os = "macos")))]
            cmd.env("XDG_STATE_HOME", &xdg_state);
            #[cfg(target_os = "macos")]
            cmd.env("XDG_STATE_HOME", &ignored_xdg);
        };

        // 1. Run `proxenos integrate claude-code`.
        let mut int_cmd = sb.command(&["integrate", "claude-code"]);
        configure_env(&mut int_cmd);
        let int_out = int_cmd.output().unwrap();
        assert!(
            int_out.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&int_out.stdout),
            String::from_utf8_lossy(&int_out.stderr)
        );
        let manifest_path = expected_state_dir.join("integrate").join("manifest.json");
        assert!(
            manifest_path.is_file(),
            "manifest missing at {}",
            manifest_path.display()
        );

        // 2. Run one confirmed delegation start through `proxenos mcp --client claude`.
        let mut mcp_cmd = sb.command(&["mcp", "--client", "claude"]);
        configure_env(&mut mcp_cmd);
        mcp_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = mcp_cmd.spawn().expect("proxenos mcp spawns");
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        let send = |stdin: &mut std::process::ChildStdin, msg: &Value| {
            writeln!(stdin, "{msg}").unwrap();
            stdin.flush().unwrap();
        };
        let recv = |stdout: &mut BufReader<std::process::ChildStdout>, id: i64| -> Value {
            loop {
                let mut line = String::new();
                let n = stdout.read_line(&mut line).unwrap();
                assert!(n > 0, "proxenos mcp closed stdout before answering id {id}");
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame.get("id") == Some(&json!(id)) {
                    return frame;
                }
            }
        };

        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "integrate-test", "version": "0.1"}
                }
            }),
        );
        let init_resp = recv(&mut stdout, 1);
        assert!(init_resp.get("result").is_some(), "{init_resp}");
        send(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "codex",
                    "arguments": {
                        "prompt": "verify shared state dir",
                        "model": "test-model",
                        "cwd": cwd,
                        "timeout_ms": 5_000
                    }
                }
            }),
        );
        let call_resp = recv(&mut stdout, 2);
        drop(stdin);
        let mcp_output = child.wait_with_output().unwrap();
        assert!(
            mcp_output.status.success(),
            "mcp stderr: {}",
            String::from_utf8_lossy(&mcp_output.stderr)
        );
        let raw_envelope = call_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("unexpected tools/call response: {call_resp}"));
        let envelope: Value = serde_json::from_str(raw_envelope).unwrap();
        assert_eq!(envelope["observed"]["state"], "finished", "{envelope}");
        assert!(
            envelope["observed"]["session_save_failure"].is_null(),
            "{envelope}"
        );

        let sessions_dir = expected_state_dir.join("sessions").join("codex");
        let session_files: Vec<PathBuf> = fs::read_dir(&sessions_dir)
            .unwrap_or_else(|err| panic!("missing sessions dir {}: {err}", sessions_dir.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(
            session_files.len(),
            1,
            "expected 1 session file in {}",
            sessions_dir.display()
        );
        assert!(manifest_path.starts_with(&expected_state_dir));
        assert!(session_files[0].starts_with(&expected_state_dir));
        #[cfg(target_os = "macos")]
        assert!(
            !ignored_xdg.exists(),
            "macOS must ignore XDG_STATE_HOME: {}",
            ignored_xdg.display()
        );

        // 3. Run `proxenos doctor --json` and verify it reports the exact same proxenos directory.
        let mut doc_cmd = sb.command(&["doctor", "--json"]);
        configure_env(&mut doc_cmd);
        let doc_out = doc_cmd.output().unwrap();
        let report: Value = serde_json::from_slice(&doc_out.stdout).unwrap();
        assert!(
            doc_out.status.success(),
            "report: {report}\nstderr: {}",
            String::from_utf8_lossy(&doc_out.stderr)
        );
        assert_eq!(report["ok"], true, "{report}");
        let state_check = find_check(&report, "state_dir.permissions");
        assert_eq!(state_check["status"], "ok", "{report}");
        assert_eq!(
            state_check["details"]["path"],
            expected_state_dir.to_str().unwrap(),
            "{report}"
        );
    }
}
