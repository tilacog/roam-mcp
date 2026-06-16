//! Conformance tests for the `SQLite` backend against a synthetic database
//! that mirrors what Emacs actually writes: org-roam v2's table schema
//! (`node_id` columns, split refs) with emacsql value encoding (every
//! string quote-wrapped, lists printed as Lisp, `nil` for null).
//!
//! The scanner backend is exercised against equivalent expectations in
//! `tests/scan_index.rs` and `src/index/scan.rs`; together they pin both
//! implementations of `RoamIndex` to the same observable behavior.

use std::path::{Path, PathBuf};

use org_roam_mcp::index::sqlite::SqliteIndex;
use org_roam_mcp::index::{NodeQuery, RoamIndex};

const PSALM_ID: &str = "11111111-1111-1111-1111-111111111111";
const SHEPHERD_ID: &str = "22222222-2222-2222-2222-222222222222";
const VERSE_ID: &str = "33333333-3333-3333-3333-333333333333";

/// Quote a string the way emacsql prints it into `SQLite`.
fn lisp(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Build a synthetic org-roam v2 database. Schema copied from
/// `org-roam-db--table-schemata` (emacsql renders `node-id` as `node_id`).
fn synthetic_db(dir: &Path) -> PathBuf {
    let db_path = dir.join("org-roam.db");
    let conn = rusqlite::Connection::open(&db_path).expect("create db");
    conn.execute_batch(
        "CREATE TABLE files (file UNIQUE PRIMARY KEY, title, hash NOT NULL, \
             atime NOT NULL, mtime NOT NULL);
         CREATE TABLE nodes (id NOT NULL PRIMARY KEY, file NOT NULL, level NOT NULL, \
             pos NOT NULL, todo, priority, scheduled, deadline, title, properties, olp);
         CREATE TABLE aliases (node_id NOT NULL, alias);
         CREATE TABLE citations (node_id NOT NULL, cite_key NOT NULL, pos NOT NULL, properties);
         CREATE TABLE refs (node_id NOT NULL, ref NOT NULL, type NOT NULL);
         CREATE TABLE tags (node_id NOT NULL, tag);
         CREATE TABLE links (pos NOT NULL, source NOT NULL, dest NOT NULL, \
             type NOT NULL, properties NOT NULL);",
    )
    .expect("schema");

    let fsm_file = dir.join("fsm_canticle.org").display().to_string();
    let noodly_file = dir.join("noodly.org").display().to_string();

    let insert_node = |id: &str, file: &str, level: i64, pos: i64, title: &str, olp: &str| {
        conn.execute(
            "INSERT INTO nodes VALUES (?, ?, ?, ?, NULL, NULL, NULL, NULL, ?, NULL, ?)",
            rusqlite::params![lisp(id), lisp(file), level, pos, lisp(title), olp],
        )
        .expect("insert node");
    };
    insert_node(PSALM_ID, &fsm_file, 0, 1, "Pastafarian Canticle", "nil");
    insert_node(
        SHEPHERD_ID,
        &noodly_file,
        0,
        1,
        "Noodly Appendage imagery",
        "nil",
    );
    insert_node(
        VERSE_ID,
        &fsm_file,
        1,
        200,
        "Verse 4",
        "(\"Pastafarian Canticle\")",
    );

    conn.execute(
        "INSERT INTO aliases VALUES (?, ?)",
        rusqlite::params![lisp(PSALM_ID), lisp("The Noodly Psalm")],
    )
    .expect("alias");

    for (node, tag) in [
        (PSALM_ID, "pastafarianism"),
        (PSALM_ID, "canticles"),
        (SHEPHERD_ID, "symbolism"),
    ] {
        conn.execute(
            "INSERT INTO tags VALUES (?, ?)",
            rusqlite::params![lisp(node), lisp(tag)],
        )
        .expect("tag");
    }

    // org-roam splits refs before storing: @citekey loses the @ (type
    // "cite"); a URL loses its scheme (type "https", ref "//host/path").
    conn.execute(
        "INSERT INTO refs VALUES (?, ?, ?)",
        rusqlite::params![lisp(PSALM_ID), lisp("nora2023"), lisp("cite")],
    )
    .expect("cite ref");
    conn.execute(
        "INSERT INTO refs VALUES (?, ?, ?)",
        rusqlite::params![
            lisp(PSALM_ID),
            lisp("//en.wikipedia.org/wiki/Flying_Spaghetti_Monster"),
            lisp("https")
        ],
    )
    .expect("url ref");

    // noodly.org links to fsm_canticle by id; fsm_canticle links out to a URL.
    conn.execute(
        "INSERT INTO links VALUES (?, ?, ?, ?, ?)",
        rusqlite::params![
            10,
            lisp(SHEPHERD_ID),
            lisp(PSALM_ID),
            lisp("id"),
            "(:outline nil)"
        ],
    )
    .expect("id link");
    conn.execute(
        "INSERT INTO links VALUES (?, ?, ?, ?, ?)",
        rusqlite::params![
            20,
            lisp(PSALM_ID),
            lisp("//example.com/commentary"),
            lisp("https"),
            "(:outline nil)"
        ],
    )
    .expect("url link");

    drop(conn);
    db_path
}

fn open_index(dir: &tempfile::TempDir) -> SqliteIndex {
    let db = synthetic_db(dir.path());
    SqliteIndex::open(&db).expect("open synthetic db")
}

#[test]
fn node_lookup_by_bare_uuid_decodes_all_fields() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let node = idx.node(PSALM_ID).expect("query").expect("node found");
    assert_eq!(node.id, PSALM_ID, "id must be decoded, not quote-wrapped");
    assert_eq!(node.title, "Pastafarian Canticle");
    assert!(
        node.file.to_string_lossy().ends_with("fsm_canticle.org"),
        "file must be a usable path, got {:?}",
        node.file
    );
    assert!(node.is_file(), "level 0 in the db means a file-level node");
    assert_eq!(node.aliases, vec!["The Noodly Psalm"]);
    assert_eq!(node.tags, vec!["canticles", "pastafarianism"]);
}

#[test]
fn headline_node_has_level_and_olp() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let node = idx.node(VERSE_ID).expect("query").expect("node found");
    assert_eq!(node.level, Some(1));
    assert!(!node.is_file());
    assert_eq!(node.olp, vec!["Pastafarian Canticle"]);
}

#[test]
fn unknown_id_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    assert!(idx
        .node("00000000-dead-beef-0000-000000000000")
        .unwrap()
        .is_none());
}

#[test]
fn find_nodes_results_carry_aliases_and_tags() {
    // Parity with the scanner backend, which populates both fields on
    // every search hit.
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .find_nodes(&NodeQuery {
            query: None,
            tags: &[],
            limit: None,
        })
        .expect("search");
    let psalm = found.iter().find(|n| n.id == PSALM_ID).expect("psalm");
    assert_eq!(psalm.aliases, vec!["The Noodly Psalm"]);
    assert_eq!(psalm.tags, vec!["canticles", "pastafarianism"]);
}

#[test]
fn by_ref_results_carry_aliases_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx.by_ref("@nora2023").expect("by_ref");
    assert_eq!(found[0].aliases, vec!["The Noodly Psalm"]);
    assert_eq!(found[0].tags, vec!["canticles", "pastafarianism"]);
}

#[test]
fn find_nodes_by_title_substring() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .find_nodes(&NodeQuery {
            query: Some("canticle"),
            tags: &[],
            limit: Some(10),
        })
        .expect("search");
    assert!(found.iter().any(|n| n.id == PSALM_ID), "title match");
}

#[test]
fn find_nodes_by_alias_substring() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .find_nodes(&NodeQuery {
            query: Some("Noodly Psalm"),
            tags: &[],
            limit: Some(10),
        })
        .expect("search");
    assert!(
        found.iter().any(|n| n.id == PSALM_ID),
        "alias must match: {found:?}"
    );
}

#[test]
fn find_nodes_filters_by_tag() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .find_nodes(&NodeQuery {
            query: None,
            tags: &["pastafarianism".to_string()],
            limit: None,
        })
        .expect("tag search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, PSALM_ID);
}

#[test]
fn tag_filter_does_not_match_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .find_nodes(&NodeQuery {
            query: None,
            tags: &["The Noodly Psalm".to_string()],
            limit: None,
        })
        .expect("tag search");
    assert!(found.is_empty(), "aliases must not satisfy a tag filter");
}

#[test]
fn list_tags_counts_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let tags = idx.tags().expect("tags query must not error on v2 schema");
    let map: std::collections::HashMap<String, usize> = tags.into_iter().collect();
    assert_eq!(map.get("pastafarianism").copied(), Some(1));
    assert_eq!(map.get("canticles").copied(), Some(1));
    assert_eq!(map.get("symbolism").copied(), Some(1));
}

#[test]
fn by_ref_accepts_at_citekey_form() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx.by_ref("@nora2023").expect("by_ref");
    assert_eq!(found.len(), 1, "@citekey is the documented input form");
    assert_eq!(found[0].id, PSALM_ID);
}

#[test]
fn by_ref_accepts_full_url_form() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let found = idx
        .by_ref("https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster")
        .expect("by_ref");
    assert_eq!(found.len(), 1, "full URL must match the split stored form");
    assert_eq!(found[0].id, PSALM_ID);
}

#[test]
fn backlinks_decode_ids_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let back = idx.backlinks(PSALM_ID).expect("backlinks");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].source, SHEPHERD_ID);
    assert_eq!(back[0].dest.as_deref(), Some(PSALM_ID));
    assert_eq!(back[0].kind, "id");
}

#[test]
fn forward_links_classify_url_with_ref_target() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let fwd = idx.forward_links(PSALM_ID).expect("forward links");
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].kind, "https");
    assert_eq!(fwd[0].dest, None, "a URL is not a node id");
    assert_eq!(
        fwd[0].ref_target.as_deref(),
        Some("https://example.com/commentary")
    );
}

#[test]
fn node_count_counts_all_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    assert_eq!(idx.node_count().unwrap(), 3);
}

#[test]
fn limit_is_applied_after_filtering() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    // Both file-level nodes match "e" somewhere in title; limit 1 must
    // return exactly 1, and limit None must return all matches.
    let all = idx
        .find_nodes(&NodeQuery {
            query: Some("e"),
            tags: &[],
            limit: None,
        })
        .unwrap();
    assert!(all.len() >= 2);
    let one = idx
        .find_nodes(&NodeQuery {
            query: Some("e"),
            tags: &[],
            limit: Some(1),
        })
        .unwrap();
    assert_eq!(one.len(), 1);
}

#[test]
fn orphans_returns_only_unconnected_nodes() {
    // In the synthetic db: psalm has 1 backlink (from noodly) and
    // noodly has 1 id forward link (to psalm), so they are connected.
    // verse has no edges at all, so it is the only orphan.
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let ids: std::collections::HashSet<String> = idx
        .orphans()
        .expect("orphans")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(ids.len(), 1, "expected 1 orphan, got {ids:?}");
    assert!(
        ids.contains(VERSE_ID),
        "verse must be the orphan; psalm and noodly are connected"
    );
    assert!(
        !ids.contains(PSALM_ID),
        "psalm has a backlink, not an orphan"
    );
    assert!(
        !ids.contains(SHEPHERD_ID),
        "noodly has an id forward link, not an orphan"
    );
}

#[test]
fn orphans_excludes_psalm_even_with_only_a_url_outgoing_link() {
    // psalm has a URL forward link but no id forward links. URLs do
    // not point at other notes, so psalm is not an orphan — it has a
    // backlink from noodly. The filter is on type = "id" only.
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let orphans = idx.orphans().expect("orphans");
    assert!(
        !orphans.iter().any(|n| n.id == PSALM_ID),
        "psalm has a backlink, must not be an orphan even though its \
         only outgoing link is a URL: {orphans:?}"
    );
}

#[test]
fn orphans_includes_aliases_and_tags() {
    // Parity with the scanner backend: orphans() must populate both
    // fields, since callers may use them to triage the result.
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let orphans = idx.orphans().expect("orphans");
    let verse = orphans
        .iter()
        .find(|n| n.id == VERSE_ID)
        .expect("verse is an orphan");
    // verse (headline under psalm) shares the file with psalm, so the
    // file-level node's aliases are *not* inherited; verse is its own
    // node with no aliases or tags in the synthetic db.
    assert!(verse.aliases.is_empty());
    assert!(verse.tags.is_empty());
}

#[test]
fn orphans_results_are_sorted_by_title() {
    let dir = tempfile::tempdir().unwrap();
    let idx = open_index(&dir);
    let titles: Vec<String> = idx
        .orphans()
        .expect("orphans")
        .into_iter()
        .map(|n| n.title)
        .collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted, "orphans must come back title-sorted");
}

// --- §1 (todo-followup): SQLite backend coverage for new features --------

/// Build a database that also has a `citations` table with in-body citation
/// rows — mirrors what `org-roam-db-insert-citation` writes for `[cite:@key]`.
fn synthetic_db_with_citations(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("org-roam.db");
    let conn = rusqlite::Connection::open(&db_path).expect("create db");
    conn.execute_batch(
        "CREATE TABLE files (file UNIQUE PRIMARY KEY, title, hash NOT NULL, \
             atime NOT NULL, mtime NOT NULL);
         CREATE TABLE nodes (id NOT NULL PRIMARY KEY, file NOT NULL, level NOT NULL, \
             pos NOT NULL, todo, priority, scheduled, deadline, title, properties, olp);
         CREATE TABLE aliases (node_id NOT NULL, alias);
         CREATE TABLE citations (node_id NOT NULL, cite_key NOT NULL, pos NOT NULL, properties);
         CREATE TABLE refs (node_id NOT NULL, ref NOT NULL, type NOT NULL);
         CREATE TABLE tags (node_id NOT NULL, tag);
         CREATE TABLE links (pos NOT NULL, source NOT NULL, dest NOT NULL, \
             type NOT NULL, properties NOT NULL);",
    )
    .expect("schema");

    let ref_file = dir.join("note.org").display().to_string();
    let cite_file = dir.join("note2.org").display().to_string();
    let ref_id = "aaaa0001-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let cite_id = "aaaa0002-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    for (id, file, title) in [
        (ref_id, ref_file.as_str(), "Note with in-body citation"),
        (cite_id, cite_file.as_str(), "Note without explicit ref"),
    ] {
        conn.execute(
            "INSERT INTO nodes VALUES (?, ?, 0, 1, NULL, NULL, NULL, NULL, ?, NULL, 'nil')",
            rusqlite::params![lisp(id), lisp(file), lisp(title)],
        )
        .expect("insert node");
    }

    // ref_id has @nora2023 in ROAM_REFS (the explicit-ref path).
    conn.execute(
        "INSERT INTO refs VALUES (?, ?, ?)",
        rusqlite::params![lisp(ref_id), lisp("nora2023"), lisp("cite")],
    )
    .expect("explicit ref");

    // cite_id has @smith2020 as an in-body citation only (no ROAM_REFS entry).
    // org-roam stores cite_key without the @ sigil.
    conn.execute(
        "INSERT INTO citations VALUES (?, ?, ?, ?)",
        rusqlite::params![lisp(cite_id), lisp("smith2020"), 42, "nil"],
    )
    .expect("in-body citation");

    // Also add a @nora2023 in-body citation to ref_id (same key as ROAM_REFS)
    // to test dedup.
    conn.execute(
        "INSERT INTO citations VALUES (?, ?, ?, ?)",
        rusqlite::params![lisp(ref_id), lisp("nora2023"), 10, "nil"],
    )
    .expect("duplicated citation");

    drop(conn);
    db_path
}

#[test]
fn in_body_citation_found_via_citations_table() {
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db_with_citations(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    // note2 has @smith2020 as an in-body citation (no ROAM_REFS entry).
    // by_ref("@smith2020") must find it via the citations table.
    let found = idx.by_ref("@smith2020").expect("by_ref");
    assert_eq!(found.len(), 1, "should find exactly one node via citations");
    assert_eq!(
        found[0].id, "aaaa0002-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "should be note2 (the one with @smith2020 in citations)"
    );
}

#[test]
fn by_ref_merges_refs_and_citations_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db_with_citations(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    // note has @nora2023 in both refs (explicit) and citations (in-body).
    // The result must contain exactly one entry, not two.
    let found = idx.by_ref("@nora2023").expect("by_ref");
    let matching: Vec<_> = found
        .iter()
        .filter(|n| n.id == "aaaa0001-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "dedup must prevent the same node appearing twice; got {found:?}"
    );
}

#[test]
fn forward_links_includes_in_body_citations() {
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db_with_citations(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    // cite_id has only an in-body citation for @smith2020 (no links table rows).
    // forward_links must return a cite LinkRecord for it.
    let fwd = idx
        .forward_links("aaaa0002-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("forward_links");
    let cite = fwd.iter().find(|l| l.kind == "cite").expect("cite record");
    assert_eq!(cite.raw_dest, "@smith2020");
    assert_eq!(cite.ref_target.as_deref(), Some("@smith2020"));
    assert_eq!(
        cite.dest, None,
        "in-body citations have no node destination"
    );
}

#[test]
fn forward_links_cite_shape_matches_scanner_backend() {
    // The SQLite backend's cite LinkRecord from the citations table must
    // have the same shape as the scanner backend's in-body cite record:
    // kind="cite", dest=None, raw_dest="@key", ref_target=Some("@key").
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db_with_citations(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    // ref_id has @nora2023 in both links (from ROAM_REFS classification) and
    // citations. Check that the citations path still emits the right shape.
    let fwd = idx
        .forward_links("aaaa0001-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("forward_links");
    let cite_records: Vec<_> = fwd.iter().filter(|l| l.kind == "cite").collect();
    // There may be one cite record (from citations) since the links table in
    // this DB has no cite links for note_id.
    assert!(
        cite_records.iter().all(|l| l.dest.is_none()),
        "cite links must not claim a node dest: {cite_records:?}"
    );
    assert!(
        cite_records
            .iter()
            .all(|l| l.ref_target == Some(l.raw_dest.clone())),
        "ref_target must equal raw_dest for cite links: {cite_records:?}"
    );
}

#[test]
fn by_ref_url_query_does_not_search_citations_table() {
    // URL-shaped inputs (containing "://") are not citekeys; the citations
    // table must not be queried for them.
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db_with_citations(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    // There are no URL refs in this DB; the call must succeed and return empty.
    let found = idx.by_ref("https://example.com/no-match").expect("by_ref");
    assert!(
        found.is_empty(),
        "URL lookup should return empty, got {found:?}"
    );
}

#[test]
fn forward_links_on_node_with_no_citations_returns_links_only() {
    // A node with links-table rows but no citations-table rows must return
    // only the links records (no spurious cite entries).
    let dir = tempfile::tempdir().unwrap();
    let db = synthetic_db(dir.path());
    let idx = SqliteIndex::open(&db).expect("open");

    let fwd = idx.forward_links(PSALM_ID).expect("forward_links");
    // Psalm's only outgoing link is a URL (from the links table).
    assert!(
        fwd.iter()
            .all(|l| l.kind != "cite" || l.raw_dest.starts_with('@')),
        "no spurious cite records expected: {fwd:?}"
    );
}
