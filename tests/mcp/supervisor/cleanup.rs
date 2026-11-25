use super::*;

#[test]
fn a_cli_may_remove_its_own_temporary_file() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let mut args = launch(json!([format!("remove:{}", temp.display()), "exit:17"]));
    args["temp_file"] = json!(temp);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["code"], 17);
    assert!(!temp.exists());
    assert!(server.finish().0.success());
    assert!(!server.stderr().contains("could not remove temporary file"));
}

#[test]
fn a_removed_input_remains_reserved_until_its_call_finishes() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let gate = directory.0.join("gate");
    let mut first = launch(json!([
        format!("remove:{}", temp.display()),
        format!("hold:{}", gate.display()),
        format!("until-released:{}", gate.display()),
        "exit:7",
    ]));
    first["temp_file"] = json!(temp);
    let mut owner = server();
    owner.start_call(1, "test_run", first);
    let child = lock_file(&gate);
    assert!(!is_free(&child));
    assert!(!temp.exists());

    let mut contender = server();
    let mut second = launch(json!([format!("exists:{}", temp.display()), "exit:8"]));
    second["temp_file"] = json!(directory.0.join(".").join("instructions"));
    let refused = contender.call(1, "test_run", second.clone());
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(!temp.exists());
    fs::write(gate.with_extension("release"), []).unwrap();
    assert_eq!(finished(&owner.response(1))["code"], 7);

    let result = finished(&contender.call(2, "test_run", second));
    assert_eq!(result["code"], 8);
    assert_eq!(result["stdout"], "exists=true\n");
    assert!(!temp.exists());
}

#[test]
fn cleanup_reports_the_path_and_keeps_the_child_result_on_failure() {
    let directory = Directory::new();
    let temp = directory.0.join("instructions");
    let entry = temp.join("entry");
    let mut args = launch(json!([
        format!("remove:{}", temp.display()),
        format!("mkdir:{}", temp.display()),
        format!("hold:{}", entry.display()),
        "out:completed",
        "exit:19",
    ]));
    args["temp_file"] = json!(temp);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["ending"], "exited");
    assert_eq!(result["code"], 19);
    assert_eq!(result["stdout"], "completed\n");
    assert!(
        entry.exists(),
        "cleanup must leave directory contents alone"
    );
    server.wait_for_stderr(&format!(
        "could not remove temporary file {}",
        temp.display()
    ));
}

#[cfg(windows)]
fn deny_deletion_before_exit(
    server: &mut Server,
    directory: &Directory,
    retry_ms: Option<u64>,
) -> (PathBuf, File, File) {
    use std::os::windows::fs::OpenOptionsExt;

    let temp = directory.0.join("instructions");
    let gate = directory.0.join("gate");
    let mut args = launch(json!([
        format!("hold:{}", gate.display()),
        format!("until-released:{}", gate.display()),
        "exit:21",
    ]));
    args["temp_file"] = json!(temp);
    if let Some(budget) = retry_ms {
        args["sharing_retry_ms"] = json!(budget);
    }
    server.start_call(1, "test_run", args);
    let child = lock_file(&gate);
    assert!(!is_free(&child));
    let deny_delete = File::options()
        .read(true)
        .share_mode(0)
        .open(&temp)
        .unwrap();
    (temp, child, deny_delete)
}

#[cfg(windows)]
#[test]
fn a_persistent_sharing_violation_has_a_bounded_retry_period() {
    let directory = Directory::new();
    let mut server = server();
    let (temp, child, deny_delete) = deny_deletion_before_exit(&mut server, &directory, None);
    let started = Instant::now();
    fs::write(directory.0.join("gate.release"), []).unwrap();
    let result = finished(&server.response(1));
    assert_eq!(result["code"], 21);
    assert!(is_free(&child));
    assert!(started.elapsed() >= Duration::from_secs(1));
    server.wait_for_stderr(&format!(
        "could not remove temporary file {}",
        temp.display()
    ));
    drop(deny_delete);
    assert!(temp.exists());
}

#[cfg(windows)]
#[test]
fn cleanup_retries_a_temporary_file_after_a_sharing_violation_clears() {
    let directory = Directory::new();
    let mut server = server();
    // The worker can keep trying past the client's entire bounded wait for the call's result.
    let (temp, child, deny_delete) =
        deny_deletion_before_exit(&mut server, &directory, Some(u64::MAX));
    fs::write(directory.0.join("gate.release"), []).unwrap();
    let result = finished(&server.response(1));
    assert_eq!(result["code"], 21);
    assert!(is_free(&child));
    assert!(temp.exists());
    drop(deny_delete);
    assert!(server.finish().0.success());
    assert!(!temp.exists(), "{}", server.stderr());
    assert!(!server.stderr().contains("could not remove temporary file"));
}
