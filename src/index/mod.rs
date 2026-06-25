//! Index layer for org-roam.
//!
//! Two implementations of the same [`RoamIndex`] trait live behind it:
//! - [`sqlite::SqliteIndex`]: reads `org-roam.db` directly (canonical, fast).
//! - [`scan::ScanIndex`]: walks the directory with `orgize` (fallback).
//!
//! The trait surface is intentionally narrow — the read-side of the MCP
//! tools needs node metadata, backlinks, forward links, reflinks, and tag
//! counts. Anything richer (subtree extraction, anchor resolution) lives
//! in `org::parse` and `org::anchors`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod scan;
pub mod sqlite;

/// Anything that can go wrong inside an index implementation.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("malformed data: {0}")]
    Malformed(String),

    #[error("backend error: {0}")]
    Other(String),
}

pub type IndexResult<T> = Result<T, IndexError>;

/// Metadata for a single org-roam node (file or headline with `:ID:`).
///
/// The headline-level fields (`level`, `todo`, `olp`, `priority`) are only
/// populated for headline nodes; for file-level nodes they are `None` / empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeMeta {
    /// The org-roam node ID, a UUID string.
    pub id: String,

    /// Absolute path of the file containing the node.
    pub file: PathBuf,

    /// Headline title, or file-level title for file nodes.
    pub title: String,

    /// 1-based headline level (`*` = 1). `None` for file-level nodes.
    pub level: Option<usize>,

    /// TODO keyword (`TODO`, `DONE`, ...) if any.
    pub todo: Option<String>,

    /// Priority cookie (e.g. `[#A]`) if any.
    pub priority: Option<String>,

    /// Outline path from the file root to this headline (empty for file nodes).
    pub olp: Vec<String>,

    /// Byte offset of the headline start within the file. `None` for file nodes.
    pub pos: Option<usize>,

    /// Aliases from `ROAM_ALIASES`.
    pub aliases: Vec<String>,

    /// Tags from `:tag1:tag2:` syntax.
    pub tags: Vec<String>,
}

impl NodeMeta {
    /// Whether this is a file-level node (no headline).
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.level.is_none()
    }
}

/// A link from one node to another, or a reflink (citation / URL).
///
/// For `id:` links, `dest` is the destination node ID; for every other
/// kind `dest` is `None`. `raw_dest` is the link target text as written
/// (or as stored in `org-roam.db`). For URL and citation links,
/// `ref_target` holds the full URL or `@citekey`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkRecord {
    /// Node ID of the linking node (the source).
    pub source: String,

    /// Resolved destination node ID, if known.
    pub dest: Option<String>,

    /// Original text of the link target (e.g. `"Some Title"`, `"notes.org::*Heading"`).
    pub raw_dest: String,

    /// Link type, matching org-roam's vocabulary. Both backends emit:
    /// `"id"`, `"roam"`, `"file"`, `"https"` / `"http"`, `"cite"`, or
    /// `"fuzzy"` (a bare-text link). Other org link protocols pass
    /// through as-is.
    pub kind: String,

    /// For URL/citation links, the full URL or `@citekey`.
    pub ref_target: Option<String>,
}

/// Search parameters for `RoamIndex::find_nodes`.
///
/// All fields are optional; the backend does whatever combination it can.
/// `query` matches against title and aliases case-insensitively.
/// `tags` requires the node to bear *all* listed tags (AND); tags never
/// match against titles or aliases.
/// `limit` caps the result count *after* all filters are applied.
#[derive(Debug, Clone, Default)]
pub struct NodeQuery<'a> {
    pub query: Option<&'a str>,
    pub tags: &'a [String],
    pub limit: Option<usize>,
}

/// The index interface shared by `SQLite` and scanner backends.
pub trait RoamIndex: Send + Sync {
    /// Search nodes by title / alias / tag. Returns metadata only (no body).
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn find_nodes(&self, q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>>;

    /// Look up a single node by ID. Returns `None` if the ID is unknown.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn node(&self, id: &str) -> IndexResult<Option<NodeMeta>>;

    /// Backlinks: links whose resolved destination is `id`. In practice
    /// only `id:` links resolve to a node ID, so only those appear here.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn backlinks(&self, id: &str) -> IndexResult<Vec<LinkRecord>>;

    /// Forward links from `id` to other nodes.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn forward_links(&self, id: &str) -> IndexResult<Vec<LinkRecord>>;

    /// Nodes that have a matching `ROAM_REFS` (URL or `@citekey`).
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn by_ref(&self, r: &str) -> IndexResult<Vec<NodeMeta>>;

    /// All tags and the number of nodes bearing each.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn tags(&self) -> IndexResult<Vec<(String, usize)>>;

    /// Total number of nodes (for diagnostics / tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn node_count(&self) -> IndexResult<usize>;

    /// Nodes with no edges in the `id:` link graph: no outgoing `id:`
    /// forward links and no incoming `id:` links (backlinks). Such
    /// notes exist in the index but are unreachable from any other
    /// note — they are the candidates for triage (merge, link, or
    /// delete). Returned sorted by title.
    ///
    /// URL, file, citation, and fuzzy links do not point at other
    /// notes, so they are not counted as edges.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn orphans(&self) -> IndexResult<Vec<NodeMeta>>;

    /// Where this index reads its data from (DB path or roam dir).
    fn source(&self) -> &str;

    /// Return a random node from the index.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails or the index is empty.
    fn random_node(&self) -> IndexResult<NodeMeta>;
}

/// Pick the appropriate backend for a `Config`. Prefers `SQLite` if a DB is
/// available and `no_db` is not set; otherwise builds a scanner.
///
/// # Errors
///
/// Returns an error if the chosen backend fails to open.
pub fn open(config: &crate::Config) -> IndexResult<std::sync::Arc<dyn RoamIndex>> {
    use std::sync::Arc;
    if config.has_db() {
        sqlite::SqliteIndex::open(&config.db_path()).map(|i| Arc::new(i) as Arc<dyn RoamIndex>)
    } else {
        scan::ScanIndex::open(&config.roam_dir).map(|i| Arc::new(i) as Arc<dyn RoamIndex>)
    }
}
