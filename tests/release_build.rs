//! A build without the `test-tools` feature, as released, serves none of the test tools.
#![cfg(not(feature = "test-tools"))]

#[allow(dead_code)]
mod common;

use common::Server;
use serde_json::json;

#[test]
fn a_release_build_lists_no_tools_and_refuses_the_test_ones() {
    let (mut server, _) = Server::initialized("2025-11-25");
    let listed = server.request(1, "tools/list", json!({}));
    assert_eq!(listed["result"]["tools"], json!([]));
    for (id, name) in [(2, "test_echo"), (3, "test_panic"), (4, "test_wait")] {
        let refused = server.call(id, name, json!({}));
        assert_eq!(refused["error"]["code"], -32602, "{name}: {refused}");
    }
    let (status, _) = server.finish();
    assert!(status.success());
}
