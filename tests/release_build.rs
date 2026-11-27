//! A build without the `test-tools` feature, as released, serves none of the test tools.
#![cfg(not(feature = "test-tools"))]

#[allow(dead_code)]
mod common;

use common::Server;
use serde_json::json;

#[test]
fn a_release_build_lists_only_shared_tools_and_refuses_the_test_ones() {
    let (mut server, _) = Server::initialized("2025-11-25");
    let listed = server.request(1, "tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["await", "cancel", "usage"]);
    for (id, name) in [
        (2, "test_echo"),
        (3, "test_panic"),
        (4, "test_wait"),
        (5, "test_run"),
    ] {
        let refused = server.call(id, name, json!({}));
        assert_eq!(refused["error"]["code"], -32602, "{name}: {refused}");
    }
    let (status, _) = server.finish();
    assert!(status.success());
}
