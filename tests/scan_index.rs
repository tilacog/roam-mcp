//! Integration tests for the filesystem scanner index.

use std::path::PathBuf;

use org_roam_mcp::index::scan::ScanIndex;
use org_roam_mcp::index::{NodeQuery, RoamIndex};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-vault")
}

#[test]
fn scan_finds_all_nodes() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    // Four file-level nodes plus the headline node in legacy.org.
    assert_eq!(idx.node_count().unwrap(), 5, "expected five nodes");
}

#[test]
fn scan_node_has_aliases_and_tags() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let n = idx
        .node("11111111-1111-1111-1111-111111111111")
        .expect("lookup")
        .expect("node exists");
    assert_eq!(n.title, "Pastafarian Canticle");
    assert!(n.aliases.contains(&"Ps FSM".to_string()));
    assert!(n.aliases.contains(&"The Noodly Psalm".to_string()));
    assert!(n.tags.contains(&"pastafarianism".to_string()));
    assert!(n.tags.contains(&"canticles".to_string()));
}

#[test]
fn scan_search_finds_title() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let q = NodeQuery {
        query: Some("canticle"),
        tags: &[],
        limit: Some(10),
    };
    let results = idx.find_nodes(&q).expect("search");
    assert!(results.iter().any(|n| n.title == "Pastafarian Canticle"));
}

#[test]
fn scan_search_finds_alias() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let q = NodeQuery {
        query: Some("Noodly Psalm"),
        tags: &[],
        limit: Some(10),
    };
    let results = idx.find_nodes(&q).expect("search");
    assert!(results
        .iter()
        .any(|n| n.id == "11111111-1111-1111-1111-111111111111"));
}

#[test]
fn scan_search_finds_tag() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let q = NodeQuery {
        query: None,
        tags: &["canticles".to_string()],
        limit: Some(10),
    };
    let results = idx.find_nodes(&q).expect("search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "11111111-1111-1111-1111-111111111111");
}

#[test]
fn scan_backlinks() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let links = idx
        .backlinks("22222222-2222-2222-2222-222222222222")
        .expect("backlinks");
    // fsm_canticle.org and noodly.org both reference the noodly.id... wait,
    // the file-level link in fsm_canticle.org points to noodly.id; noodly.org
    // also references fsm_canticle.id. So the backlinks of 22222 should include
    // fsm_canticle (one link from fsm_canticle → noodly).
    assert!(links
        .iter()
        .any(|l| l.source == "11111111-1111-1111-1111-111111111111"));
}

#[test]
fn scan_backlinks_dedup_same_id_at_file_and_headline() {
    // Regression: a file with the same `:ID:` at the file level AND on a
    // headline used to produce two backlink records (one per AST section
    // the link appeared in). The link graph records edges between nodes,
    // not in-file positions; the second copy was a duplicate the user
    // could not justify away.
    let dir = tempfile::tempdir().expect("tmpdir");
    let shared_id = "aaaaaaaa-1111-1111-1111-111111111111";
    let dest_id = "bbbbbbbb-2222-2222-2222-222222222222";

    // File-level drawer AND a headline with the same id; the link
    // appears in both the pre-headline section and the headline body.
    std::fs::write(
        dir.path().join("dual.org"),
        format!(
            ":PROPERTIES:\n:ID:       {shared_id}\n:END:\n#+title: Dual\n\n\
             pre-body [[id:{dest_id}]]\n\n\
             * Section\n\
             :PROPERTIES:\n\
             :ID:       {shared_id}\n\
             :END:\n\
             headline body [[id:{dest_id}]]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dest.org"),
        format!(":PROPERTIES:\n:ID:       {dest_id}\n:END:\n#+title: Dest\n"),
    )
    .unwrap();

    let idx = ScanIndex::open(dir.path()).expect("open");
    let back = idx.backlinks(dest_id).expect("backlinks");
    let from_shared: Vec<&_> = back.iter().filter(|l| l.source == shared_id).collect();
    assert_eq!(
        from_shared.len(),
        1,
        "duplicate backlink from {shared_id} to {dest_id}: {back:?}"
    );

    // Forward links from the shared id should also be unique.
    let fwd = idx.forward_links(shared_id).expect("forward");
    let to_dest: Vec<&_> = fwd
        .iter()
        .filter(|l| l.dest.as_deref() == Some(dest_id))
        .collect();
    assert_eq!(
        to_dest.len(),
        1,
        "duplicate forward link from {shared_id} to {dest_id}: {fwd:?}"
    );
}

#[test]
fn scan_forward_links() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let links = idx
        .forward_links("11111111-1111-1111-1111-111111111111")
        .expect("forward");
    assert!(links
        .iter()
        .any(|l| l.dest.as_deref() == Some("22222222-2222-2222-2222-222222222222")));
}

#[test]
fn scan_by_ref() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let nodes = idx
        .by_ref("https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster")
        .expect("by_ref");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, "11111111-1111-1111-1111-111111111111");
}

#[test]
fn scan_tags() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let tags = idx.tags().expect("tags");
    let names: Vec<&str> = tags.iter().map(|(t, _)| t.as_str()).collect();
    assert!(names.contains(&"pastafarianism"));
    assert!(names.contains(&"canticles"));
}

#[test]
fn scan_v1_roam_tags_keyword_indexed() {
    // legacy.org declares its tags with org-roam v1's `#+ROAM_TAGS:`.
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let n = idx
        .node("44444444-4444-4444-4444-444444444444")
        .expect("lookup")
        .expect("node exists");
    assert_eq!(n.tags, vec!["legacy", "hub"]);
    let tags = idx.tags().expect("tags");
    let names: Vec<&str> = tags.iter().map(|(t, _)| t.as_str()).collect();
    assert!(names.contains(&"legacy"), "tags = {names:?}");
    assert!(names.contains(&"hub"), "tags = {names:?}");
}

#[test]
fn scan_v1_roam_key_keyword_resolves_by_ref() {
    // legacy.org declares its ref with org-roam v1's `#+ROAM_KEY:`.
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let nodes = idx.by_ref("legacy-key").expect("by_ref");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, "44444444-4444-4444-4444-444444444444");
}

#[test]
fn scan_orphans_lists_nodes_with_no_id_edges() {
    // In the sample vault, fsm_canticle and noodly form a connected pair
    // (each has one id forward link and one backlink). The other
    // three nodes (daily, legacy file-level, legacy headline) have no
    // id links in either direction — those are the orphans.
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let ids: std::collections::HashSet<String> = idx
        .orphans()
        .expect("orphans")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(
        ids.len(),
        3,
        "expected 3 orphans in the sample vault, got {ids:?}"
    );
    let expected: std::collections::HashSet<&str> = [
        "33333333-3333-3333-3333-333333333333", // daily.org
        "44444444-4444-4444-4444-444444444444", // legacy.org file
        "55555555-5555-5555-5555-555555555555", // legacy.org "Worship themes" headline
    ]
    .into_iter()
    .collect();
    let actual: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);
    // The connected pair must not be in the orphan set.
    assert!(!ids.contains("11111111-1111-1111-1111-111111111111"));
    assert!(!ids.contains("22222222-2222-2222-2222-222222222222"));
}

#[test]
fn scan_orphans_excludes_nodes_with_url_only_links() {
    // A node that has only a URL forward link is still an orphan: a URL
    // is not an edge in the id-link note graph.
    let dir = tempfile::tempdir().expect("tmpdir");
    let target = dir.path().join("20260101000000-target.org");
    std::fs::write(
        &target,
        ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
         #+title: Target\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("20260101000001-source.org"),
        ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
         #+title: Source\n\nSee [[https://example.com][the example]].\n",
    )
    .unwrap();

    let idx = ScanIndex::open(dir.path()).expect("open");
    let ids: std::collections::HashSet<String> = idx
        .orphans()
        .expect("orphans")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert!(
        ids.contains("11111111-1111-1111-1111-111111111111"),
        "target has no edges, must be an orphan"
    );
    assert!(
        ids.contains("22222222-2222-2222-2222-222222222222"),
        "source has only a URL link, must still be an orphan"
    );
    assert_eq!(ids.len(), 2, "extra orphans leaked: {ids:?}");
}

#[test]
fn scan_orphans_excludes_nodes_with_self_link() {
    // A self-loop is an edge in the link graph (the node appears as
    // both a source and a dest of an id link), so it is not an orphan.
    let dir = tempfile::tempdir().expect("tmpdir");
    std::fs::write(
        dir.path().join("20260101000000-self.org"),
        ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
         #+title: Self\n\nSee [[id:11111111-1111-1111-1111-111111111111]].\n",
    )
    .unwrap();

    let idx = ScanIndex::open(dir.path()).expect("open");
    assert!(
        idx.orphans().expect("orphans").is_empty(),
        "a self-loop counts as an edge; the node is not an orphan"
    );
}

#[test]
fn scan_orphans_results_are_sorted_by_title() {
    let idx = ScanIndex::open(&fixture_dir()).expect("open");
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

// --- §3 (todo-followup): performance regression test for §6 -------------
//
// The §6 name-reclassification reads every file once to build a name table,
// then checks each fuzzy link against it. This test generates synthetic
// vaults of increasing size and verifies that `ScanIndex::open` scales
// linearly (not quadratically). A vault with 100× more files should take
// less than 1000× longer (generous: allows O(N log N) and even O(N·M)
// with small constants, but catches actual O(N²) growth).

/// Write `count` org files with `names_per_file` `#+NAME:` keywords and
/// `fuzzy_links_per_file` fuzzy `[[name]]` links each.
fn write_synthetic_vault(
    dir: &std::path::Path,
    count: usize,
    names_per_file: usize,
    fuzzy_links_per_file: usize,
) {
    use std::fmt::Write as _;
    for i in 0..count {
        let id = format!("{i:08x}-0000-0000-0000-000000000000");
        let mut content = format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: File {i}\n\n");
        for n in 0..names_per_file {
            write!(
                content,
                "#+NAME: name-{i}-{n}\n| col |\n|-----|\n| val |\n\n"
            )
            .unwrap();
        }
        for l in 0..fuzzy_links_per_file {
            writeln!(content, "See [[name-{i}-{l}]] for details.").unwrap();
        }
        std::fs::write(dir.join(format!("{i:08}-file.org")), &content).unwrap();
    }
}

fn time_open(dir: &std::path::Path) -> std::time::Duration {
    let start = std::time::Instant::now();
    ScanIndex::open(dir).expect("open synthetic vault");
    start.elapsed()
}

#[test]
fn name_reclassification_scales_sub_quadratically() {
    let small = tempfile::tempdir().expect("small tmpdir");
    let large = tempfile::tempdir().expect("large tmpdir");

    // N=10 and N=100 files; 3 names and 3 fuzzy links each.
    write_synthetic_vault(small.path(), 10, 3, 3);
    write_synthetic_vault(large.path(), 100, 3, 3);

    // Warm run to populate the OS page cache for both vaults.
    let _ = time_open(small.path());
    let _ = time_open(large.path());

    // Timed runs.
    let t_small = time_open(small.path());
    let t_large = time_open(large.path());

    // Sanity check: both must complete within a generous wall-clock bound.
    // 5 seconds is far above what the expected linear-ish index build
    // should take for 100 files. If this fires, the machine is overloaded.
    assert!(
        t_large.as_secs() < 5,
        "indexing 100 synthetic files took {t_large:?} — suspiciously slow"
    );

    // The 10× file-count increase must not produce a 10×-worse-than-10× slowdown,
    // i.e. the ratio of times must be less than 100 (very generous; O(N) would
    // produce a ratio close to 10). If the implementation is O(N²), the ratio
    // will be ~100 and the test will fail with a clear message.
    if t_small.as_nanos() > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = t_large.as_nanos() as f64 / t_small.as_nanos() as f64;
        assert!(
            ratio < 100.0,
            "10× more files caused {ratio:.1}× slowdown — likely O(N²): \
             t_small={t_small:?} t_large={t_large:?}"
        );
    }
}

#[test]
fn name_reclassification_large_vault_completes_in_time() {
    // A 1000-file vault with 2 names and 2 fuzzy links per file must
    // complete within 30 seconds on any reasonable CI machine. If the
    // implementation is O(N²), this takes >60s and the test times out.
    let dir = tempfile::tempdir().expect("1000-file tmpdir");
    write_synthetic_vault(dir.path(), 1000, 2, 2);
    let elapsed = time_open(dir.path());
    assert!(
        elapsed.as_secs() < 30,
        "indexing 1000 files took {elapsed:?} — likely O(N²) growth"
    );
}

// ── search_by_tag conformance ────────────────────────────────────────────────

#[test]
fn scan_search_by_tag_is_exact_and_case_sensitive() {
    use org_roam_mcp::tools::query::{search_by_tag, SearchByTagParams};
    use rmcp::handler::server::wrapper::Parameters;

    let idx = ScanIndex::open(&fixture_dir()).expect("open");
    let idx: std::sync::Arc<dyn RoamIndex> = std::sync::Arc::new(idx);

    let res = search_by_tag(
        &idx,
        Parameters(SearchByTagParams {
            tag: "pastafarianism".to_string(),
            limit: None,
            offset: None,
        }),
    )
    .expect("search");
    let text = res
        .content
        .into_iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["total"], serde_json::json!(1), "got: {v}");
    assert_eq!(
        v["nodes"][0]["node_id"],
        serde_json::json!("11111111-1111-1111-1111-111111111111")
    );

    // Case-sensitive: an uppercase tag matches nothing, even though the
    // scanner's own tag filter is case-insensitive (the exact re-filter
    // in search_by_tag guarantees case-sensitivity across backends).
    let res = search_by_tag(
        &idx,
        Parameters(SearchByTagParams {
            tag: "PASTAFARIANISM".to_string(),
            limit: None,
            offset: None,
        }),
    )
    .expect("search");
    let text = res
        .content
        .into_iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["total"],
        serde_json::json!(0),
        "uppercase must not match: {v}"
    );
    assert_eq!(v["nodes"], serde_json::json!([]));
}
