use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::provider_support::{Fleet, call, turn};

fn file(result: &Value) -> PathBuf {
    PathBuf::from(result["observed"]["content_file"].as_str().unwrap())
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
}

#[test]
fn the_archive_bounds_disk_use_and_preserves_the_turn_outcome_on_eviction_failure() {
    let fleet = Fleet::new(&["agy"]);
    let long = "a".repeat((16 << 10) + 1);
    let ok = turn(&long);
    let failed = json!({"status":"ERROR","error":"Quota exceeded","response":long});
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
    let second = file(&run());
    let directory = first.parent().unwrap();
    for _ in 2..8 {
        assert!(file(&run()).is_file());
    }
    assert_eq!(fs::read_dir(directory).unwrap().count(), 8);
    fs::remove_file(&first).unwrap(); // A client may remove a captured answer after reading it.
    assert!(file(&run()).is_file());
    fs::remove_file(&second).unwrap();
    fs::create_dir(&second).unwrap(); // Cannot remove this entry as a regular file.
    let failed_capture = run();
    assert_eq!(failed_capture["observed"]["failure"]["kind"], "capture");
    assert!(failed_capture["observed"]["content_file"].is_null());
    assert_eq!(
        failed_capture["reported"]["content"]
            .as_str()
            .unwrap()
            .len(),
        16 << 10
    );
    let failed_turn = run();
    assert_eq!(failed_turn["observed"]["failure"]["kind"], "abnormal_exit");
    assert_eq!(failed_turn["reported"]["failure"]["kind"], "quota");
    assert_eq!(failed_turn["observed"]["capture_error"]["kind"], "capture");
    fs::remove_dir(&second).unwrap();
    fs::write(&second, []).unwrap();
    assert!(file(&run()).is_file());
    assert!(!second.exists());
    assert_eq!(fs::read_dir(directory).unwrap().count(), 8);
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
}
