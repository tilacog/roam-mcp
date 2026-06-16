//! Integration coverage for the tools added on top of the original
//! read/write surface: `update_node`, `delete_node`, `prepend_to_node`,
//! `rename_node`, `add_link`, `list_nodes`, `search_text`,
//! `get_node_by_path`, `get_refs`, `list_anchors`, `tag_cooccurrences`,
//! `validate_node`, the daily read tools, and the enriched `server_info`.
//!
//! All run in scanner mode (`no_db = true`) so each write refreshes the
//! in-process index and the next call sees it.

mod common;

use std::path::PathBuf;

use rmcp::model::CallToolRequestParams;
use rmcp::object;
use rmcp::service::Peer;
use rmcp::RoleClient;
use serde_json::{Map, Value};
use tempfile::TempDir;

use common::{run_with_server, run_with_server as run, text_of};
use org_roam_mcp::{Config, RoamServer};

/// Call a tool and parse its (JSON) text payload.
async fn call(peer: &Peer<RoleClient>, tool: &str, args: Map<String, Value>) -> Value {
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    let result = peer
        .call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("{tool} call failed: {e}"));
    let text = text_of(&result);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// Create a node and return its `:ID:` and file path.
async fn create(peer: &Peer<RoleClient>, args: Map<String, Value>) -> (String, PathBuf) {
    let v = call(peer, "create_node", args).await;
    (
        v["id"].as_str().unwrap().to_string(),
        PathBuf::from(v["file"].as_str().unwrap()),
    )
}

fn server(dir: &TempDir, read_only: bool) -> RoamServer {
    let cfg = Config::from_args(dir.path(), read_only, true, None).unwrap();
    RoamServer::new(cfg).unwrap()
}

#[tokio::test]
async fn update_node_edits_metadata_and_body_idempotently() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, file) = create(
            &peer,
            object!({ "title": "Original", "tags": ["a"], "body": "old body\n" }),
        )
        .await;

        let res = call(
            &peer,
            "update_node",
            object!({
                "id": id,
                "title": "Renamed",
                "tags": ["x", "y"],
                "aliases": ["nick"],
                "refs": ["https://example.com"],
                "body": "fresh body line\n"
            }),
        )
        .await;
        assert_eq!(res["updated"], Value::Bool(true));

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("#+title: Renamed"),
            "title not updated: {text}"
        );
        assert!(
            text.contains("#+filetags: :x:y:"),
            "tags not updated: {text}"
        );
        assert!(
            text.contains(":ROAM_ALIASES: \"nick\""),
            "aliases missing: {text}"
        );
        assert!(
            text.contains(":ROAM_REFS: https://example.com"),
            "refs missing: {text}"
        );
        assert!(
            text.contains("fresh body line"),
            "body not replaced: {text}"
        );
        assert!(!text.contains("old body"), "old body survived: {text}");
        // The :ID: is preserved across the update.
        assert!(text.contains(&id), "ID changed during update: {text}");

        // get_node reflects the change.
        let node = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(node["title"], Value::String("Renamed".into()));
    })
    .await;
}

#[tokio::test]
async fn update_node_preview_does_not_write() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, file) = create(&peer, object!({ "title": "Keep", "body": "body\n" })).await;
        let before = std::fs::read_to_string(&file).unwrap();

        let res = call(
            &peer,
            "update_node",
            object!({ "id": id, "title": "Would Change", "preview": true }),
        )
        .await;
        assert_eq!(res["changed"], Value::Bool(true));
        assert!(res["preview"]
            .as_str()
            .unwrap()
            .contains("#+title: Would Change"));

        // Disk is untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    })
    .await;
}

#[tokio::test]
async fn delete_node_removes_file() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, file) = create(&peer, object!({ "title": "Doomed" })).await;
        assert!(file.exists());

        let res = call(&peer, "delete_node", object!({ "id": id })).await;
        assert_eq!(res["deleted"], Value::Bool(true));
        assert_eq!(res["kind"], Value::String("file".into()));
        assert!(!file.exists(), "file should be gone");
    })
    .await;
}

#[tokio::test]
async fn delete_node_removes_only_the_headline_subtree() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let hid = "aaaaaaaa-1111-2222-3333-444444444444";
        let (_file_id, file) = create(
            &peer,
            object!({
                "title": "Container",
                "body": format!(
                    "intro\n\n* Sub\n:PROPERTIES:\n:ID: {hid}\n:END:\nsub body\n\n* Keep\nkeep body\n"
                )
            }),
        )
        .await;

        let res = call(&peer, "delete_node", object!({ "id": hid })).await;
        assert_eq!(res["kind"], Value::String("headline".into()));

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(file.exists(), "file must remain");
        assert!(!text.contains("* Sub"), "subtree should be gone: {text}");
        assert!(text.contains("* Keep"), "sibling must remain: {text}");
        assert!(text.contains("intro"), "preamble body must remain: {text}");
    })
    .await;
}

#[tokio::test]
async fn prepend_to_node_inserts_before_existing_body() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, file) = create(&peer, object!({ "title": "P", "body": "existing line\n" })).await;
        call(
            &peer,
            "prepend_to_node",
            object!({ "id": id, "content": "PREPENDED" }),
        )
        .await;

        let text = std::fs::read_to_string(&file).unwrap();
        let prep = text.find("PREPENDED").unwrap();
        let existing = text.find("existing line").unwrap();
        let title = text.find("#+title").unwrap();
        assert!(title < prep && prep < existing, "ordering wrong: {text}");
    })
    .await;
}

#[tokio::test]
async fn rename_node_updates_title_and_file_but_keeps_id() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, old_file) = create(&peer, object!({ "title": "Old Title" })).await;

        let res = call(
            &peer,
            "rename_node",
            object!({ "id": id, "title": "Brand New" }),
        )
        .await;
        let new_file = PathBuf::from(res["file"].as_str().unwrap());
        assert!(new_file
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("brand-new"));
        assert!(
            !old_file.exists() || new_file == old_file,
            "old file should be gone"
        );

        let text = std::fs::read_to_string(&new_file).unwrap();
        assert!(text.contains("#+title: Brand New"));
        // Same :ID:, so get_node still resolves.
        let node = call(&peer, "get_node", object!({ "id": id })).await;
        assert_eq!(node["title"], Value::String("Brand New".into()));
    })
    .await;
}

#[tokio::test]
async fn add_link_writes_link_and_shows_in_forward_links() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (src, src_file) = create(&peer, object!({ "title": "Source" })).await;
        let (dst, _) = create(&peer, object!({ "title": "Destination" })).await;

        let res = call(&peer, "add_link", object!({ "id": src, "target": dst })).await;
        assert!(res["link"].as_str().unwrap().contains(&dst));

        let text = std::fs::read_to_string(&src_file).unwrap();
        assert!(
            text.contains(&format!("[[id:{dst}][Destination]]")),
            "link missing: {text}"
        );

        let fwd = call(&peer, "get_forward_links", object!({ "id": src })).await;
        let has = fwd
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["link"]["dest"] == Value::String(dst.clone()));
        assert!(has, "forward link not indexed: {fwd}");
    })
    .await;
}

#[tokio::test]
async fn list_nodes_paginates() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        for t in ["Alpha", "Bravo", "Charlie"] {
            create(&peer, object!({ "title": t })).await;
        }
        let page = call(&peer, "list_nodes", object!({ "limit": 2, "offset": 0 })).await;
        assert_eq!(page["total"], Value::from(3));
        assert_eq!(page["count"], Value::from(2));
        assert_eq!(page["nodes"][0]["title"], Value::String("Alpha".into()));

        let page2 = call(&peer, "list_nodes", object!({ "limit": 2, "offset": 2 })).await;
        assert_eq!(page2["count"], Value::from(1));
        assert_eq!(page2["nodes"][0]["title"], Value::String("Charlie".into()));
    })
    .await;
}

#[tokio::test]
async fn search_text_finds_body_matches() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, _) = create(
            &peer,
            object!({ "title": "Animals", "body": "the quick brown fox jumps\n" }),
        )
        .await;
        let hits = call(&peer, "search_text", object!({ "query": "brown fox" })).await;
        let arr = hits.as_array().unwrap();
        assert!(!arr.is_empty(), "expected a match");
        assert!(arr
            .iter()
            .any(|h| h["node_id"] == Value::String(id.clone())));
    })
    .await;
}

#[tokio::test]
async fn get_node_by_path_resolves_via_file() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, file) = create(&peer, object!({ "title": "By Path" })).await;
        let node = call(
            &peer,
            "get_node_by_path",
            object!({ "path": file.to_str().unwrap() }),
        )
        .await;
        assert_eq!(node["id"], Value::String(id));
        assert_eq!(node["title"], Value::String("By Path".into()));
    })
    .await;
}

#[tokio::test]
async fn get_refs_returns_declared_refs() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, _) = create(
            &peer,
            object!({ "title": "Cited", "refs": ["https://example.com/a", "@key2023"] }),
        )
        .await;
        let payload = call(&peer, "get_refs", object!({ "id": id })).await;
        let refs: Vec<String> = payload["refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            refs.contains(&"https://example.com/a".to_string()),
            "{refs:?}"
        );
        assert!(refs.contains(&"@key2023".to_string()), "{refs:?}");
    })
    .await;
}

#[tokio::test]
async fn list_anchors_reports_targets_and_headlines() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (id, _) = create(
            &peer,
            object!({
                "title": "Anchored",
                "body": "<<spot-one>>\nfirst para\n\n* A Section\nbody\n"
            }),
        )
        .await;
        let res = call(&peer, "list_anchors", object!({ "id": id })).await;
        let targets: Vec<String> = res["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let headlines: Vec<String> = res["headlines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(targets.contains(&"spot-one".to_string()), "{targets:?}");
        assert!(
            headlines.contains(&"A Section".to_string()),
            "{headlines:?}"
        );
    })
    .await;
}

// --- §3: list_anchors surfaces #+NAME: keywords ---------------------

#[tokio::test]
async fn list_anchors_includes_name_properties() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), move |peer| async move {
        let (id, _) = create(
            &peer,
            object!({
                "title": "With name",
                "body": "\
#+NAME: growth-table
| year | nodes |
|------+-------|
| 2024 | 2     |
",
            }),
        )
        .await;
        let res = call(&peer, "list_anchors", object!({ "id": id })).await;
        let names: Vec<String> = res["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"growth-table".to_string()),
            "expected growth-table in names, got: {names:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn validate_node_flags_dangling_links() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (clean, _) = create(&peer, object!({ "title": "Clean" })).await;
        let ok = call(&peer, "validate_node", object!({ "id": clean })).await;
        assert_eq!(ok["ok"], Value::Bool(true), "{ok}");

        let (broken, _) = create(
            &peer,
            object!({
                "title": "Broken",
                "body": "see [[id:deadbeef-0000-0000-0000-000000000000][ghost]]\n"
            }),
        )
        .await;
        let bad = call(&peer, "validate_node", object!({ "id": broken })).await;
        assert_eq!(bad["ok"], Value::Bool(false), "{bad}");
        assert!(
            bad["dangling_links"]
                .as_array()
                .unwrap()
                .contains(&Value::String(
                    "deadbeef-0000-0000-0000-000000000000".into()
                )),
            "{bad}"
        );
    })
    .await;
}

#[tokio::test]
async fn daily_read_tools_observe_captures() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        // Before any capture, today's note does not exist.
        let pre = call(&peer, "get_daily_note", object!({})).await;
        assert_eq!(pre["exists"], Value::Bool(false), "{pre}");

        call(
            &peer,
            "daily_capture",
            object!({ "content": "captured today" }),
        )
        .await;

        let post = call(&peer, "get_daily_note", object!({})).await;
        assert_eq!(post["exists"], Value::Bool(true), "{post}");
        assert!(post["body"].as_str().unwrap().contains("captured today"));

        let listed = call(&peer, "list_dailies", object!({})).await;
        assert!(listed["count"].as_u64().unwrap() >= 1, "{listed}");
    })
    .await;
}

#[tokio::test]
async fn server_info_reports_backend_and_config() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let info = call(&peer, "server_info", object!({})).await;
        assert_eq!(info["backend"], Value::String("scanner".into()));
        assert_eq!(info["has_db"], Value::Bool(false));
        assert!(info["version"].is_string());
        assert!(info["sync"]["debounce_ms"].is_number());
    })
    .await;
}

#[tokio::test]
async fn read_only_mode_rejects_new_write_tools() {
    let dir = TempDir::new().unwrap();
    // Pre-seed a node with the scanner-mode server, then reopen read-only.
    {
        run(server(&dir, false), |peer| async move {
            create(&peer, object!({ "title": "Seed" })).await;
        })
        .await;
    }

    run(server(&dir, true), |peer| async move {
        for tool in [
            "update_node",
            "delete_node",
            "rename_node",
            "prepend_to_node",
            "add_link",
        ] {
            let params = CallToolRequestParams::new(tool.to_string()).with_arguments(
                object!({ "id": "x", "title": "y", "content": "z", "target": "t" }),
            );
            let res = peer.call_tool(params).await;
            assert!(res.is_err(), "{tool} must be rejected in read-only mode");
        }
        // Reads still work.
        let info = call(&peer, "server_info", object!({})).await;
        assert_eq!(info["read_only"], Value::Bool(true));
    })
    .await;
}

#[tokio::test]
async fn list_orphans_finds_unconnected_nodes() {
    // Two siblings: alpha has no id links at all, bravo links to
    // charlie, charlie links back to bravo. Only alpha is an orphan.
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (alpha, _) = create(&peer, object!({ "title": "Alpha" })).await;
        let (bravo, _) = create(&peer, object!({ "title": "Bravo" })).await;
        let (charlie, _) = create(&peer, object!({ "title": "Charlie" })).await;
        // bravo <-> charlie: bidirectional id edges, both connected.
        call(
            &peer,
            "add_link",
            object!({ "id": bravo, "target": charlie }),
        )
        .await;
        call(
            &peer,
            "add_link",
            object!({ "id": charlie, "target": bravo }),
        )
        .await;

        let res = call(&peer, "list_orphans", object!({})).await;
        assert_eq!(
            res["total"],
            Value::from(1),
            "only alpha is orphaned: {res}"
        );
        assert_eq!(res["count"], Value::from(1));
        assert_eq!(
            res["nodes"][0]["id"],
            Value::String(alpha.clone()),
            "alpha is the orphan: {res}"
        );
        assert_eq!(res["nodes"][0]["title"], Value::String("Alpha".into()));
    })
    .await;
}

#[tokio::test]
async fn list_orphans_paginates() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        // Five siblings, no links at all — every one of them is an orphan.
        for t in ["Aardvark", "Badger", "Capybara", "Dingo", "Echidna"] {
            create(&peer, object!({ "title": t })).await;
        }
        let page1 = call(&peer, "list_orphans", object!({ "limit": 2, "offset": 0 })).await;
        assert_eq!(page1["total"], Value::from(5));
        assert_eq!(page1["count"], Value::from(2));
        assert_eq!(page1["limit"], Value::from(2));
        assert_eq!(page1["offset"], Value::from(0));
        assert_eq!(page1["nodes"][0]["title"], Value::String("Aardvark".into()));
        assert_eq!(page1["nodes"][1]["title"], Value::String("Badger".into()));

        let page3 = call(&peer, "list_orphans", object!({ "limit": 2, "offset": 4 })).await;
        assert_eq!(page3["count"], Value::from(1));
        assert_eq!(page3["nodes"][0]["title"], Value::String("Echidna".into()));
    })
    .await;
}

#[tokio::test]
async fn list_orphans_is_empty_when_every_node_is_connected() {
    let dir = TempDir::new().unwrap();
    run(server(&dir, false), |peer| async move {
        let (a, _) = create(&peer, object!({ "title": "A" })).await;
        let (b, _) = create(&peer, object!({ "title": "B" })).await;
        call(&peer, "add_link", object!({ "id": a, "target": b })).await;
        call(&peer, "add_link", object!({ "id": b, "target": a })).await;

        let res = call(&peer, "list_orphans", object!({})).await;
        assert_eq!(res["total"], Value::from(0), "no orphans: {res}");
        assert_eq!(res["count"], Value::from(0));
        assert_eq!(res["nodes"].as_array().unwrap().len(), 0);
    })
    .await;
}

#[tokio::test]
async fn list_orphans_works_in_read_only_mode() {
    // The tool is a read; it must remain callable when writes are off.
    let dir = TempDir::new().unwrap();
    {
        run(server(&dir, false), |peer| async move {
            create(&peer, object!({ "title": "Lonely" })).await;
        })
        .await;
    }
    run(server(&dir, true), |peer| async move {
        let res = call(&peer, "list_orphans", object!({})).await;
        assert!(res["total"].as_u64().unwrap() >= 1, "{res}");
    })
    .await;
}

#[tokio::test]
async fn unlinked_references_filters_short_needles() {
    // The unlinked_references tool skips needles shorter than 3
    // characters (otherwise every common word would produce noise).
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("node.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n:END:\n#+title: T\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.org"),
        ":PROPERTIES:\n:ID: 11111111-2222-3333-4444-555555555555\n:END:\n\
         #+title: Other\n\nThe string `ab` is too short to ever match.\n",
    )
    .unwrap();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        // A 2-character title would not be in the needles list; the
        // tool returns an empty array.
        let res = call(
            &peer,
            "unlinked_references",
            object!({ "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" }),
        )
        .await;
        let arr = res.as_array().expect("array");
        assert!(arr.is_empty(), "expected no hits, got: {arr:?}");
    })
    .await;
}

#[tokio::test]
async fn unlinked_references_with_no_needles_returns_empty() {
    // A node with no title and no aliases (theoretically possible
    // if a vault has an `#+title:` that's empty and no aliases)
    // returns an empty array instead of erroring. With a normal
    // title (length >= 3), the result depends on whether the
    // title appears elsewhere.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("node.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n:END:\n#+title: T\n",
    )
    .unwrap();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        // A single-char title is filtered out.
        let res = call(
            &peer,
            "unlinked_references",
            object!({ "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" }),
        )
        .await;
        let arr = res.as_array().expect("array");
        assert!(arr.is_empty(), "got: {arr:?}");
    })
    .await;
}

#[tokio::test]
async fn unlinked_references_unknown_id_returns_invalid_params() {
    // Calling with an unknown id must fail with a "not found"
    // error, not return an empty array. This is part of the
    // "honest failure" contract.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("node.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n:END:\n#+title: T\n",
    )
    .unwrap();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        let params = CallToolRequestParams::new("unlinked_references")
            .with_arguments(object!({ "id": "deadbeef-0000-0000-0000-000000000000" }));
        let res = peer.call_tool(params).await;
        assert!(res.is_err(), "unknown id must return an error");
    })
    .await;
}

// --- §0.1: unlinked_references coverage --------------------------------

fn vault_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("vault")
}

fn vault_server() -> (TempDir, RoamServer) {
    let dir = TempDir::new().unwrap();
    // Copy the vault fixture into a tmpdir so we don't disturb other tests
    // that may want a clean vault.
    let src = vault_dir();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let name = entry.file_name();
            std::fs::copy(entry.path(), dir.path().join(&name)).unwrap();
        }
    }
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    (dir, server)
}

#[tokio::test]
async fn unlinked_references_finds_alias_in_plain_text() {
    let (_dir, server) = vault_server();
    let psalm_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    run_with_server(server, move |peer| async move {
        let psalm_id = psalm_id.to_string();
        let res = call(
            &peer,
            "unlinked_references",
            object!({ "id": psalm_id, "limit": 50 }),
        )
        .await;
        let arr = res.as_array().expect("array");
        assert!(!arr.is_empty(), "expected hits, got {res}");

        // Find a hit in shepherd.org with the alias as the matched text.
        let hit = arr
            .iter()
            .find(|h| {
                h["file"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("shepherd.org"))
                    && h["matched"] == Value::String("The Shepherd Psalm".into())
            })
            .unwrap_or_else(|| panic!("alias hit not found in {arr:?}"));

        // The snippet text should match the literal surrounding context.
        let snippet = hit["snippet"].as_str().expect("snippet string");
        assert!(
            snippet.contains("The Shepherd Psalm"),
            "snippet should contain the alias text: {snippet}"
        );
    })
    .await;
}

#[tokio::test]
async fn unlinked_references_does_not_match_inside_links() {
    // Write a temp file that contains the alias *inside* an org link,
    // and confirm the inside-link match is skipped by `inside_link`.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("psalm23.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         :ROAM_ALIASES: \"The Shepherd Psalm\"\n:END:\n#+title: Psalm 23\n\nThe Lord is my shepherd.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("linked.org"),
        ":PROPERTIES:\n:ID: 99999999-9999-9999-9999-999999999999\n:END:\n#+title: Linked\n\n\
         See [[id:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee][The Shepherd Psalm]] for more.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plain.org"),
        ":PROPERTIES:\n:ID: 88888888-8888-8888-8888-888888888888\n:END:\n#+title: Plain\n\n\
         The Shepherd Psalm is also mentioned here in plain text.\n",
    )
    .unwrap();

    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    let psalm_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
    run_with_server(server, move |peer| async move {
        let res = call(
            &peer,
            "unlinked_references",
            object!({ "id": psalm_id, "limit": 50 }),
        )
        .await;
        let arr = res.as_array().expect("array");

        // The hit in plain.org should be present.
        let in_plain = arr.iter().any(|h| {
            h["file"].as_str().is_some_and(|p| p.ends_with("plain.org"))
                && h["matched"] == Value::String("The Shepherd Psalm".into())
        });
        assert!(in_plain, "expected a hit in plain.org, got: {arr:?}");

        // The hit in linked.org must be filtered out (alias is inside `[[...]]`).
        let in_linked = arr.iter().any(|h| {
            h["file"]
                .as_str()
                .is_some_and(|p| p.ends_with("linked.org"))
                && h["matched"] == Value::String("The Shepherd Psalm".into())
        });
        assert!(
            !in_linked,
            "alias match inside `[[...]]` must be filtered out, got: {arr:?}"
        );
    })
    .await;
}
