use super::*;

#[test]
fn an_expired_launch_skips_the_cli_and_removes_its_input_file() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let marker = directory.0.join("started");
    let mut args = launch(json!([format!("hold:{}", marker.display())]));
    args["temp_file"] = json!(input);
    args["deadline_ms"] = json!(0);
    let mut server = server();
    let result = finished(&server.call(1, "test_run", args));
    assert_eq!(result["ending"], "timed_out");
    assert!(result["code"].is_null() && result["signal"].is_null());
    assert!(!marker.exists());
    assert!(!input.exists());
}

#[test]
fn a_call_cancelled_before_spawn_cleans_up_without_running_the_cli() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let marker = directory.0.join("started");
    let mut args = launch(json!([format!("hold:{}", marker.display())]));
    args["temp_file"] = json!(input);
    args["on_cancel"] = json!("start");
    let mut server = server();
    server.start_call(1, "test_run", args);
    wait_for("call did not prepare its file", || input.exists());
    server.close_input();
    let result = finished(&server.response(1));
    assert_eq!(result["ending"], "cancelled");
    assert!(result["code"].is_null() && result["signal"].is_null());
    assert!(!marker.exists());
    assert!(!input.exists());
}

#[test]
fn dropping_supervision_ends_the_live_process_tree_and_releases_its_files() {
    let directory = Directory::new();
    let input = directory.0.join("instructions");
    let child = directory.0.join("child");
    let grandchild = directory.0.join("grandchild");
    let mut args = launch(json!([
        format!("hold:{}", child.display()),
        format!("grandchild:{}", grandchild.display()),
        "sleep:60000"
    ]));
    args["temp_file"] = json!(input);
    args["on_cancel"] = json!("drop");
    let mut server = server();
    server.start_call(1, "test_run", args);
    let child = lock_file(&child);
    let grandchild = lock_file(&grandchild);
    assert!(!is_free(&child) && !is_free(&grandchild));
    assert!(input.exists());
    server.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 }
    }));
    wait_for(
        "dropped supervisor left a process or its input file",
        || is_free(&child) && is_free(&grandchild) && !input.exists(),
    );
    let next = server.call(2, "test_echo", json!({"alive":true}));
    assert_eq!(text(&next), r#"{"alive":true}"#);
}
