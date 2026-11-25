//! Discovery observes PATH at first use and runs probes under D6 (D1, D7).

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Server, WAIT, text};
use crate::provider_support::{Fleet, call, turn};

fn install(file: &Path) {
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let stub = env!("CARGO_BIN_EXE_proxenos-stub");
    fs::hard_link(stub, file)
        .or_else(|_| fs::copy(stub, file).map(drop))
        .unwrap();
}

fn listed(server: &mut Server) -> Vec<String> {
    let response = server.request(12, "tools/list", json!({}));
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .filter(|name| !name.starts_with("test_"))
        .map(str::to_owned)
        .collect()
}

fn version(fleet: &Fleet, cli: &str, step: Value) {
    fs::write(fleet.root.join(format!("{cli}.version")), step.to_string()).unwrap();
}

#[test]
fn first_list_freezes_discovery_and_omits_both_tools_of_the_client() {
    let fleet = Fleet::new(&["codex", "agy"]);
    for (client, visible, cli) in [
        ("codex", "antigravity", "agy"),
        ("antigravity", "codex", "codex"),
    ] {
        let probes = fleet.records().len();
        let mut server = fleet.server(client);
        assert_eq!(fleet.records().len(), probes, "handshake started discovery");
        let names = listed(&mut server);
        assert_eq!(names, [visible.to_owned(), format!("{visible}-reply")]);
        assert_eq!(listed(&mut server), names);
        assert_eq!(fleet.records().len(), probes + 1);
        let record = &fleet.records()[probes];
        assert_eq!(record["cli"], cli);
        assert_eq!(record["args"], json!(["--version"]));
        let denied = server.call(13, client, json!({"prompt":"own provider"}));
        assert_eq!(denied["error"]["code"], -32602);
        assert_eq!(fleet.records().len(), probes + 1);
    }
}

#[test]
fn absent_at_discovery_stays_absent_after_install_until_the_server_restarts() {
    let fleet = Fleet::new(&[]);
    let path = fleet
        .root
        .join("bin")
        .join(format!("codex{}", std::env::consts::EXE_SUFFIX));
    let mut server = fleet.server("claude");
    assert!(listed(&mut server).is_empty());
    install(&path);
    assert!(listed(&mut server).is_empty());
    let absent = call(&mut server, "codex", json!({"prompt":"before restart"}));
    assert_eq!(absent["observed"]["failure"]["kind"], "not_found");
    assert!(fleet.records().is_empty());
    let mut restarted = fleet.server("claude");
    assert_eq!(listed(&mut restarted), ["codex", "codex-reply"]);
}

#[test]
fn failed_version_check_stays_listed_and_recovers_on_call() {
    let fleet = Fleet::new(&["agy"]);
    version(
        &fleet,
        "agy",
        json!({"stderr":"stub initialization failed","code":17}),
    );
    let mut server = fleet.server("claude");
    assert_eq!(listed(&mut server), ["antigravity", "antigravity-reply"]);
    let failed = call(&mut server, "antigravity", json!({"prompt":"first"}));
    let failure = &failed["observed"]["failure"];
    assert_eq!(failure["kind"], "launch");
    assert!(
        failure["message"]
            .as_str()
            .unwrap()
            .contains("stub initialization failed")
    );
    assert_eq!(fleet.records().len(), 2, "list and call each probed once");

    version(
        &fleet,
        "agy",
        json!({"stdout":"version unavailable","code":1}),
    );
    let failed = call(&mut server, "antigravity", json!({"prompt":"again"}));
    assert!(
        failed["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("version unavailable")
    );
    fs::remove_file(fleet.root.join("agy.version")).unwrap();
    fleet.script("agy", json!([turn("answer")]));
    let success = call(&mut server, "antigravity", json!({"prompt":"after repair"}));
    assert_eq!(success["reported"]["content"], "answer");
    assert_eq!(
        fleet.records().len(),
        5,
        "three failures, one working probe, one turn"
    );
}

#[test]
fn version_probe_timeout_reaches_the_call_as_a_typed_failure() {
    let fleet = Fleet::new(&["codex"]);
    version(&fleet, "codex", json!({"sleep_ms":60000}));
    let mut server = fleet.server("claude");
    server.start_call(17, "codex", json!({"prompt":"deadline"}));
    let frame = server
        .next_frame(Instant::now() + Duration::from_secs(30))
        .unwrap();
    assert_eq!(frame["id"], 17);
    let failed: Value = serde_json::from_str(text(&frame)).unwrap();
    assert_eq!(failed["observed"]["failure"]["kind"], "timeout", "{failed}");
    assert_eq!(fleet.records().len(), 1);
}

#[test]
fn cancelling_a_list_stops_parallel_probes_but_the_waiting_call_rechecks() {
    let fleet = Fleet::new(&["codex", "agy"]);
    for cli in ["codex", "agy"] {
        version(&fleet, cli, json!({"sleep_ms":60000}));
    }
    let mut server = fleet.server("claude");
    server.send(&json!({"jsonrpc":"2.0","id":22,"method":"tools/list","params":{}}));
    let until = Instant::now() + WAIT;
    while fs::read(fleet.root.join("invocations"))
        .unwrap_or_default()
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        != 2
    {
        assert!(Instant::now() < until, "both probes should be running");
        std::thread::sleep(Duration::from_millis(10));
    }
    server.start_call(
        24,
        "antigravity",
        json!({"prompt":"waiting on the same probes"}),
    );
    assert_eq!(text(&server.call(25, "test_echo", json!({}))), "{}");
    server.send(
        &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":22}}),
    );
    // The independent call's own version check times out. On Unix both the cancelled
    // group and this timed-out group receive D6's three-second grace period.
    let reply = server
        .next_frame(Instant::now() + Duration::from_secs(30))
        .unwrap();
    assert_eq!(reply["id"], 24);
    let answer: Value = serde_json::from_str(text(&reply)).unwrap();
    assert_eq!(answer["observed"]["failure"]["kind"], "timeout", "{answer}");
    assert_eq!(listed(&mut server).len(), 4);
    let records = fleet.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records.last().unwrap()["cli"], "agy");
}

#[test]
fn closing_input_cancels_the_running_version_probe() {
    let fleet = Fleet::new(&["codex"]);
    version(&fleet, "codex", json!({"sleep_ms":60000}));
    let mut server = fleet.server("claude");
    server.start_call(3, "codex", json!({"prompt":"disconnect"}));
    let until = Instant::now() + WAIT;
    while fs::metadata(fleet.root.join("invocations")).map_or(true, |m| m.len() == 0) {
        assert!(Instant::now() < until, "probe did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    server.close_input();
    let reply = server.response(3);
    let envelope: Value = serde_json::from_str(text(&reply)).unwrap();
    assert_eq!(envelope["observed"]["failure"]["kind"], "cancellation");
    assert_eq!(envelope["observed"]["state"], "cancelled");
    assert_eq!(fleet.records().len(), 1);
}

#[test]
fn an_unlaunchable_first_path_match_wins_over_a_working_fallback() {
    let fleet = Fleet::new(&[]);
    let native = format!("agy{}", std::env::consts::EXE_SUFFIX);
    let broken = fleet.root.join("bin").join(&native);
    fs::write(&broken, []).unwrap();
    #[cfg(windows)]
    let fallback = fleet.root.join("agy/bin").join(&native);
    #[cfg(unix)]
    let fallback = fleet.root.join(".local/bin").join(&native);
    install(&fallback);
    let mut server = fleet.server("claude");
    let answer = call(&mut server, "antigravity", json!({"prompt":"path order"}));
    assert_eq!(answer["observed"]["failure"]["kind"], "launch");
    assert!(
        answer["observed"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("Version check")
    );
    assert!(fleet.records().is_empty());
    fs::remove_file(broken).unwrap();
    let mut fresh = fleet.server("claude");
    assert_eq!(listed(&mut fresh), ["antigravity", "antigravity-reply"]);
    assert_eq!(fleet.records().len(), 1);
}

#[cfg(windows)]
mod windows {
    use super::*;

    #[test]
    fn a_recognized_shim_in_the_first_directory_precedes_a_native_exe_in_the_second() {
        let fleet = Fleet::new(&[]);
        let bin = fleet.root.join("bin");
        let second = fleet.root.join("second");
        install(&second.join("codex.exe"));
        install(&bin.join("other/runner.exe"));
        fs::write(
            bin.join("codex.bat"),
            "@set \"loader=%~dp0\"\r\n@\"%loader%other\\runner.exe\" %*\r\n",
        )
        .unwrap();
        let mut command = fleet.command("claude");
        command.env("PATH", std::env::join_paths([&bin, &second]).unwrap());
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        assert_eq!(listed(&mut server), ["codex", "codex-reply"]);
        assert_eq!(fleet.records()[0]["cli"], "runner");
    }

    #[test]
    fn relative_appdata_does_not_select_a_program_from_the_working_directory() {
        let fleet = Fleet::new(&[]);
        install(&fleet.root.join("relative/agy/bin/agy.exe"));
        let mut command = fleet.command("claude");
        command.env("LOCALAPPDATA", "relative");
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        assert!(listed(&mut server).is_empty());
        assert!(fleet.records().is_empty());
    }

    #[test]
    fn opaque_and_ambiguous_launchers_are_listed_but_never_started() {
        let fleet = Fleet::new(&[]);
        let bin = fleet.root.join("bin");
        install(&bin.join("target.exe"));
        fs::write(bin.join("target.cmd"), "@echo wrong").unwrap();
        fs::write(bin.join("target.js"), []).unwrap();
        let sources: &[&[u8]] = &[
            b"\xff\x80",
            b"rem nothing to execute",
            b"relative.exe %*",
            b"echo \"%~dp0target.exe\" %*",
            b"echo \"%~dp0target.js\" %*",
            b"set _prog=other.exe\r\n\"%_prog%\" \"%dp0%target.js\" %*",
            b"echo>%dp0%\\node.exe \"%dp0%target.js\" %*",
            b"other.exe --required-policy \"%~dp0target.js\" %*",
            b"\"%~dp0target.exe\"extra %*",
            b"'missing quote %*",
            b"target.exe %*\r\ntarget.exe %*",
            b"\"%missing%target.exe\" %*",
            b"\"%~dp0target.cmd\" %*",
        ];
        for source in sources {
            fs::write(bin.join("agy.cmd"), source).unwrap();
            let mut server = fleet.server("claude");
            assert_eq!(listed(&mut server), ["antigravity", "antigravity-reply"]);
            let failed = call(&mut server, "antigravity", json!({"prompt":"unsupported"}));
            assert_eq!(
                failed["observed"]["failure"]["kind"],
                "unsupported_launcher"
            );
            assert!(
                failed["observed"]["failure"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Cannot follow batch launcher")
            );
            assert!(fleet.records().is_empty());
        }
    }

    #[test]
    fn javascript_shims_resolve_only_native_node_in_precedence_order() {
        let fleet = Fleet::new(&[]);
        let shim_dir = fleet.root.join("shim");
        let other_dir = fleet.root.join("other");
        let shim = shim_dir.join("codex.cmd");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        let script = shim_dir.join("loader.cjs");
        fs::write(&script, []).unwrap();
        fs::write(shim, "set \"_prog=node\"\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\" \"%dp0%loader.cjs\" %*").unwrap();
        let stable_node = fleet.root.join("Programs/nodejs/node.exe");
        let path_node = other_dir.join("node.exe");
        let sibling = shim_dir.join("node.exe");
        let custom = fleet.root.join("preferred.exe");
        let mut probes = 0;
        for choice in [
            None,
            Some(&stable_node),
            Some(&path_node),
            Some(&sibling),
            Some(&custom),
        ] {
            if let Some(file) = choice {
                install(file);
            }
            if choice == Some(&path_node) {
                fs::write(shim_dir.join("codex.cmd"), "node \"%dp0%loader.cjs\" %*").unwrap();
            }
            let mut command = fleet.command("claude");
            command.env_remove("PROXENOS_NODE_BIN").env(
                "PATH",
                std::env::join_paths([&other_dir, &shim_dir]).unwrap(),
            );
            // Windows supplies its own ProgramFiles to child processes. An invalid explicit
            // override exercises the missing-runtime diagnostic on hosts with Node installed.
            if choice.is_none() {
                command.env("PROXENOS_NODE_BIN", fleet.root.join("missing.exe"));
            }
            if choice == Some(&custom) {
                command.env("PROXENOS_NODE_BIN", &custom);
            }
            let mut server = Server::start_command(command);
            server.initialize("2025-11-25");
            assert_eq!(listed(&mut server), ["codex", "codex-reply"]);
            if let Some(expected) = choice {
                probes += 1;
                let records = fleet.records();
                assert_eq!(records.len(), probes);
                assert_eq!(
                    records.last().unwrap()["args"],
                    json!([script, "--version"])
                );
                assert_eq!(
                    records.last().unwrap()["cli"],
                    expected.file_stem().unwrap().to_str().unwrap()
                );
                // Replace each lower-priority binary with an invalid image. A higher rung must
                // reach the stub even while the lower rungs still look like installed runtimes.
                fs::remove_file(expected).unwrap();
                fs::write(expected, []).unwrap();
            } else {
                let err = call(&mut server, "codex", json!({"prompt":"missing node"}));
                assert!(
                    err["observed"]["failure"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("PROXENOS_NODE_BIN"),
                    "{err}"
                );
                assert!(fleet.records().is_empty());
            }
        }
    }

    #[test]
    fn a_powershell_shim_passes_a_file_argument_to_an_absolute_runtime() {
        let fleet = Fleet::new(&[]);
        let bin = fleet.root.join("bin");
        let script = bin.join("agy.ps1");
        fs::write(&script, []).unwrap();
        fs::write(
            bin.join("agy.cmd"),
            format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" %*",
                script.display()
            ),
        )
        .unwrap();
        install(
            &fleet
                .root
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        );
        let mut command = fleet.command("claude");
        command.env("SystemRoot", &fleet.root);
        let mut server = Server::start_command(command);
        server.initialize("2025-11-25");
        assert_eq!(listed(&mut server), ["antigravity", "antigravity-reply"]);
        let record = &fleet.records()[0];
        assert_eq!(record["cli"], "powershell");
        assert_eq!(
            record["args"],
            json!([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script,
                "--version"
            ])
        );
    }
}
