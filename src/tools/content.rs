//! Content / section tools: read a node or a sub-section of it.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::index::{IndexError, RoamIndex};
use crate::org::{AnchorResolver, OrgDoc};

/// `get_node_section` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetNodeSectionParams {
    /// The node's :ID:.
    pub id: String,

    /// Anchor: `<<target>>` name, `CUSTOM_ID`, headline title, or free-text search.
    pub anchor: String,
}

/// `get_node_section` — resolve a sub-section of a node.
///
/// The anchor is resolved *within the node's body* (the whole file for a
/// file-level node, the headline subtree for a headline node), so an anchor
/// in a sibling node of the same file never matches. This is the same
/// resolution the `org-roam://node/{id}#anchor` resource performs.
///
/// Resolution order: `CUSTOM_ID`, headline title, dedicated target
/// `<<name>>`, then case-insensitive free-text search.
///
/// Returned `begin`/`end` are byte offsets into the *file*, not the body.
///
/// # Errors
///
/// Returns an error if the node is not found, its file cannot be read, or
/// the anchor cannot be resolved.
pub fn get_node_section(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeSectionParams>,
) -> Result<CallToolResult, McpError> {
    let id = &p.0.id;
    let body = read_node_body(index, id).map_err(McpError::from)?;
    let warning = body.stale_warning();
    let offset = body.offset;

    let doc = OrgDoc::from_text(body.body);
    let section = AnchorResolver::resolve(&doc, &p.0.anchor).ok_or_else(|| {
        McpError::invalid_params(format!("anchor not found: {}", p.0.anchor), None)
    })?;

    let mut payload = serde_json::json!({
        "id": id,
        "anchor": p.0.anchor,
        "kind": section.kind,
        "begin": section.begin + offset,
        "end": section.end + offset,
        "text": section.text,
    });
    if let Some(w) = warning {
        payload["warning"] = w.into();
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )]))
}

/// How a node's [`NodeBody`] was derived from its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// File-level node: the body is the whole file (as requested).
    WholeFile,
    /// Headline node: the body is that headline's subtree (as requested).
    Subtree,
    /// Headline node whose id the index still knows but whose headline the
    /// file no longer contains (a stale index entry). The body fell back to
    /// the whole file, so it is *wider* than the caller asked for.
    StaleHeadlineFallback,
}

/// A node's body text plus where it starts in its file.
#[derive(Debug, Clone)]
pub struct NodeBody {
    pub node: crate::index::NodeMeta,
    /// Whole file for file-level nodes; the headline subtree otherwise.
    pub body: String,
    /// Byte offset of `body` within the file (0 for file-level nodes).
    pub offset: usize,
    /// How `body` was derived. Lets callers detect the stale-index fallback
    /// and report it instead of silently returning the wrong scope.
    pub kind: BodyKind,
}

impl NodeBody {
    /// A human-readable warning when the body is wider than requested
    /// because of a stale index entry; `None` when the body is exactly the
    /// scope the caller asked for.
    #[must_use]
    pub fn stale_warning(&self) -> Option<&'static str> {
        match self.kind {
            BodyKind::StaleHeadlineFallback => Some(
                "the index lists this id as a headline node, but its file no longer contains \
                 that headline; the body is the whole file. Run sync_database to refresh the \
                 index.",
            ),
            BodyKind::WholeFile | BodyKind::Subtree => None,
        }
    }
}

/// Why a node body could not be produced. Distinguishes "no such node"
/// (a caller mistake) from index/IO failures (server-side problems), so
/// callers can report each honestly.
#[derive(Debug)]
pub enum NodeBodyError {
    NotFound(String),
    Index(IndexError),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for NodeBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "node not found: {id}"),
            Self::Index(e) => write!(f, "index error: {e}"),
            Self::Io(path, e) => write!(f, "reading {}: {e}", path.display()),
        }
    }
}

impl From<NodeBodyError> for McpError {
    fn from(e: NodeBodyError) -> Self {
        match e {
            NodeBodyError::NotFound(_) => McpError::invalid_params(e.to_string(), None),
            NodeBodyError::Index(_) | NodeBodyError::Io(..) => {
                McpError::internal_error(e.to_string(), None)
            }
        }
    }
}

/// Read a node's body: the whole file for a file-level node, the headline
/// subtree for a headline node.
///
/// # Errors
///
/// Returns [`NodeBodyError::NotFound`] for an unknown id,
/// [`NodeBodyError::Index`] if the index query fails, and
/// [`NodeBodyError::Io`] if the node's file cannot be read.
pub fn read_node_body(index: &Arc<dyn RoamIndex>, id: &str) -> Result<NodeBody, NodeBodyError> {
    let node = index
        .node(id)
        .map_err(NodeBodyError::Index)?
        .ok_or_else(|| NodeBodyError::NotFound(id.to_string()))?;
    let doc = OrgDoc::from_file(&node.file).map_err(|e| NodeBodyError::Io(node.file.clone(), e))?;
    let (body, offset, kind) = if node.is_file() {
        (doc.text.to_string(), 0, BodyKind::WholeFile)
    } else if let Some(h) = doc.headline_by_id(id) {
        let (begin, end) = doc.subtree_range(&h);
        (doc.slice(begin, end).to_string(), begin, BodyKind::Subtree)
    } else {
        // The index knows the id but the file no longer contains it
        // (stale index entry); fall back to the whole file and flag it so
        // callers can report the wider-than-requested scope.
        (doc.text.to_string(), 0, BodyKind::StaleHeadlineFallback)
    };
    Ok(NodeBody {
        node,
        body,
        offset,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexResult, LinkRecord, NodeMeta, NodeQuery, RoamIndex};

    /// Index that returns one crafted node by id, reading from a real file.
    struct OneNode(NodeMeta);

    impl RoamIndex for OneNode {
        fn node(&self, id: &str) -> IndexResult<Option<NodeMeta>> {
            Ok((id == self.0.id).then(|| self.0.clone()))
        }
        fn find_nodes(&self, _q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>> {
            Ok(vec![self.0.clone()])
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
        fn source(&self) -> &'static str {
            "one"
        }
    }

    fn meta(id: &str, file: PathBuf, level: Option<usize>) -> NodeMeta {
        NodeMeta {
            id: id.to_string(),
            file,
            title: "T".to_string(),
            level,
            todo: None,
            priority: None,
            olp: Vec::new(),
            pos: level.map(|_| 0),
            aliases: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn index_for(node: NodeMeta) -> Arc<dyn RoamIndex> {
        Arc::new(OneNode(node))
    }

    #[test]
    fn stale_headline_falls_back_to_whole_file_and_warns() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("n.org");
        // The file has only a file-level node; the headline id below is gone.
        std::fs::write(
            &path,
            ":PROPERTIES:\n:ID: ffffffff-ffff-ffff-ffff-ffffffffffff\n:END:\n\
             #+title: File\n\nBody text here.\n",
        )
        .unwrap();
        let gone = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let idx = index_for(meta(gone, path, Some(1)));

        let body = read_node_body(&idx, gone).expect("read");
        assert_eq!(body.kind, BodyKind::StaleHeadlineFallback);
        assert!(
            body.body.contains("Body text here."),
            "fallback should return the whole file"
        );
        assert!(
            body.stale_warning().is_some(),
            "stale fallback must produce a warning"
        );
    }

    #[test]
    fn file_node_is_whole_file_without_warning() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("f.org");
        let id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        std::fs::write(
            &path,
            format!(":PROPERTIES:\n:ID: {id}\n:END:\n#+title: File\n\nWhole body.\n"),
        )
        .unwrap();
        let idx = index_for(meta(id, path, None));

        let body = read_node_body(&idx, id).expect("read");
        assert_eq!(body.kind, BodyKind::WholeFile);
        assert!(body.stale_warning().is_none(), "a file node is not stale");
    }
}
