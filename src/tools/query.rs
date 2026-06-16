//! Read-only query tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::index::scan::{keyword_values, parse_string_list};
use crate::index::{NodeMeta, NodeQuery, RoamIndex};
use crate::org::OrgDoc;
use crate::tools::content::read_node_body;

/// `search_nodes` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct SearchParams {
    /// Free-text query: matches against title and aliases (case-insensitive).
    #[serde(default)]
    pub query: Option<String>,

    /// Optional list of tags; the result requires all of them.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Maximum number of results to return. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `get_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetNodeParams {
    /// The node's :ID:.
    pub id: String,
}

/// `find_by_ref` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindByRefParams {
    /// A `ROAM_REFS` value: either a URL or a `@citekey`.
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// `unlinked_references` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnlinkedParams {
    /// Node ID to find unlinked occurrences of.
    pub id: String,

    /// Optional cap on returned occurrences.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `list_nodes` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ListNodesParams {
    /// Optional free-text filter on title / alias (case-insensitive).
    #[serde(default)]
    pub filter: Option<String>,

    /// Optional tags; results must bear all of them.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Page size. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,

    /// Number of nodes to skip before the page. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,

    /// Sort order: `title` (default) or `title_desc`.
    #[serde(default)]
    pub sort: Option<String>,
}

/// `list_orphans` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ListOrphansParams {
    /// Page size. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,

    /// Number of nodes to skip before the page. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,

    /// Sort order: `title` (default) or `title_desc`.
    #[serde(default)]
    pub sort: Option<String>,
}

/// `search_text` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchTextParams {
    /// Substring to search for in node bodies (case-insensitive).
    pub query: String,

    /// Maximum number of matches to return. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `get_node_by_path` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetNodeByPathParams {
    /// Path to a `.org` file, absolute or relative to the roam directory.
    pub path: String,
}

/// `tag_cooccurrences` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TagCooccurrenceParams {
    /// The tag whose co-occurring tags to count.
    pub tag: String,

    /// Maximum number of co-occurring tags to return. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `search_nodes` — find org-roam nodes by title / alias / tag.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn search_nodes(
    index: &Arc<dyn RoamIndex>,
    p: Parameters<SearchParams>,
) -> Result<CallToolResult, McpError> {
    let p = p.0;
    let limit = p.limit.unwrap_or(50);
    let q = NodeQuery {
        query: p.query.as_deref(),
        tags: &p.tags,
        limit: Some(limit),
    };
    let nodes = index.find_nodes(&q).map_err(internal)?;
    Ok(render_node_list(&nodes))
}

/// `get_node` — return node metadata plus its `body`: the whole file for
/// a file-level node, the headline subtree for a headline node.
///
/// # Errors
///
/// Returns an error if the index query fails, the node is not found, or
/// its file cannot be read.
pub fn get_node(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeParams>,
) -> Result<CallToolResult, McpError> {
    let body = read_node_body(index, &p.0.id).map_err(McpError::from)?;
    let warning = body.stale_warning();
    let mut out = serde_json::to_value(&body.node).map_err(internal)?;
    out["body"] = body.body.into();
    if let Some(w) = warning {
        out["warning"] = w.into();
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&out).unwrap_or_default(),
    )]))
}

/// `get_backlinks` — nodes whose `id:` links resolve to `id`, with the
/// linking node's metadata attached.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn get_backlinks(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeParams>,
) -> Result<CallToolResult, McpError> {
    let links = index.backlinks(&p.0.id).map_err(internal)?;
    let mut out = Vec::new();
    for l in links {
        if let Some(meta) = index.node(&l.source).map_err(internal)? {
            out.push(serde_json::json!({
                "node": meta,
                "link": l,
            }));
        }
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&out).unwrap_or_default(),
    )]))
}

/// `get_forward_links` — all outgoing links from `id` (every kind),
/// with destination node metadata attached where the link resolves.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn get_forward_links(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeParams>,
) -> Result<CallToolResult, McpError> {
    let links = index.forward_links(&p.0.id).map_err(internal)?;
    let mut out = Vec::new();
    for l in links {
        let mut entry = serde_json::json!({ "link": l });
        if let Some(dest) = &l.dest {
            if let Ok(Some(meta)) = index.node(dest) {
                entry["node"] = serde_json::to_value(meta).unwrap_or_default();
            }
        }
        out.push(entry);
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&out).unwrap_or_default(),
    )]))
}

/// `find_by_ref` — find nodes with a matching `ROAM_REFS` value.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn find_by_ref(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<FindByRefParams>,
) -> Result<CallToolResult, McpError> {
    let nodes = index.by_ref(&p.0.ref_).map_err(internal)?;
    Ok(render_node_list(&nodes))
}

/// `list_tags` — list all tags and the number of nodes bearing each.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn list_tags(index: &Arc<dyn RoamIndex>) -> Result<CallToolResult, McpError> {
    let tags = index.tags().map_err(internal)?;
    let text = serde_json::to_string_pretty(&tags).unwrap_or_default();
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// `unlinked_references` — places where `id`'s title or aliases appear in
/// plain text. Occurrences inside an org link (`[[...]]`) are skipped, so
/// already-linked mentions don't show up. Scans every `.org` file under
/// `roam_dir`.
///
/// # Errors
///
/// Returns an error if the index query fails or the node is not found.
pub fn unlinked_references(
    index: &Arc<dyn RoamIndex>,
    roam_dir: &std::path::Path,
    p: &Parameters<UnlinkedParams>,
) -> Result<CallToolResult, McpError> {
    let limit = p.0.limit.unwrap_or(50);
    let node = index.node(&p.0.id).map_err(internal)?;
    let Some(node) = node else {
        return Err(McpError::invalid_params("node not found", None));
    };

    let needles: Vec<String> = std::iter::once(node.title.clone())
        .chain(node.aliases.iter().cloned())
        .filter(|s| s.len() >= 3)
        .collect();
    if needles.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text("[]")]));
    }

    let mut out = Vec::new();
    'outer: for entry in walkdir::WalkDir::new(roam_dir).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|e| e.to_str()) != Some("org") {
            continue;
        }
        if entry.path() == node.file {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let lower = text.to_lowercase();
        for needle in &needles {
            let ndl = needle.to_lowercase();
            for (pos, _) in lower.match_indices(&ndl) {
                if inside_link(&lower, pos) {
                    continue;
                }
                // `lower` offsets normally coincide with `text` offsets;
                // when lowercasing changed byte lengths (rare non-ASCII
                // cases) the offset may not be a char boundary in the
                // original — skip rather than mis-slice.
                let Some(snippet) = snippet_around(&text, pos, ndl.len()) else {
                    continue;
                };
                out.push(serde_json::json!({
                    "file": entry.path(),
                    "offset": pos,
                    "snippet": snippet,
                    "matched": needle,
                }));
                if out.len() >= limit {
                    break 'outer;
                }
            }
        }
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&out).unwrap_or_default(),
    )]))
}

/// `list_nodes` — paginated enumeration of the vault. Unlike
/// `search_nodes` (which truncates at a limit with no way to page past it),
/// this returns the total count and honors `offset`, so a client can walk
/// the whole index.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn list_nodes(
    index: &Arc<dyn RoamIndex>,
    p: Parameters<ListNodesParams>,
) -> Result<CallToolResult, McpError> {
    let p = p.0;
    let q = NodeQuery {
        query: p.filter.as_deref(),
        tags: &p.tags,
        limit: None,
    };
    let mut nodes = index.find_nodes(&q).map_err(internal)?;
    if p.sort.as_deref() == Some("title_desc") {
        nodes.sort_by(|a, b| b.title.cmp(&a.title));
    } else {
        nodes.sort_by(|a, b| a.title.cmp(&b.title));
    }
    let total = nodes.len();
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(50);
    let page: Vec<NodeMeta> = nodes.into_iter().skip(offset).take(limit).collect();
    let payload = serde_json::json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "count": page.len(),
        "nodes": page,
    });
    Ok(json_result(&payload))
}

/// `list_orphans` — notes with no edges in the `id:` link graph: no
/// outgoing `id:` forward links and no incoming `id:` links. These
/// notes exist in the vault but are unreachable from any other note —
/// prime candidates for triage (merge, link, or delete). The response
/// shape matches `list_nodes` so clients can drive both with the same
/// paging logic.
///
/// URL, file, citation, and fuzzy links do not point at other notes,
/// so they are not counted as edges.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn list_orphans(
    index: &Arc<dyn RoamIndex>,
    p: Parameters<ListOrphansParams>,
) -> Result<CallToolResult, McpError> {
    let p = p.0;
    let mut nodes = index.orphans().map_err(internal)?;
    if p.sort.as_deref() == Some("title_desc") {
        nodes.sort_by(|a, b| b.title.cmp(&a.title));
    } // the index already returns title-asc, so no-op for the default
    let total = nodes.len();
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(50);
    let page: Vec<NodeMeta> = nodes.into_iter().skip(offset).take(limit).collect();
    let payload = serde_json::json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "count": page.len(),
        "nodes": page,
    });
    Ok(json_result(&payload))
}

/// `search_text` — full-text search across node bodies. `search_nodes`
/// only matches titles, aliases, and tags; this walks every `.org` file
/// and reports the matching files, line numbers, and snippets, attributing
/// each to its file-level node where one exists.
///
/// # Errors
///
/// Returns an error if the query is empty or the index query fails.
pub fn search_text(
    index: &Arc<dyn RoamIndex>,
    roam_dir: &Path,
    p: Parameters<SearchTextParams>,
) -> Result<CallToolResult, McpError> {
    let p = p.0;
    let needle = p.query.to_lowercase();
    if needle.trim().is_empty() {
        return Err(McpError::invalid_params("query must not be empty", None));
    }
    let limit = p.limit.unwrap_or(50);

    // file -> (id, title) for file-level nodes, so matches can name a node.
    let all = index
        .find_nodes(&NodeQuery {
            query: None,
            tags: &[],
            limit: None,
        })
        .map_err(internal)?;
    let mut file_node: HashMap<PathBuf, (String, String)> = HashMap::new();
    for n in &all {
        if n.is_file() {
            file_node
                .entry(n.file.clone())
                .or_insert_with(|| (n.id.clone(), n.title.clone()));
        }
    }

    let mut out = Vec::new();
    'outer: for entry in walkdir::WalkDir::new(roam_dir).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|e| e.to_str()) != Some("org")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let lower = text.to_lowercase();
        for (pos, _) in lower.match_indices(&needle) {
            // `lower` byte offsets only coincide with `text` offsets when
            // lowercasing preserved byte lengths; skip the rare mismatch.
            let Some(snippet) = snippet_around(&text, pos, needle.len()) else {
                continue;
            };
            let line = text[..pos].bytes().filter(|&b| b == b'\n').count() + 1;
            let mut hit = serde_json::json!({
                "file": entry.path(),
                "line": line,
                "snippet": snippet,
            });
            if let Some((id, title)) = file_node.get(entry.path()) {
                hit["node_id"] = id.clone().into();
                hit["title"] = title.clone().into();
            }
            out.push(hit);
            if out.len() >= limit {
                break 'outer;
            }
        }
    }
    Ok(json_result(&serde_json::Value::Array(out)))
}

/// `get_node_by_path` — look up a node by its file path rather than its
/// `:ID:`, then return the same payload as `get_node`. Resolves relative
/// paths against the roam directory and refuses paths outside it.
///
/// # Errors
///
/// Returns an error if the path cannot be resolved, is outside the roam
/// directory, has no file-level `:ID:`, or its file cannot be read.
pub fn get_node_by_path(
    index: &Arc<dyn RoamIndex>,
    roam_dir: &Path,
    p: &Parameters<GetNodeByPathParams>,
) -> Result<CallToolResult, McpError> {
    let raw = PathBuf::from(&p.0.path);
    let joined = if raw.is_absolute() {
        raw
    } else {
        roam_dir.join(raw)
    };
    let path = joined
        .canonicalize()
        .map_err(|e| McpError::invalid_params(format!("cannot resolve path: {e}"), None))?;
    if !path.starts_with(roam_dir) {
        return Err(McpError::invalid_params(
            "path is outside the roam directory",
            None,
        ));
    }
    let doc = OrgDoc::from_file(&path).map_err(internal)?;
    let id = doc
        .document()
        .properties()
        .and_then(|props| props.get("ID"))
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            McpError::invalid_params(format!("no file-level :ID: in {}", path.display()), None)
        })?;

    let body = read_node_body(index, &id).map_err(McpError::from)?;
    let mut payload = serde_json::to_value(&body.node).map_err(internal)?;
    payload["body"] = body.body.into();
    Ok(json_result(&payload))
}

/// `get_refs` — the `ROAM_REFS` (and org-roam v1 `#+ROAM_KEY:`) values
/// declared by a node. The inverse of `find_by_ref`.
///
/// # Errors
///
/// Returns an error if the node is not found or its file cannot be read.
pub fn get_refs(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeParams>,
) -> Result<CallToolResult, McpError> {
    let node = index
        .node(&p.0.id)
        .map_err(internal)?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;
    let text = std::fs::read_to_string(&node.file).map_err(internal)?;
    let doc = OrgDoc::from_text(text.clone());

    let mut refs: Vec<String> = Vec::new();
    let props = if node.is_file() {
        doc.document().properties()
    } else {
        doc.headline_by_id(&p.0.id).and_then(|h| h.properties())
    };
    if let Some(props) = props {
        if let Some(v) = props.get("ROAM_REFS") {
            refs.extend(parse_string_list(v.as_ref()));
        }
    }
    if node.is_file() {
        for v in keyword_values(&text, "roam_key") {
            refs.extend(parse_string_list(v));
        }
    }
    let mut seen = std::collections::HashSet::new();
    refs.retain(|r| seen.insert(r.clone()));

    let payload = serde_json::json!({ "id": p.0.id, "refs": refs });
    Ok(json_result(&payload))
}

/// `list_anchors` — the addressable sub-targets of a node: dedicated
/// targets `<<name>>`, headline titles, and `CUSTOM_ID`s. These are exactly
/// the anchors `get_node_section` / the `#anchor` resource can resolve.
///
/// # Errors
///
/// Returns an error if the node is not found or its file cannot be read.
pub fn list_anchors(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<GetNodeParams>,
) -> Result<CallToolResult, McpError> {
    let body = read_node_body(index, &p.0.id).map_err(McpError::from)?;
    let doc = OrgDoc::from_text(body.body.clone());

    let headlines: Vec<String> = doc
        .headlines()
        .iter()
        .map(|h| h.title_raw().trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let custom_ids: Vec<String> = doc
        .headlines()
        .iter()
        .filter_map(|h| {
            h.properties()
                .and_then(|props| props.get("CUSTOM_ID"))
                .map(|t| t.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect();

    let mut payload = serde_json::json!({
        "id": p.0.id,
        "targets": dedicated_targets(&body.body),
        "headlines": headlines,
        "custom_ids": custom_ids,
        "names": name_keywords(&body.body),
    });
    if let Some(w) = body.stale_warning() {
        payload["warning"] = w.into();
    }
    Ok(json_result(&payload))
}

/// The `#+NAME:` values declared in `text`, in document order,
/// de-duplicated. Mirrors the shape of [`dedicated_targets`].
fn name_keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("#+") else {
            continue;
        };
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("NAME") {
            continue;
        }
        let v = value.trim().to_string();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// `tag_cooccurrences` — for nodes bearing `tag`, count which other tags
/// appear alongside it, most frequent first.
///
/// # Errors
///
/// Returns an error if the index query fails.
pub fn tag_cooccurrences(
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<TagCooccurrenceParams>,
) -> Result<CallToolResult, McpError> {
    let tag = p.0.tag.clone();
    let filter = [tag.clone()];
    let nodes = index
        .find_nodes(&NodeQuery {
            query: None,
            tags: &filter,
            limit: None,
        })
        .map_err(internal)?;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for n in &nodes {
        for t in &n.tags {
            if !t.eq_ignore_ascii_case(&tag) {
                *counts.entry(t.clone()).or_default() += 1;
            }
        }
    }
    let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs.truncate(p.0.limit.unwrap_or(50));

    let cooccurring: Vec<serde_json::Value> = pairs
        .into_iter()
        .map(|(t, c)| serde_json::json!({ "tag": t, "count": c }))
        .collect();
    let payload = serde_json::json!({
        "tag": tag,
        "node_count": nodes.len(),
        "cooccurring": cooccurring,
    });
    Ok(json_result(&payload))
}

/// Dedicated targets `<<name>>` in `text` (excluding radio targets
/// `<<<name>>>`), in document order, de-duplicated.
fn dedicated_targets(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find("<<") {
        let open = i + rel;
        // Skip radio targets (`<<<`).
        if open >= 1 && bytes[open - 1] == b'<' {
            i = open + 2;
            continue;
        }
        let after = open + 2;
        if let Some(crel) = text[after..].find(">>") {
            let close = after + crel;
            let name = text[after..close].trim();
            if !name.is_empty() && !name.contains('<') && !name.contains('\n') {
                let owned = name.to_string();
                if !out.contains(&owned) {
                    out.push(owned);
                }
            }
            i = close + 2;
        } else {
            break;
        }
    }
    out
}

/// True when the byte at `pos` falls inside an org link (`[[...]]`):
/// there is an unclosed `[[` before it.
fn inside_link(text: &str, pos: usize) -> bool {
    match text[..pos].rfind("[[") {
        None => false,
        Some(open) => !text[open..pos].contains("]]"),
    }
}

/// Up to 40 bytes of context on each side of the match, clamped to char
/// boundaries. Returns `None` if `pos` itself isn't a valid boundary.
fn snippet_around(text: &str, pos: usize, match_len: usize) -> Option<String> {
    if !text.is_char_boundary(pos) {
        return None;
    }
    let mut start = pos.saturating_sub(40);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (pos + match_len + 40).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    Some(text[start..end].to_string())
}

fn render_node_list(nodes: &[NodeMeta]) -> CallToolResult {
    let text = serde_json::to_string_pretty(nodes).unwrap_or_default();
    CallToolResult::success(vec![Content::text(text)])
}

/// Render any serializable value as a pretty-printed JSON tool result.
fn json_result(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
}

fn internal<E: std::fmt::Display>(e: E) -> McpError {
    McpError::internal_error(e.to_string(), None)
}
