//! Behavioural tests for `proxenos integrate claude-code [--remove]` and `proxenos doctor` (D10).

use std::fs;
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
        #[cfg(unix)]
        {
            self.home.join(".local").join("state").join("proxenos")
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

fn canonical_path_string(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap();
    #[cfg(windows)]
    {
        let raw = canonical.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return rest.to_owned();
        }
    }
    canonical.to_str().unwrap().to_owned()
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

    let expected_bin = canonical_path_string(proxenos_bin());
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
        canonical_path_string(&bin_a)
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
        canonical_path_string(&bin_b)
    );
    assert_eq!(
        read_json(&sb.manifest_path())["clients"]["claude-code"]["binary_path"],
        canonical_path_string(&bin_b)
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

    assert!(
        sb.run(&["integrate", "claude-code", "--remove"])
            .status
            .success()
    );
    let after_remove = read_json(&sb.settings_path());
    assert_eq!(after_remove, original_val);
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
            "omega"
        ]
    );
}

#[test]
fn paths_with_spaces_and_non_ascii_work_end_to_end() {
    let sb = Sandbox::with_name(&format!("proxenos space café ąčę {}", Uuid::new_v4()));
    let bin = sb.hard_link_binary("bin dir café ąčę");
    let custom_state = sb.root.join("state dir café ąčę");

    let run_with_env = |args: &[&str]| {
        let mut cmd = sb.command_for(&bin, args);
        #[cfg(unix)]
        cmd.env("XDG_STATE_HOME", &custom_state);
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

    let expected_marketplace = custom_state.join("proxenos").join("claude-marketplace");
    let settings = read_json(&sb.settings_path());
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
        canonical_path_string(&bin)
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
    assert!(
        find_check(&missing_report, "claude-code.registration")["message"]
            .as_str()
            .unwrap()
            .contains("does not exist")
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

    // Non-directory parent for XDG_STATE_HOME exercises non-NotFound symlink_metadata error.
    let file_parent = sb.root.join("not-a-dir");
    fs::write(&file_parent, "file").unwrap();
    let mut cmd = sb.command(&["doctor", "--json"]);
    cmd.env("XDG_STATE_HOME", &file_parent);
    let err_out = cmd.output().unwrap();
    assert_eq!(err_out.status.code(), Some(1));
    let err_report: Value = serde_json::from_slice(&err_out.stdout).unwrap();
    assert_eq!(
        find_check(&err_report, "state_dir.permissions")["status"],
        "fail"
    );
}

#[cfg(feature = "test-tools")]
mod with_stubs {
    use super::*;
    use std::time::{Duration, Instant};

    fn stub_bin() -> &'static Path {
        Path::new(env!("CARGO_BIN_EXE_proxenos-stub"))
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
        assert_eq!(report["ok"], true);
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
        let tools: Vec<&str> = find_check(&report, "claude-code.handshake")["details"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        for expected in ["codex", "codex-reply", "antigravity", "antigravity-reply"] {
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
        assert_eq!(out.status.code(), Some(1));
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(find_check(&report, "cli.codex")["status"], "fail");
        assert_eq!(find_check(&report, "cli.antigravity")["status"], "warn");
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
            format!("out:{list_empty}")
        ]));
        let empty_out = sb.run(&["doctor", "--json"]);
        assert!(empty_out.status.success());
        let empty_report: Value = serde_json::from_slice(&empty_out.stdout).unwrap();
        assert_eq!(
            find_check(&empty_report, "claude-code.handshake")["status"],
            "warn"
        );

        // 2. Closed stdout before answering.
        set_stub_args(json!(["exit:0"]));
        let eof_out = sb.run(&["doctor", "--json"]);
        assert_eq!(eof_out.status.code(), Some(1));
        let eof_report: Value = serde_json::from_slice(&eof_out.stdout).unwrap();
        assert!(
            find_check(&eof_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("closed stdout")
        );

        // 3. Invalid JSON on stdout.
        set_stub_args(json!(["out:not-json"]));
        let bad_json_out = sb.run(&["doctor", "--json"]);
        assert_eq!(bad_json_out.status.code(), Some(1));
        let bad_json_report: Value = serde_json::from_slice(&bad_json_out.stdout).unwrap();
        assert!(
            find_check(&bad_json_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("invalid JSON")
        );

        // 4. JSON-RPC error response.
        let rpc_err =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad"}}).to_string();
        set_stub_args(json!([format!("out:{rpc_err}")]));
        let err_out = sb.run(&["doctor", "--json"]);
        assert_eq!(err_out.status.code(), Some(1));
        let err_report: Value = serde_json::from_slice(&err_out.stdout).unwrap();
        assert!(
            find_check(&err_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("returned error")
        );

        // 5. Missing `tools` array in tools/list response.
        let bad_list = json!({"jsonrpc":"2.0","id":2,"result":{}}).to_string();
        set_stub_args(json!([format!("out:{init_ok}"), format!("out:{bad_list}")]));
        let bad_list_out = sb.run(&["doctor", "--json"]);
        assert_eq!(bad_list_out.status.code(), Some(1));
        let bad_list_report: Value = serde_json::from_slice(&bad_list_out.stdout).unwrap();
        assert!(
            find_check(&bad_list_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("tools array")
        );

        // 6. Hung server hits deadline and is killed and reaped.
        let lock_file = sb.root.join("hung.lock");
        set_stub_args(json!([
            format!("hold:{}", lock_file.display()),
            "sleep:60000"
        ]));
        let started = Instant::now();
        let mut cmd = sb.command(&["doctor", "--json"]);
        cmd.env("PROXENOS_TEST_HANDSHAKE_TIMEOUT_MS", "250");
        let timed_out = cmd.output().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "doctor waited past handshake deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(timed_out.status.code(), Some(1));
        let timed_report: Value = serde_json::from_slice(&timed_out.stdout).unwrap();
        assert!(
            find_check(&timed_report, "claude-code.handshake")["message"]
                .as_str()
                .unwrap()
                .contains("deadline")
        );
        // If the child held the lock, verify it was killed and the lock is now free.
        if lock_file.exists() {
            let file = fs::File::options().write(true).open(&lock_file).unwrap();
            file.try_lock()
                .expect("hung handshake child was killed and released its lock");
        }
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
}
