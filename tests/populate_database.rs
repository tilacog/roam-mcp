use std::path::PathBuf;

use org_roam_mcp::index::populate::{populate_database, PopulateOptions};
use org_roam_mcp::index::sqlite::SqliteIndex;
use org_roam_mcp::index::{NodeQuery, RoamIndex};
use rusqlite::Connection;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("vault")
}

#[test]
fn populate_creates_readable_database() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: false,
    };

    let stats = populate_database(&fixture_dir(), &options).expect("populate should succeed");

    // The sample vault has multiple files and nodes; just assert that we
    // wrote something reasonable and the SQLite backend can read it back.
    assert!(stats.files > 0, "expected at least one file");
    assert!(
        stats.nodes >= stats.files,
        "expected at least as many nodes as files"
    );

    let idx = SqliteIndex::open(&db_path).expect("open populated db");
    let count = idx.node_count().expect("node_count");
    assert_eq!(count, stats.nodes, "validated node count should match");

    let all = idx.find_nodes(&NodeQuery::default()).expect("list nodes");
    assert_eq!(all.len(), stats.nodes);
}

#[test]
fn populate_refuses_to_overwrite_without_flag() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    std::fs::write(&db_path, "not a real db").unwrap();

    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: false,
    };

    let err = populate_database(&fixture_dir(), &options).expect_err("should fail when db exists");
    assert!(
        err.to_string().contains("already exists"),
        "error should mention existing db: {err}"
    );
}

#[test]
fn populate_overwrites_when_asked() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    std::fs::write(&db_path, "not a real db").unwrap();

    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: true,
    };

    let stats = populate_database(&fixture_dir(), &options).expect("populate with overwrite");
    assert!(stats.files > 0);

    // Original file should have been backed up, not left as stale bytes.
    let content = std::fs::read(&db_path).unwrap();
    assert!(content.starts_with(b"SQLite format 3"));
}

#[test]
fn populate_preserves_links_and_refs() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: false,
    };

    // Build a tiny vault with two linked nodes and a ref.
    let vault = dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(
        vault.join("a.org"),
        ":PROPERTIES:\n:ID: aaaa1111-1111-1111-1111-111111111111\n:ROAM_REFS: https://example.com\n:END:\n#+title: Alpha\n\nLink to [[id:bbbb2222-2222-2222-2222-222222222222]].\n",
    )
    .unwrap();
    std::fs::write(
        vault.join("b.org"),
        ":PROPERTIES:\n:ID: bbbb2222-2222-2222-2222-222222222222\n:END:\n#+title: Beta\n\nBack to [[id:aaaa1111-1111-1111-1111-111111111111]].\n",
    )
    .unwrap();

    let stats = populate_database(&vault, &options).expect("populate tiny vault");
    assert_eq!(stats.files, 2);
    assert_eq!(stats.nodes, 2);
    assert_eq!(stats.links, 2);
    assert_eq!(stats.refs, 1);

    let idx = SqliteIndex::open(&db_path).expect("open populated db");
    let alpha_backlinks = idx
        .backlinks("aaaa1111-1111-1111-1111-111111111111")
        .unwrap();
    assert_eq!(alpha_backlinks.len(), 1);
    assert_eq!(
        alpha_backlinks[0].source,
        "bbbb2222-2222-2222-2222-222222222222"
    );

    let by_url = idx.by_ref("https://example.com").unwrap();
    assert_eq!(by_url.len(), 1);
    assert_eq!(by_url[0].id, "aaaa1111-1111-1111-1111-111111111111");
}

#[test]
fn populate_stores_link_and_citation_positions() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: false,
    };

    let vault = dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(
        vault.join("a.org"),
        ":PROPERTIES:\n\
         :ID: aaaa1111-1111-1111-1111-111111111111\n\
         :END:\n\
         #+title: Alpha\n\n\
         Link to [[id:bbbb2222-2222-2222-2222-222222222222]].\n\
         [cite:@nora2023]\n",
    )
    .unwrap();
    std::fs::write(
        vault.join("b.org"),
        ":PROPERTIES:\n\
         :ID: bbbb2222-2222-2222-2222-222222222222\n\
         :END:\n\
         #+title: Beta\n",
    )
    .unwrap();

    let stats = populate_database(&vault, &options).expect("populate vault with positions");
    assert_eq!(stats.links, 2);
    assert_eq!(stats.citations, 1);

    let conn = Connection::open(&db_path).unwrap();

    let mut stmt = conn
        .prepare("SELECT pos, type FROM links WHERE source = ? ORDER BY pos")
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map(["\"aaaa1111-1111-1111-1111-111111111111\""], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(rows.len(), 2);
    let id_link = rows.iter().find(|(_, t)| t == "\"id\"").unwrap();
    let cite_link = rows.iter().find(|(_, t)| t == "\"cite\"").unwrap();
    assert!(id_link.0 > 0, "id link position should be positive");
    assert!(cite_link.0 > 0, "cite link position should be positive");
    assert!(cite_link.0 > id_link.0, "citation comes after the id link");

    let cite_pos: i64 = conn
        .query_row(
            "SELECT pos FROM citations WHERE node_id = ? AND cite_key = ?",
            ["\"aaaa1111-1111-1111-1111-111111111111\"", "\"nora2023\""],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cite_pos, cite_link.0,
        "citation table position matches the link"
    );

    let file_pos: i64 = conn
        .query_row(
            "SELECT pos FROM nodes WHERE id = ?",
            ["\"aaaa1111-1111-1111-1111-111111111111\""],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_pos, 1, "file-level node position matches org-roam");
}
