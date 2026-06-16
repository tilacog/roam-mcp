//! Integration tests for the validation feature.
//!
//! Covers three layers:
//!
//! 1. `validate_node` (overloaded) — accepts either a node ID (existing
//!    cross-node check) or a raw body (new source check). Returns
//!    `isError: true` when the source has issues.
//! 2. `find_invalid_nodes` — walks the vault and returns a flat issue list.
//! 3. The create/update gate — a body that fails validation must not be
//!    written to disk, and a failed `update_node` must not modify the file.

mod common;

use std::path::PathBuf;

use rmcp::model::CallToolRequestParams;
use rmcp::object;
use tempfile::TempDir;

use common::{run_with_server as run, text_of};
use org_roam_mcp::{Config, RoamServer};

/// Helper: call a tool and return the text payload.
async fn call_text(
    peer: &rmcp::service::Peer<rmcp::RoleClient>,
    tool: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> (Option<bool>, String) {
    let result = peer
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .expect("tool call");
    (result.is_error, text_of(&result))
}

// ── validate_node (source branch) ────────────────────────────────────────

#[tokio::test]
async fn validate_node_with_valid_body_reports_ok() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let body = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:END:
#+title: Valid

* heading
body
";
        let (is_err, text) = call_text(&peer, "validate_node", object!({ "body": body })).await;
        assert!(
            !is_err.unwrap_or(false),
            "valid body must not error: {text}"
        );
        assert!(
            text.contains("\"ok\":true"),
            "expected ok:true, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn validate_node_with_invalid_body_returns_is_error() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        // No :PROPERTIES: drawer at all.
        let body = "#+title: No drawer\n* heading\n";
        let (is_err, text) = call_text(&peer, "validate_node", object!({ "body": body })).await;
        assert_eq!(is_err, Some(true), "expected isError, got: {text}");
        assert!(
            text.contains("missing_properties_drawer"),
            "expected the variant, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn validate_node_with_malformed_id_returns_issue_with_line() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let body = "\
:PROPERTIES:
:ID:       not-a-uuid
:END:
#+title: Bad
";
        let (is_err, text) = call_text(&peer, "validate_node", object!({ "body": body })).await;
        assert_eq!(is_err, Some(true), "expected isError, got: {text}");
        // Parse the JSON and confirm the issue is structured.
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
        let issues = parsed["issues"].as_array().expect("issues array");
        assert!(
            issues
                .iter()
                .any(|i| i["variant"] == "malformed_id_drawer" && i["line"].is_number()),
            "expected a malformed_id_drawer issue with a line, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn validate_node_with_neither_id_nor_body_errors() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let result = peer
            .call_tool(CallToolRequestParams::new("validate_node").with_arguments(object!({})))
            .await;
        assert!(result.is_err(), "expected a hard error, got: {result:?}");
    })
    .await;
}

// ── validate_node (ID branch — existing behavior) ────────────────────────

#[tokio::test]
async fn validate_node_by_id_returns_ok_for_existing_node() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        // First create a node.
        let (_, create_text) =
            call_text(&peer, "create_node", object!({ "title": "ID check" })).await;
        let created: serde_json::Value = serde_json::from_str(&create_text).unwrap();
        let id = created["id"].as_str().expect("id").to_string();

        // Then validate by ID.
        let (is_err, text) = call_text(&peer, "validate_node", object!({ "id": id })).await;
        assert!(!is_err.unwrap_or(false), "expected ok, got: {text}");
        assert!(text.contains("\"ok\":true"), "got: {text}");
    })
    .await;
}

// ── create / update gate ─────────────────────────────────────────────────

#[tokio::test]
async fn create_node_writes_a_valid_file() {
    // The `create_node` body is synthesized, so it must always be valid.
    // This is a self-check; if it ever fails, the synthesis code has a bug.
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let (is_err, text) = call_text(
            &peer,
            "create_node",
            object!({
                "title": "Gated create",
                "tags": ["x"],
                "body": "Some body.\n",
                "aliases": ["alt"],
                "refs": ["https://example.com"]
            }),
        )
        .await;
        assert!(
            !is_err.unwrap_or(false),
            "create_node should succeed, got: {text}"
        );
        // File should exist on disk.
        let count = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("org"))
            .count();
        assert_eq!(count, 1, "exactly one .org file should be written");
    })
    .await;
}

#[tokio::test]
async fn update_node_refuses_to_write_invalid_body() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        // Create a valid node first.
        let (_, create_text) =
            call_text(&peer, "create_node", object!({ "title": "Update gate" })).await;
        let created: serde_json::Value = serde_json::from_str(&create_text).unwrap();
        let id = created["id"].as_str().expect("id").to_string();

        // Replace the body with text that contains an unclosed drawer.
        // `replace_file_body` keeps the header drawer, so the structural
        // check on the file's body is what we want to trip.
        let (is_err, text) = call_text(
            &peer,
            "update_node",
            object!({
                "id": id,
                "body": "* heading\n  :PROPERTIES:\n  :FOO: bar\nno end here",
            }),
        )
        .await;
        assert_eq!(is_err, Some(true), "expected isError, got: {text}");
        assert!(
            text.contains("unclosed_drawer") || text.contains("ok\":false"),
            "expected validation failure payload, got: {text}"
        );

        // File on disk must not have been overwritten with the invalid body.
        let file_path = created["file"].as_str().expect("file");
        let on_disk = std::fs::read_to_string(file_path).unwrap();
        assert!(
            !on_disk.contains("no end here"),
            "file must not have been overwritten with invalid body, got: {on_disk}"
        );
    })
    .await;
}

#[tokio::test]
async fn update_node_preview_includes_validation_report() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    run(server, |peer| async move {
        let (_, create_text) =
            call_text(&peer, "create_node", object!({ "title": "Preview gate" })).await;
        let created: serde_json::Value = serde_json::from_str(&create_text).unwrap();
        let id = created["id"].as_str().expect("id").to_string();

        let (_, text) = call_text(
            &peer,
            "update_node",
            object!({
                "id": id,
                "body": "* heading\n  :PROPERTIES:\n  :FOO: bar\nno end here",
                "preview": true,
            }),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        // The JSON has shape `{ id, file, valid, issues, preview }` for the
        // preview branch — `valid` and `issues` sit at the top level.
        assert_eq!(parsed["valid"], serde_json::Value::Bool(false));
        assert!(
            !parsed["issues"].as_array().unwrap().is_empty(),
            "preview should include issues, got: {text}"
        );
    })
    .await;
}

// ── find_invalid_nodes ───────────────────────────────────────────────────

#[tokio::test]
async fn find_invalid_nodes_reports_only_broken_files() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    // Hand-craft a vault with one valid and two broken files.
    std::fs::write(
        path.join("good.org"),
        "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:END:
#+title: Good

* heading
",
    )
    .unwrap();
    std::fs::write(
        path.join("bad_id.org"),
        "\
:PROPERTIES:
:ID:       not-a-uuid
:END:
#+title: Bad id
",
    )
    .unwrap();
    std::fs::write(
        path.join("no_drawer.org"),
        "#+title: No drawer\n* heading\n",
    )
    .unwrap();

    run(server, |peer| async move {
        let (is_err, text) = call_text(&peer, "find_invalid_nodes", object!({})).await;
        assert!(!is_err.unwrap_or(false), "scan should succeed, got: {text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["scanned"], 3);
        assert_eq!(parsed["with_issues"], 2);
        let issues = parsed["issues"].as_array().expect("issues array");
        // Both broken files should appear; the good one should not.
        let files: std::collections::HashSet<String> = issues
            .iter()
            .map(|i| i["file_path"].as_str().unwrap().to_string())
            .collect();
        assert!(files.iter().any(|f| f.ends_with("bad_id.org")));
        assert!(files.iter().any(|f| f.ends_with("no_drawer.org")));
        assert!(!files.iter().any(|f| f.ends_with("good.org")));
    })
    .await;
}

#[tokio::test]
async fn find_invalid_nodes_returns_clean_report_for_valid_vault() {
    let dir = TempDir::new().expect("tmpdir");
    let path: PathBuf = dir.path().to_path_buf();
    let cfg = Config::from_args(&path, false, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();

    std::fs::write(
        path.join("only_good.org"),
        "\
:PROPERTIES:
:ID:       22222222-2222-2222-2222-222222222222
:END:
#+title: Only good
",
    )
    .unwrap();

    run(server, |peer| async move {
        let (is_err, text) = call_text(&peer, "find_invalid_nodes", object!({})).await;
        assert!(!is_err.unwrap_or(false), "got: {text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["scanned"], 1);
        assert_eq!(parsed["with_issues"], 0);
        assert_eq!(parsed["issue_count"], 0);
    })
    .await;
}
