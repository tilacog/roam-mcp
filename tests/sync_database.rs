//! Integration coverage for the `sync_database` tool.
//!
//! These run in scanner mode (`no_db = true`) so no Emacs / `org-roam.db`
//! is required: the drift block reports scanner-only, a forced `auto`/
//! `scanner` sync rebuilds the in-process index, and `never` mode is a
//! no-op with a warning. The sqlite-sync path (which shells out to
//! `emacsclient`) is deliberately not exercised here.

mod common;

use std::path::Path;

use rmcp::model::CallToolRequestParams;
use rmcp::object;
use rmcp::service::Peer;
use rmcp::RoleClient;
use serde_json::{Map, Value};
use tempfile::TempDir;

use common::{run_with_server as run, text_of};
use org_roam_mcp::sync::SyncMode;
use org_roam_mcp::{Config, RoamServer};

async fn call(peer: &Peer<RoleClient>, tool: &str, args: Map<String, Value>) -> Value {
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    match peer.call_tool(params).await {
        Ok(result) => {
            let text = text_of(&result);
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        }
        // MCP protocol errors (e.g. "node not found" → -32602) return Null so
        // callers can assert `value.is_null()` or `value.get("id").is_none()`.
        Err(_) => Value::Null,
    }
}

fn server_with_mode(dir: &TempDir, read_only: bool, mode: SyncMode) -> RoamServer {
    let mut cfg = Config::from_args(dir.path(), read_only, true, None).unwrap();
    cfg.sync_mode = mode;
    RoamServer::new(cfg).unwrap()
}

fn server(dir: &TempDir) -> RoamServer {
    server_with_mode(dir, false, SyncMode::default())
}

#[tokio::test]
async fn reports_state_without_syncing() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        let v = call(&peer, "sync_database", object!({})).await;
        assert_eq!(v["ok"], Value::Bool(true));
        assert_eq!(v["synced"], Value::Bool(false), "force:false must not sync");
        assert_eq!(v["active_backend"], "scanner");
        assert_eq!(v["db_exists"], Value::Bool(false));
        assert_eq!(v["drift"]["scanner_node_count"], 0);
        assert!(
            v["drift"]["sqlite_node_count"].is_null(),
            "no db => null sqlite count, got {}",
            v["drift"]["sqlite_node_count"]
        );
        // The "no org-roam.db" warning used to be returned here. It
        // was misleading (the scanner is the index in this mode) and
        // drowned out real warnings. `db_exists: false` already
        // communicates the state; the warning is gone. Any remaining
        // warnings must be unrelated to the missing db.
        let warnings = v["warnings"].as_array().expect("warnings array");
        assert!(
            !warnings
                .iter()
                .any(|w| w.as_str().is_some_and(|s| s.contains("no org-roam.db"))),
            "the noisy no-db warning must not be returned, got {warnings:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn force_rebuilds_scanner_and_drift_tracks_new_nodes() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        // Empty vault: drift sees zero nodes.
        let before = call(&peer, "sync_database", object!({})).await;
        assert_eq!(before["drift"]["scanner_node_count"], 0);

        call(&peer, "create_node", object!({ "title": "Castle Dracula" })).await;

        let v = call(&peer, "sync_database", object!({ "force": true })).await;
        assert_eq!(v["ok"], Value::Bool(true));
        assert_eq!(v["synced"], Value::Bool(true));
        assert_eq!(
            v["outcome"].as_str().unwrap_or_default(),
            "scanner index rebuilt"
        );
        assert_eq!(
            v["drift"]["scanner_node_count"], 1,
            "the new node must show up in scanner drift"
        );
    })
    .await;
}

#[tokio::test]
async fn never_mode_force_is_noop_with_warning() {
    let dir = TempDir::new().unwrap();
    run(
        server_with_mode(&dir, false, SyncMode::Never),
        |peer| async move {
            let v = call(
                &peer,
                "sync_database",
                object!({ "force": true, "backend": "sqlite" }),
            )
            .await;
            // A no-op, not an error.
            assert_eq!(v["ok"], Value::Bool(true));
            assert_eq!(v["synced"], Value::Bool(false));
            assert_eq!(v["mode"], "Never");
            let warnings = v["warnings"].as_array().expect("warnings array");
            assert!(
                warnings
                    .iter()
                    .any(|w| w.as_str().is_some_and(|s| s.contains("never"))),
                "expected a 'never' warning, got {warnings:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn available_and_reports_in_read_only_mode() {
    let dir = TempDir::new().unwrap();
    run(
        server_with_mode(&dir, true, SyncMode::default()),
        |peer| async move {
            let v = call(&peer, "sync_database", object!({})).await;
            assert_eq!(v["ok"], Value::Bool(true));
            assert_eq!(v["active_backend"], "scanner");
        },
    )
    .await;
}

/// Quote a value the way emacsql prints it into the db.
fn lisp(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Write a minimal org-roam.db holding the given `(id, file)` nodes.
fn write_db(dir: &Path, nodes: &[(&str, &str)]) {
    let conn = rusqlite::Connection::open(dir.join("org-roam.db")).expect("create db");
    conn.execute_batch(
        "CREATE TABLE nodes (id NOT NULL PRIMARY KEY, file NOT NULL, level NOT NULL, \
             pos NOT NULL, todo, priority, scheduled, deadline, title, properties, olp);
         CREATE TABLE aliases (node_id NOT NULL, alias);
         CREATE TABLE tags (node_id NOT NULL, tag);",
    )
    .expect("schema");
    for (id, file) in nodes {
        conn.execute(
            "INSERT INTO nodes VALUES (?, ?, 0, 1, NULL, NULL, NULL, NULL, ?, NULL, 'nil')",
            rusqlite::params![lisp(id), lisp(file), lisp("t")],
        )
        .expect("insert node");
    }
}

/// Write an on-disk file-level node.
fn write_node_file(dir: &Path, name: &str, id: &str) {
    std::fs::write(
        dir.join(name),
        format!(":PROPERTIES:\n:ID:       {id}\n:END:\n#+title: {name}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn drift_reports_db_vs_disk_divergence() {
    const ON_DISK_AND_DB: &str = "11111111-1111-1111-1111-111111111111";
    const ON_DISK_ONLY: &str = "22222222-2222-2222-2222-222222222222";
    const DB_ONLY: &str = "33333333-3333-3333-3333-333333333333";

    let dir = TempDir::new().unwrap();
    // Two files on disk; the db knows one of them plus a stale row.
    write_node_file(dir.path(), "shared.org", ON_DISK_AND_DB);
    write_node_file(dir.path(), "fresh.org", ON_DISK_ONLY);
    write_db(
        dir.path(),
        &[(ON_DISK_AND_DB, "shared.org"), (DB_ONLY, "deleted.org")],
    );

    // no_db = false so the sqlite backend is active and drift is computed.
    let cfg = Config::from_args(dir.path(), false, false, None).unwrap();
    run(RoamServer::new(cfg).unwrap(), |peer| async move {
        let v = call(&peer, "sync_database", object!({})).await;
        assert_eq!(v["active_backend"], "sqlite");
        assert_eq!(v["db_exists"], Value::Bool(true));
        assert_eq!(v["drift"]["scanner_node_count"], 2);
        assert_eq!(v["drift"]["sqlite_node_count"], 2);

        let missing_in_sqlite = v["drift"]["missing_in_sqlite"].as_array().unwrap();
        assert_eq!(
            missing_in_sqlite,
            &vec![Value::String(ON_DISK_ONLY.to_string())],
            "the un-synced on-disk node must be flagged as missing_in_sqlite"
        );
        let missing_in_scanner = v["drift"]["missing_in_scanner"].as_array().unwrap();
        assert_eq!(
            missing_in_scanner,
            &vec![Value::String(DB_ONLY.to_string())],
            "the stale db row must be flagged as missing_in_scanner"
        );
    })
    .await;
}

#[tokio::test]
async fn rejects_unknown_backend() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        let params = CallToolRequestParams::new("sync_database".to_string())
            .with_arguments(object!({ "force": true, "backend": "postgres" }));
        let result = peer.call_tool(params).await;
        assert!(result.is_err(), "an unknown backend must be rejected");
    })
    .await;
}

// --- §4 (todo-followup): scanner-rebuild interleaving tests ----------------
//
// These tests verify that the index reflects file-system changes after a
// forced scanner rebuild (which bypasses the file-watcher). We trigger the
// rebuild via `sync_database { force: true }` (which calls the private
// `rebuild_scanner_index` method) and then query via read tools to confirm
// the index was updated.

/// Write a minimal org-roam node to a file on disk, then force a scanner
/// rebuild, and assert that `get_node` can find the new node.
#[tokio::test]
async fn write_then_force_rebuild_is_visible_to_get_node() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        // Write a file directly on disk (bypassing the MCP write tools,
        // so the write-tool-triggered rebuild doesn't fire).
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        std::fs::write(
            dir.path().join("manual.org"),
            format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: Manually Written\n"),
        )
        .unwrap();

        // Before the rebuild, the node is not visible.
        let before = call(&peer, "get_node", object!({ "id": id })).await;
        assert!(
            before.is_null() || before.get("error").is_some() || before.get("id").is_none(),
            "node must not be visible before rebuild, got: {before}"
        );

        // Force a scanner rebuild.
        let sync_result = call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;
        assert_eq!(
            sync_result["ok"],
            serde_json::Value::Bool(true),
            "force rebuild must succeed: {sync_result}"
        );

        // After the rebuild the node is visible.
        let after = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(
            after["title"],
            serde_json::Value::String("Manually Written".into()),
            "node must be visible after rebuild: {after}"
        );
    })
    .await;
}

/// Create two nodes sequentially, each via a direct write + forced rebuild,
/// and verify that the second rebuild sees both nodes (not just the second).
#[tokio::test]
async fn two_sequential_writes_both_visible_after_rebuild() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        let id1 = "11111111-aaaa-bbbb-cccc-dddddddddddd";
        let id2 = "22222222-aaaa-bbbb-cccc-dddddddddddd";

        for (id, name) in [(id1, "First"), (id2, "Second")] {
            std::fs::write(
                dir.path().join(format!("{name}.org")),
                format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: {name}\n"),
            )
            .unwrap();
        }

        // A single rebuild after both writes must index both files.
        call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;

        for (id, title) in [(id1, "First"), (id2, "Second")] {
            let node = call(&peer, "get_node", object!({ "id": id })).await;
            assert_eq!(
                node["title"],
                serde_json::Value::String(title.into()),
                "node {id} must be visible after rebuild: {node}"
            );
        }
    })
    .await;
}

/// Delete a file, force a rebuild, and verify the node is no longer visible.
#[tokio::test]
async fn deletion_and_rebuild_removes_node_from_index() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        let id = "deadbeef-dead-beef-dead-beefdeadbeef";
        let path = dir.path().join("doomed.org");
        std::fs::write(
            &path,
            format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: Doomed\n"),
        )
        .unwrap();

        // First rebuild: node must appear.
        call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;
        let before = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(
            before["title"],
            serde_json::Value::String("Doomed".into()),
            "node must be visible after first rebuild: {before}"
        );

        // Delete the file and rebuild again.
        std::fs::remove_file(&path).unwrap();
        call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;

        // After deletion + rebuild, the node must not be findable.
        let after = call(&peer, "get_node", object!({ "id": id })).await;
        assert!(
            after.is_null() || after.get("id").is_none(),
            "deleted node must not be visible after rebuild: {after}"
        );
    })
    .await;
}

/// Replace a file with different content, rebuild, and verify the index
/// reflects the new content (not a cache of the old).
#[tokio::test]
async fn file_recreation_with_new_content_is_visible_after_rebuild() {
    let dir = TempDir::new().unwrap();
    run(server(&dir), |peer| async move {
        let id = "cafecafe-cafe-cafe-cafe-cafecafecafe";
        let path = dir.path().join("mutable.org");

        // Create the original file and index it.
        std::fs::write(
            &path,
            format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: Original Title\n"),
        )
        .unwrap();
        call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;
        let v1 = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(
            v1["title"],
            serde_json::Value::String("Original Title".into())
        );

        // Overwrite with new content and rebuild.
        std::fs::write(
            &path,
            format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: Revised Title\n"),
        )
        .unwrap();
        call(
            &peer,
            "sync_database",
            object!({ "force": true, "backend": "scanner" }),
        )
        .await;
        let v2 = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(
            v2["title"],
            serde_json::Value::String("Revised Title".into()),
            "index must reflect the updated title after rebuild: {v2}"
        );
    })
    .await;
}
