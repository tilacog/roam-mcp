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

    let doc = OrgDoc::from_text(body.body);
    let section = AnchorResolver::resolve(&doc, &p.0.anchor).ok_or_else(|| {
        McpError::invalid_params(format!("anchor not found: {}", p.0.anchor), None)
    })?;

    let payload = serde_json::json!({
        "id": id,
        "anchor": p.0.anchor,
        "kind": section.kind,
        "begin": section.begin + body.offset,
        "end": section.end + body.offset,
        "text": section.text,
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )]))
}

/// A node's body text plus where it starts in its file.
#[derive(Debug, Clone)]
pub struct NodeBody {
    pub node: crate::index::NodeMeta,
    /// Whole file for file-level nodes; the headline subtree otherwise.
    pub body: String,
    /// Byte offset of `body` within the file (0 for file-level nodes).
    pub offset: usize,
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
    let (body, offset) = if node.is_file() {
        (doc.text.to_string(), 0)
    } else if let Some(h) = doc.headline_by_id(id) {
        let (begin, end) = doc.subtree_range(&h);
        (doc.slice(begin, end).to_string(), begin)
    } else {
        // The index knows the id but the file no longer contains it
        // (stale index entry); fall back to the whole file.
        (doc.text.to_string(), 0)
    };
    Ok(NodeBody { node, body, offset })
}
