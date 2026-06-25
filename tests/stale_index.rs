//! End-to-end test of the stale-index fallback signal.
//!
//! When the index still lists an id as a headline node but the file no
//! longer contains that headline, `read_node_body` falls back to the whole
//! file. These tests inject such a stale index via `RoamServer::with_index`
//! and assert the fallback is *reported* (not silently returned) through
//! both a tool (`get_node`) and a prompt (`summarize-node`).

mod common;

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, GetPromptRequestParams};
use rmcp::object;
use tempfile::TempDir;

use common::{prompt_text, run_with_server, text_of};
use org_roam_mcp::index::{IndexResult, LinkRecord, NodeMeta, NodeQuery, RoamIndex};
use org_roam_mcp::{Config, RoamServer};

/// A headline id the index knows but the on-disk file does not contain.
const STALE_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

/// Index that reports one headline node whose file lacks that headline.
struct StaleIndex {
    meta: NodeMeta,
}

impl RoamIndex for StaleIndex {
    fn node(&self, id: &str) -> IndexResult<Option<NodeMeta>> {
        Ok((id == self.meta.id).then(|| self.meta.clone()))
    }
    fn find_nodes(&self, _q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>> {
        Ok(vec![self.meta.clone()])
    }
    fn backlinks(&self, _id: &str) -> IndexResult<Vec<LinkRecord>> {
        Ok(Vec::new())
    }
    fn forward_links(&self, _id: &str) -> IndexResult<Vec<LinkRecord>> {
        Ok(Vec::new())
    }
    fn by_ref(&self, _r: &str) -> IndexResult<Vec<NodeMeta>> {
        Ok(Vec::new())
    }
    fn tags(&self) -> IndexResult<Vec<(String, usize)>> {
        Ok(Vec::new())
    }
    fn node_count(&self) -> IndexResult<usize> {
        Ok(1)
    }
    fn orphans(&self) -> IndexResult<Vec<NodeMeta>> {
        Ok(Vec::new())
    }
    fn random_node(&self) -> IndexResult<NodeMeta> {
        Ok(self.meta.clone())
    }
    fn node_by_path(&self, _path: &std::path::Path) -> IndexResult<Option<NodeMeta>> {
        Ok(None)
    }
    fn nodes_with_external_links(&self) -> IndexResult<Vec<(NodeMeta, Vec<LinkRecord>)>> {
        Ok(Vec::new())
    }
    fn source(&self) -> &'static str {
        "stale"
    }
}

/// Build a server whose index points `STALE_ID` (a headline node) at a file
/// that only has a file-level node — forcing the whole-file fallback.
fn stale_server(dir: &TempDir) -> RoamServer {
    let path = dir.path().join("n.org");
    std::fs::write(
        &path,
        ":PROPERTIES:\n:ID: ffffffff-ffff-ffff-ffff-ffffffffffff\n:END:\n\
         #+title: File\n\nBody text here.\n",
    )
    .unwrap();
    let meta = NodeMeta {
        id: STALE_ID.to_string(),
        file: path,
        title: "Vanished heading".to_string(),
        level: Some(1),
        todo: None,
        priority: None,
        olp: Vec::new(),
        pos: Some(0),
        aliases: Vec::new(),
        tags: Vec::new(),
    };
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    RoamServer::with_index(cfg, Arc::new(StaleIndex { meta }))
}

#[tokio::test]
async fn get_node_reports_stale_index_fallback() {
    let dir = TempDir::new().unwrap();
    let server = stale_server(&dir);
    run_with_server(server, |peer| async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_node").with_arguments(object!({ "id": STALE_ID })),
            )
            .await
            .expect("get_node call");
        let text = text_of(&result);
        assert!(
            text.contains("\"warning\""),
            "get_node should carry a warning field on stale fallback, got: {text}"
        );
        assert!(
            text.contains("whole file"),
            "the warning should explain the whole-file fallback, got: {text}"
        );
        // The fallback body is the entire file.
        assert!(
            text.contains("Body text here."),
            "fallback should return the whole file body, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn summarize_node_prompt_notes_stale_index_fallback() {
    let dir = TempDir::new().unwrap();
    let server = stale_server(&dir);
    run_with_server(server, |peer| async move {
        let result = peer
            .get_prompt(
                GetPromptRequestParams::new("summarize-node")
                    .with_arguments(object!({ "id": STALE_ID })),
            )
            .await
            .expect("get_prompt summarize-node");
        let text = prompt_text(&result);
        assert!(
            text.contains("[note:") && text.contains("whole file"),
            "the prompt should flag the stale fallback, got: {text}"
        );
    })
    .await;
}
