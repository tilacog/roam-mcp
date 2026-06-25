//! End-to-end MCP test: spin up the server in-process, drive it with the
//! rmcp client, and assert on tool results.

mod common;

use std::path::PathBuf;

use rmcp::model::CallToolRequestParams;
use rmcp::object;
use tempfile::TempDir;

use common::{run_with_server, text_of};
use org_roam_mcp::{Config, RoamServer};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-vault")
}

#[tokio::test]
async fn server_ping_works() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(CallToolRequestParams::new("ping").with_arguments(object!({})))
            .await
            .expect("ping call");
        let text = text_of(&result);
        assert!(
            text.contains("pong"),
            "ping should return pong, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_search_nodes_finds_psalm() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("search_nodes")
                    .with_arguments(object!({ "query": "canticle", "limit": 10 })),
            )
            .await
            .expect("search call");
        let text = text_of(&result);
        assert!(
            text.contains("Pastafarian Canticle"),
            "expected Pastafarian Canticle in results, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_get_node_returns_metadata_and_body() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_node")
                    .with_arguments(object!({ "id": "11111111-1111-1111-1111-111111111111" })),
            )
            .await
            .expect("get_node call");
        let text = text_of(&result);
        assert!(
            text.contains("\"title\""),
            "expected node metadata, got: {text}"
        );
        assert!(
            text.contains("\"body\""),
            "expected body field, got: {text}"
        );
        assert!(
            text.contains("shadow of the meatball"),
            "body should contain the file content, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_get_node_on_headline_node_returns_subtree_body() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_node")
                    .with_arguments(object!({ "id": "55555555-5555-5555-5555-555555555555" })),
            )
            .await
            .expect("get_node call");
        let text = text_of(&result);
        assert!(
            text.contains("culinary motif"),
            "body should contain the subtree content, got: {text}"
        );
        assert!(
            !text.contains("ROAM_TAGS"),
            "headline-node body must not leak the whole file, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_get_node_section_resolves_dedicated_target() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_node_section").with_arguments(object!({
                    "id": "11111111-1111-1111-1111-111111111111",
                    "anchor": "verse-4",
                })),
            )
            .await
            .expect("section call");
        let text = text_of(&result);
        assert!(
            text.contains("shadow of the meatball"),
            "anchor should resolve to verse 4, got: {text}"
        );
        assert!(
            text.contains("\"kind\""),
            "expected anchor kind metadata, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_get_backlinks_finds_linking_node() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_backlinks")
                    .with_arguments(object!({ "id": "22222222-2222-2222-2222-222222222222" })),
            )
            .await
            .expect("backlinks call");
        let text = text_of(&result);
        assert!(
            text.contains("11111111-1111-1111-1111-111111111111"),
            "fsm_canticle should backlink to noodly, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_find_by_ref_returns_node() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(CallToolRequestParams::new("find_by_ref").with_arguments(
                object!({ "ref": "https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster" }),
            ))
            .await
            .expect("by_ref call");
        let text = text_of(&result);
        assert!(text.contains("11111111-1111-1111-1111-111111111111"));
    })
    .await;
}

#[tokio::test]
async fn server_list_tags_includes_pastafarianism() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(CallToolRequestParams::new("list_tags").with_arguments(object!({})))
            .await
            .expect("tags call");
        let text = text_of(&result);
        assert!(
            text.contains("pastafarianism"),
            "expected 'pastafarianism' tag, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_read_resource_returns_node_body() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let uri = "org-roam://node/11111111-1111-1111-1111-111111111111";
        let result = peer
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri))
            .await
            .expect("read_resource call");
        let mut text = String::new();
        for c in result.contents {
            if let rmcp::model::ResourceContents::TextResourceContents { text: t, .. } = c {
                text.push_str(&t);
            }
        }
        assert!(
            text.contains("Pastafarian Canticle"),
            "expected Pastafarian Canticle in resource body, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn server_random_node_returns_node() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(CallToolRequestParams::new("random_node").with_arguments(object!({})))
            .await
            .expect("random_node call");
        let text = text_of(&result);
        // Should return a node with metadata and id, at minimum, an id and title
        assert!(
            text.contains("\"id\"") && text.contains("\"title\""),
            "expected node with id and title, got: {text}"
        );
    })
    .await;
}

// --- §1: anchor prefix syntax is accepted server-side -------------------

#[tokio::test]
async fn server_get_node_section_accepts_anchor_prefixes() {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, |peer| async move {
        // `#v4` is org's typed form for a CUSTOM_ID search and must
        // resolve the same as the bare `v4`.
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_node_section").with_arguments(object!({
                    "id": "11111111-1111-1111-1111-111111111111",
                    "anchor": "#v4",
                })),
            )
            .await
            .expect("section call");
        let text = text_of(&result);
        assert!(
            text.contains("shadow of the meatball"),
            "anchor #v4 should resolve to the v4 verse, got: {text}"
        );
        assert!(
            text.contains("\"kind\""),
            "expected anchor kind metadata, got: {text}"
        );
    })
    .await;
}

// --- §4: in-body org-cite surfaces through find_by_ref ---------------

fn body_cite_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("body-cite.org"),
        ":PROPERTIES:\n:ID: 77777777-7777-7777-7777-777777777777\n:END:\n\
         #+title: Body cite\n\n\
         A literature note that mentions [cite:@nora2023; @smith2020 p. 41] \
         in the body but does not declare them in ROAM_REFS.\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn server_find_by_ref_finds_in_body_citation() {
    let dir = body_cite_dir();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("find_by_ref")
                    .with_arguments(object!({ "ref": "@nora2023" })),
            )
            .await
            .expect("find_by_ref call");
        let text = text_of(&result);
        assert!(
            text.contains("77777777-7777-7777-7777-777777777777"),
            "in-body citation should be findable, got: {text}"
        );
    })
    .await;
}

// --- §6: get_forward_links distinguishes name from fuzzy -------------

fn named_table_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("named.org"),
        ":PROPERTIES:\n:ID: 88888888-8888-8888-8888-888888888888\n:END:\n\
         #+title: Named\n\n\
         #+NAME: growth-table\n\
         | year | nodes |\n\
         |------+-------|\n\
         | 2024 | 2     |\n\n\
         See [[growth-table]] for the data, and [[unknown]] for missing.\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn server_get_forward_links_distinguishes_name_fuzzy() {
    let dir = named_table_dir();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_forward_links")
                    .with_arguments(object!({ "id": "88888888-8888-8888-8888-888888888888" })),
            )
            .await
            .expect("forward links");
        let text = text_of(&result);
        // The intra-file link to a `#+NAME:` is `kind: "name"`.
        assert!(
            text.contains("\"name\""),
            "expected a `name` kind, got: {text}"
        );
        // The unresolvable `[[unknown]]` is `kind: "fuzzy"`.
        assert!(
            text.contains("\"fuzzy\""),
            "expected a `fuzzy` kind, got: {text}"
        );
    })
    .await;
}
