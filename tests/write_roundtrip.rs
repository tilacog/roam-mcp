//! Round-trip tests: create a node, then re-scan and confirm it's discoverable.

mod common;

use std::path::PathBuf;

use rmcp::model::CallToolRequestParams;
use rmcp::object;
use tempfile::TempDir;

use common::{run_with_server as run, text_of};
use org_roam_mcp::index::scan::ScanIndex;
use org_roam_mcp::index::{NodeQuery, RoamIndex};
use org_roam_mcp::{Config, RoamServer};

#[tokio::test]
async fn create_node_writes_valid_org_file() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("create_node").with_arguments(object!({
                    "title": "Test note",
                    "tags": ["test", "fixture"],
                    "body": "Body of the test note.\n",
                    "aliases": ["tst"],
                    "refs": ["https://example.com"]
                })),
            )
            .await
            .expect("create_node call");
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert!(parsed["id"].is_string());
        assert!(text.contains("\"file\""));

        // File exists on disk and has the right shape.
        let mut created = None;
        for entry in std::fs::read_dir(&path).unwrap() {
            let e = entry.unwrap();
            if e.path().extension().and_then(|x| x.to_str()) == Some("org") {
                created = Some(e.path());
                break;
            }
        }
        let file = created.expect("an .org file was created");
        let body = std::fs::read_to_string(&file).unwrap();
        assert!(
            body.contains(":ID:"),
            "must contain :ID: property, got: {body}"
        );
        assert!(
            body.contains("Test note"),
            "must contain title, got: {body}"
        );

        // Re-scan and confirm the new node is discoverable.
        let scan = ScanIndex::open(&path).expect("reopen");
        let q = NodeQuery {
            query: Some("Test note"),
            tags: &[],
            limit: Some(10),
        };
        let found = scan.find_nodes(&q).expect("search");
        assert!(
            found.iter().any(|n| n.title == "Test note"),
            "re-scan should find new node, got titles: {:?}",
            found.iter().map(|n| &n.title).collect::<Vec<_>>()
        );
    })
    .await;
}

#[tokio::test]
async fn append_to_node_appears_in_re_scan() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("create_node")
                    .with_arguments(object!({ "title": "Append test", "body": "" })),
            )
            .await
            .expect("create_node");
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
        let id = parsed["id"].as_str().unwrap().to_string();

        let r2 = peer
            .call_tool(
                CallToolRequestParams::new("append_to_node")
                    .with_arguments(object!({ "id": id, "content": "appended paragraph" })),
            )
            .await
            .expect("append_to_node");
        assert!(text_of(&r2).contains("ok"));

        // Re-scan inside the closure (after both tool calls).
        let scan = ScanIndex::open(&path).expect("rescan");
        let q = NodeQuery {
            query: Some("Append test"),
            tags: &[],
            limit: Some(10),
        };
        let found = scan.find_nodes(&q).expect("search");
        let node = found.first().expect("Append test node exists");
        let body = std::fs::read_to_string(&node.file).unwrap();
        assert!(
            body.contains("appended paragraph"),
            "body should contain append, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn append_under_last_headline_terminates() {
    // Regression: appending under the last headline of a file used to
    // spin forever (no next sibling). The harness's 10s timeout turns a
    // hang into a failure.
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("create_node").with_arguments(object!({
                    "title": "Daily",
                    "body": "* Notes\nsome notes\n\n* Tasks\n- [ ] existing\n"
                })),
            )
            .await
            .expect("create_node");
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
        let id = parsed["id"].as_str().unwrap().to_string();

        let r2 = peer
            .call_tool(CallToolRequestParams::new("append_to_node").with_arguments(
                object!({ "id": id, "content": "- [ ] new task", "headline": "Tasks" }),
            ))
            .await
            .expect("append under last headline");
        assert!(text_of(&r2).contains("ok"));

        let scan = ScanIndex::open(&path).expect("rescan");
        let q = NodeQuery {
            query: Some("Daily"),
            tags: &[],
            limit: Some(10),
        };
        let found = scan.find_nodes(&q).expect("search");
        let node = found.first().expect("Daily node exists");
        let body = std::fs::read_to_string(&node.file).unwrap();
        assert!(
            body.contains("- [ ] new task"),
            "append must land in the file, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn read_only_mode_rejects_create() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("create_node")
                    .with_arguments(object!({ "title": "Should not exist" })),
            )
            .await;
        assert!(
            result.is_err(),
            "read-only server should reject create_node"
        );
    })
    .await;

    // After server cleanup, the dir should have no .org files.
    let count = std::fs::read_dir(&path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("org"))
        .count();
    assert_eq!(count, 0, "read-only server must not write files");
}

#[tokio::test]
async fn insert_anchor_writes_target_and_returns_link() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("create_node").with_arguments(object!({
                    "title": "Anchor test",
                    "body": "First paragraph to be anchored."
                })),
            )
            .await
            .expect("create_node");
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
        let id = parsed["id"].as_str().unwrap().to_string();

        let r2 = peer
            .call_tool(
                CallToolRequestParams::new("insert_anchor").with_arguments(object!({
                    "id": id,
                    "search_text": "First paragraph",
                    "anchor_name": "para-1",
                })),
            )
            .await
            .expect("insert_anchor");
        let text = text_of(&r2);
        assert!(
            text.contains("[[id:") && text.contains("::para-1]]"),
            "should return an id:UUID::para-1 link, got: {text}"
        );

        // Verify the anchor made it to disk.
        let scan = ScanIndex::open(&path).expect("rescan");
        let q = NodeQuery {
            query: Some("Anchor test"),
            tags: &[],
            limit: Some(10),
        };
        let found = scan.find_nodes(&q).expect("search");
        let node = found.first().expect("Anchor test node exists");
        let body = std::fs::read_to_string(&node.file).unwrap();
        assert!(
            body.contains("<<para-1>>"),
            "should contain dedicated target, got: {body}"
        );
    })
    .await;
}
