use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::common::Server;
use crate::provider_support::{
    Fleet, assert_observed_failure, assert_reported_failure, call, turn,
};

fn file(result: &Value) -> PathBuf {
    PathBuf::from(
        result["observed"]["content_file"]
            .as_str()
            .unwrap_or_else(|| panic!("missing observed.content_file in envelope: {result}")),
    )
}

fn failure_file(result: &Value) -> PathBuf {
    PathBuf::from(
        result["observed"]["failure_file"]
            .as_str()
            .unwrap_or_else(|| panic!("missing observed.failure_file in envelope: {result}")),
    )
}

#[test]
fn the_excerpt_preserves_utf8_and_the_private_answer_outlives_child_cleanup() {
    let fleet = Fleet::new(&["agy"]);
    let prefix = "a".repeat((16 << 10) - 1);
    let answer = format!("{prefix}風 and a second line\n");
    fleet.script("agy", json!([turn(&answer)]));
    let mut server = fleet.server("claude");
    let result = call(
        &mut server,
        "antigravity",
        json!({"prompt":"Answer at length."}),
    );
    assert_eq!(result["observed"]["state"], "finished");
    assert_eq!(result["reported"]["content"], prefix);
    assert_eq!(result["observed"]["content_file_bytes"], answer.len());
    let retained = file(&result);
    let directory = retained.parent().unwrap();
    assert!(retained.is_absolute());
    assert_eq!(fs::read_to_string(&retained).unwrap(), answer);
    let records = fleet.records();
    let args = records.last().unwrap()["args"].as_array().unwrap();
    let log = args
        .windows(2)
        .find(|pair| pair[0] == "--log-file")
        .unwrap();
    let scratch = Path::new(log[1].as_str().unwrap());
    assert_ne!(scratch.parent(), Some(directory));
    assert!(!scratch.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&retained, &directory.to_owned()] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }
    assert!(server.finish().0.success());
    assert!(!directory.exists());
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn the_archive_bounds_disk_use_and_preserves_the_turn_outcome_on_eviction_failure() {
    let fleet = Fleet::new(&["agy"]);
    let long = "a".repeat((16 << 10) + 1);
    let ok = turn(&long);
    let failed = json!({"status":"ERROR","error":format!("Quota exceeded: {long}"),"response":""});
    let mut steps = vec![ok.clone(); 10];
    steps.extend([json!({"stdout":failed.to_string(), "code":1}), ok]);
    fleet.script("agy", Value::Array(steps));
    let mut server = fleet.server("claude");
    let mut run = || {
        call(
            &mut server,
            "antigravity",
            json!({"prompt":"Save an answer."}),
        )
    };
    let first = file(&run());
    fleet.assert_steps_remaining("agy", 11);
    let second = file(&run());
    fleet.assert_steps_remaining("agy", 10);
    let directory = first.parent().unwrap();
    for i in 2..8 {
        assert!(file(&run()).is_file());
        fleet.assert_steps_remaining("agy", 11 - i);
    }
    assert_eq!(fs::read_dir(directory).unwrap().count(), 8);
    fs::remove_file(&first).unwrap(); // A client may remove a captured answer after reading it.
    assert!(file(&run()).is_file());
    fleet.assert_steps_remaining("agy", 3);
    fs::remove_file(&second).unwrap();
    fs::create_dir(&second).unwrap(); // Cannot remove this entry as a regular file.
    let failed_capture = run();
    assert_observed_failure(
        &failed_capture,
        "capture",
        "Could not retain the captured content",
    );
    assert!(failed_capture["observed"]["content_file"].is_null());
    assert_eq!(
        failed_capture["reported"]["content"]
            .as_str()
            .unwrap()
            .len(),
        16 << 10
    );
    fleet.assert_steps_remaining("agy", 2);
    let failed_turn = run();
    assert_reported_failure(
        &failed_turn,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        "Quota exceeded",
    );
    assert_eq!(failed_turn["observed"]["capture_error"]["kind"], "capture");
    assert!(
        failed_turn["observed"]["capture_error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("missing capture_error message: {failed_turn}"))
            .contains("Could not retain the captured failure"),
        "{failed_turn}"
    );
    fleet.assert_steps_remaining("agy", 1);
    fs::remove_dir(&second).unwrap();
    fs::write(&second, []).unwrap();
    assert!(file(&run()).is_file());
    assert!(!second.exists());
    assert_eq!(fs::read_dir(directory).unwrap().count(), 8);
    fleet.assert_steps_remaining("agy", 0);
    let turns = fleet
        .records()
        .into_iter()
        .filter(|record| record["args"] != json!(["--version"]))
        .count();
    assert_eq!(turns, 12);
}

#[test]
fn the_file_carries_only_what_was_captured_from_the_final_message() {
    let fleet = Fleet::new(&["codex"]);
    let limit = 4 << 20;
    let answer = "z".repeat(limit + 35);
    fleet.script("codex", json!([{
        "stdout":"{\"type\":\"thread.started\",\"thread_id\":\"long-turn\"}\n{\"type\":\"turn.completed\"}\n",
        "final_message":answer
    }]));
    let mut server = fleet.server("claude");
    let result = call(
        &mut server,
        "codex",
        json!({"prompt":"Return the complete record."}),
    );
    assert_eq!(result["observed"]["state"], "finished", "{result}");
    let attempt = &result["observed"]["attempts"][0];
    assert_eq!(attempt["provider_file_bytes"], answer.len());
    assert_eq!(attempt["provider_file_captured"], limit);
    assert_eq!(result["observed"]["content_captured_bytes"], limit);
    assert_eq!(result["observed"]["content_file_bytes"], limit);
    assert_eq!(fs::read_to_string(file(&result)).unwrap(), answer[..limit]);
    fleet.assert_steps_remaining("codex", 0);
}

#[test]
fn long_provider_failure_messages_and_reset_times_are_bounded_and_archived() {
    let fleet = Fleet::new(&["agy"]);
    let lead = "Quota exhausted. Resets at ";
    let filler = "e".repeat((16 << 10) - lead.len() - 1);
    let prefix = format!("{lead}{filler}");
    let full_error = format!("{prefix}風 trailing failure detail\n");
    let failed = json!({"status": "ERROR", "response": "", "error": full_error});
    fleet.script("agy", json!([{"stdout": failed.to_string(), "code": 1}]));
    let mut server = fleet.server("claude");
    let result = call(&mut server, "antigravity", json!({"prompt": "fail long"}));
    assert_reported_failure(
        &result,
        Some(("abnormal_exit", "exit code 1")),
        "quota",
        lead,
    );
    assert_eq!(result["reported"]["failure"]["message"], prefix);
    assert_eq!(
        result["observed"]["failure_captured_bytes"],
        full_error.len()
    );
    assert_eq!(result["observed"]["failure_file_bytes"], full_error.len());
    let reset = result["reported"]["failure"]["reset_at"]
        .as_str()
        .unwrap_or_else(|| panic!("missing reset_at in envelope: {result}"));
    assert!(
        reset.len() <= 16 << 10,
        "reset_at was unbounded: {}",
        reset.len()
    );
    let retained = failure_file(&result);
    assert!(retained.is_absolute());
    assert_eq!(fs::read_to_string(&retained).unwrap(), full_error);
    let directory = retained.parent().unwrap();
    assert!(server.finish().0.success());
    assert!(!directory.exists());
    fleet.assert_steps_remaining("agy", 0);
}

#[test]
fn relative_tmpdir_still_produces_absolute_scratch_and_archive_paths() {
    let fleet = Fleet::new(&["agy"]);
    let rel_tmp = fleet.root.join("rel-tmp");
    let work = fleet.root.join("work");
    fs::create_dir(&rel_tmp).unwrap();
    fs::create_dir(&work).unwrap();
    let long = "b".repeat((16 << 10) + 32);
    fleet.script("agy", json!([turn(&long)]));
    let mut command = fleet.command("claude");
    for key in ["TMPDIR", "TMP", "TEMP"] {
        command.env(key, "rel-tmp");
    }
    let mut server = Server::start_command(command);
    server.initialize("2025-11-25");
    let result = call(
        &mut server,
        "antigravity",
        json!({"prompt": "Answer.", "cwd": work}),
    );
    assert_eq!(result["observed"]["state"], "finished", "{result}");
    let retained = file(&result);
    assert!(retained.is_absolute(), "{result}");
    assert!(
        retained
            .canonicalize()
            .unwrap()
            .starts_with(rel_tmp.canonicalize().unwrap()),
        "archive path escaped relative temp root: {}",
        retained.display()
    );
    let records = fleet.records();
    let args = records.last().unwrap()["args"].as_array().unwrap();
    let log = Path::new(
        args.windows(2)
            .find(|pair| pair[0] == "--log-file")
            .unwrap()[1]
            .as_str()
            .unwrap(),
    );
    assert!(
        log.is_absolute(),
        "log path was relative: {}",
        log.display()
    );
    assert!(
        log.parent()
            .unwrap()
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            == rel_tmp.canonicalize().unwrap()
    );
    fleet.assert_steps_remaining("agy", 0);
}
