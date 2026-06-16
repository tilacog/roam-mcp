//! Write tools: create nodes, append content, insert anchors, daily capture.
//!
//! All writes are file-level and append-only. The org-roam database
//! is never touched — after a successful write the server triggers
//! `org-roam-db-sync` (see `sync::DbSyncer`) and, in scanner mode,
//! rebuilds its own index so the change is immediately visible.
//!
//! Every write goes through `util::atomic_write`, which refuses to touch
//! a file that Emacs holds a lockfile for and replaces atomically via a
//! sibling temp file.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Local, NaiveDate};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::RoamIndex;
use crate::org::{edit, OrgDoc};
use crate::util::{atomic_write, default_filename, remove_file_unlocked, rename_unlocked, slugify};

/// `create_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateNodeParams {
    /// Title for the new node.
    pub title: String,

    /// Optional tags for `#+filetags:`.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Optional body content (org text) placed after the title.
    #[serde(default)]
    pub body: Option<String>,

    /// Optional `ROAM_REFS` values.
    #[serde(default)]
    pub refs: Vec<String>,

    /// Optional `ROAM_ALIASES` values.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// `append_to_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppendParams {
    /// The node's :ID:.
    pub id: String,

    /// Content to append.
    pub content: String,

    /// If set, append under this headline (matched by title) within the
    /// node; otherwise append at the end of the node.
    #[serde(default)]
    pub headline: Option<String>,
}

/// `insert_anchor` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InsertAnchorParams {
    /// The node's :ID:.
    pub id: String,

    /// A short unique string in the body to anchor against.
    pub search_text: String,

    /// The anchor name to use in `<<anchor>>` and in the returned link.
    pub anchor_name: String,
}

/// Reject the call when the server is read-only.
///
/// The server also removes the write tools from its router in read-only
/// mode; this guard protects direct library callers.
fn ensure_writable(cfg: &Config) -> Result<(), McpError> {
    if cfg.can_write() {
        Ok(())
    } else {
        Err(McpError::invalid_request(
            "server is in --read-only mode",
            None,
        ))
    }
}

/// Read `path`, let `edit` transform the text, then write the result back
/// atomically. Refuses when the file changed on disk between the read and
/// the write (e.g. an Emacs save racing this call) — retry on a fresh
/// read rather than clobbering the other writer. A race in the remaining
/// stat-to-rename window is still possible; this narrows it from the
/// whole read-edit span to microseconds.
fn rewrite_file(
    path: &std::path::Path,
    edit: impl FnOnce(&mut String) -> Result<(), McpError>,
) -> Result<(), McpError> {
    let io_err =
        |e: std::io::Error| McpError::internal_error(format!("{}: {e}", path.display()), None);
    let mtime_before = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(io_err)?;
    let mut text = std::fs::read_to_string(path).map_err(io_err)?;
    edit(&mut text)?;
    let mtime_after = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(io_err)?;
    if mtime_after != mtime_before {
        return Err(McpError::internal_error(
            format!("{} changed on disk during the edit; retry", path.display()),
            None,
        ));
    }
    atomic_write(path, &text).map_err(io_err)
}

/// Reject anchor names that would break the `<<name>>` syntax or the
/// `[[id:UUID::name]]` link handed back to the caller.
fn validate_anchor_name(name: &str) -> Result<(), McpError> {
    if name.trim().is_empty() {
        return Err(McpError::invalid_params(
            "anchor_name must not be empty",
            None,
        ));
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | '[' | ']') || c.is_control())
    {
        return Err(McpError::invalid_params(
            "anchor_name must not contain '<', '>', '[', ']', or control characters",
            None,
        ));
    }
    Ok(())
}

/// Insert `content` into `text`: after the subtree of the headline titled
/// `headline` when one matches, otherwise at the end of the text.
fn insert_under_headline(text: &mut String, headline: Option<&str>, content: &str) {
    let insertion = format!("\n{}\n", content.trim_end());
    let pos = headline
        .and_then(|h| {
            let doc = OrgDoc::from_text(text.clone());
            doc.headlines()
                .into_iter()
                .find(|hl| hl.title_raw().trim() == h.trim())
                .map(|hl| doc.subtree_range(&hl).1)
        })
        .unwrap_or(text.len());
    text.insert_str(pos, &insertion);
}

/// `create_node` — create a fresh `.org` file with an `:ID:` property.
///
/// # Errors
///
/// Returns an error if write operations are disabled or the file cannot be written.
pub fn create_node(
    cfg: &Config,
    p: Parameters<CreateNodeParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let id = uuid::Uuid::new_v4().to_string();
    let now = Local::now().naive_local();
    let filename = default_filename(now, &p.title);
    let path: PathBuf = cfg.roam_dir.join(&filename);

    // Refuse to clobber an existing file with the same name.
    if path.exists() {
        return Err(McpError::invalid_params(
            format!("file already exists: {}", path.display()),
            None,
        ));
    }

    let mut body = String::new();
    body.push_str(":PROPERTIES:\n");
    let _ = writeln!(body, ":ID:       {id}");
    if !p.aliases.is_empty() {
        let quoted: Vec<String> = p
            .aliases
            .iter()
            .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
            .collect();
        let _ = writeln!(body, ":ROAM_ALIASES: {}", quoted.join(" "));
    }
    if !p.refs.is_empty() {
        let _ = writeln!(body, ":ROAM_REFS: {}", p.refs.join(" "));
    }
    body.push_str(":END:\n");
    let _ = writeln!(body, "#+title: {}", p.title);
    if !p.tags.is_empty() {
        let filetags = p.tags.iter().fold(String::new(), |mut s, t| {
            let _ = write!(s, ":{t}:");
            s
        });
        let _ = writeln!(body, "#+filetags: {filetags}");
    }
    if let Some(b) = p.body {
        if !b.is_empty() {
            body.push('\n');
            body.push_str(&b);
            if !b.ends_with('\n') {
                body.push('\n');
            }
        }
    }

    atomic_write(&path, &body).map_err(|e| {
        McpError::internal_error(format!("writing {}: {}", path.display(), e), None)
    })?;

    let payload = serde_json::json!({
        "id": id,
        "file": path,
        "slug": slugify(&p.title),
    });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )]))
}

/// `append_to_node` — append text to a node, optionally under a specific
/// headline. Refuses if the file is locked by Emacs.
///
/// # Errors
///
/// Returns an error if the node is not found or the file cannot be written.
pub fn append_to_node(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<AppendParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let node = node.ok_or_else(|| McpError::invalid_params("node not found", None))?;

    rewrite_file(&node.file, |text| {
        insert_under_headline(text, p.headline.as_deref(), &p.content);
        Ok(())
    })?;

    Ok(CallToolResult::success(vec![Content::text("ok")]))
}

/// `insert_anchor` — locate `search_text` in the node body and place
/// `<<anchor_name>>` immediately before the matching paragraph. Returns
/// the resulting `[[id:UUID::anchor_name]]` link text.
///
/// # Errors
///
/// Returns an error if the node is not found, the search text is missing, or the file cannot be written.
pub fn insert_anchor(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<InsertAnchorParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    validate_anchor_name(&p.anchor_name)?;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let node = node.ok_or_else(|| McpError::invalid_params("node not found", None))?;

    let file = node.file.clone();
    rewrite_file(&node.file, |text| {
        let pos = text.find(&p.search_text).ok_or_else(|| {
            McpError::invalid_params(format!("search_text not found in {}", file.display()), None)
        })?;
        // Walk back to the start of the line containing `pos`.
        let line_start = text[..pos].rfind('\n').map_or(0, |n| n + 1);
        let marker = format!("<<{}>>\n", p.anchor_name);
        text.insert_str(line_start, &marker);
        Ok(())
    })?;

    let link = format!("[[id:{}::{}]]", p.id, p.anchor_name);
    Ok(CallToolResult::success(vec![Content::text(link)]))
}

/// `daily_capture` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DailyCaptureParams {
    /// Optional content to append to today's daily note.
    #[serde(default)]
    pub content: Option<String>,

    /// If set, append the content under this headline within the daily note.
    #[serde(default)]
    pub headline: Option<String>,
}

/// `daily_capture` — create or retrieve today's daily org-roam note,
/// optionally appending content. Returns the node ID and file path.
///
/// The note's location and name come from `Config::dailies_dir` and
/// `Config::dailies_format` (default: `<roam_dir>/YYYYMMDD.org`); the
/// note follows the standard org-roam property-drawer + title convention.
/// If the file already exists, its existing `:ID:` is reused. If it
/// doesn't exist, a fresh UUID is generated and the file is created
/// (along with any missing parent directories).
///
/// # Errors
///
/// Returns an error if write operations are disabled, the dailies
/// filename pattern is invalid, or the file cannot be written.
pub fn daily_capture(
    cfg: &Config,
    p: Parameters<DailyCaptureParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let today = Local::now().date_naive();
    let stem = format_date(&cfg.dailies_format, today)?;
    let dir = match &cfg.dailies_dir {
        Some(d) => cfg.roam_dir.join(d),
        None => cfg.roam_dir.clone(),
    };
    let path: PathBuf = dir.join(format!("{stem}.org"));
    // The stem may contain separators (e.g. `%Y/%m/%d`); create whatever
    // directories the final path needs.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            McpError::internal_error(format!("creating {}: {e}", parent.display()), None)
        })?;
    }
    let title = today.format("%Y-%m-%d").to_string();

    let id = if path.exists() {
        // Re-use the existing :ID:.
        let text = std::fs::read_to_string(&path).map_err(|e| {
            McpError::internal_error(format!("reading {}: {}", path.display(), e), None)
        })?;
        extract_file_id(&text).unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    } else {
        let new_id = uuid::Uuid::new_v4().to_string();
        let header = format!(":PROPERTIES:\n:ID:       {new_id}\n:END:\n#+title: {title}\n");
        atomic_write(&path, &header).map_err(|e| {
            McpError::internal_error(format!("writing {}: {}", path.display(), e), None)
        })?;
        new_id
    };

    if let Some(content) = p.content.filter(|s| !s.trim().is_empty()) {
        rewrite_file(&path, |text| {
            insert_under_headline(text, p.headline.as_deref(), &content);
            Ok(())
        })?;
    }

    let payload = serde_json::json!({ "id": id, "file": path, "date": title });
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )]))
}

// ── update / delete / prepend / rename / add_link ───────────────────────────

/// `update_node` parameters. Every metadata field is an `Option`: omitting
/// it leaves that part of the node untouched, while passing an explicit
/// (even empty) value sets it. This is the idempotent counterpart to
/// `create_node`, keyed on `:ID:` — the file's `:ID:` is never changed, so
/// the update can be replayed without breaking the backlink graph.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateNodeParams {
    /// The node's :ID:.
    pub id: String,

    /// New `#+title:`.
    #[serde(default)]
    pub title: Option<String>,

    /// Replacement body (everything after the header keywords). Omit to
    /// keep the existing body.
    #[serde(default)]
    pub body: Option<String>,

    /// New tag set for `#+filetags:`. An empty list removes the keyword.
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// New `ROAM_ALIASES`. An empty list removes the property.
    #[serde(default)]
    pub aliases: Option<Vec<String>>,

    /// New `ROAM_REFS`. An empty list removes the property.
    #[serde(default)]
    pub refs: Option<Vec<String>>,

    /// Arbitrary `:PROPERTIES:` drawer entries to set (`:ID:` is rejected).
    #[serde(default)]
    pub properties: Option<HashMap<String, String>>,

    /// If true, compute the new file text and return it without writing.
    #[serde(default)]
    pub preview: bool,
}

/// Apply the requested edits to a file-level node's text in place.
fn apply_node_edits(text: &mut String, p: &UpdateNodeParams) -> Result<(), McpError> {
    if let Some(title) = &p.title {
        edit::set_keyword(text, "title", Some(title));
    }
    if let Some(tags) = &p.tags {
        edit::set_keyword(text, "filetags", edit::render_filetags(tags).as_deref());
    }
    if let Some(aliases) = &p.aliases {
        edit::set_drawer_property(
            text,
            "ROAM_ALIASES",
            edit::render_alias_list(aliases).as_deref(),
        );
    }
    if let Some(refs) = &p.refs {
        edit::set_drawer_property(text, "ROAM_REFS", edit::render_ref_list(refs).as_deref());
    }
    if let Some(props) = &p.properties {
        for (k, v) in props {
            if k.eq_ignore_ascii_case("id") {
                return Err(McpError::invalid_params(
                    "cannot change the :ID: property via update_node",
                    None,
                ));
            }
            edit::set_drawer_property(text, k, Some(v));
        }
    }
    if let Some(body) = &p.body {
        edit::replace_file_body(text, body);
    }
    Ok(())
}

/// `update_node` — edit a file-level node's title, body, tags, aliases,
/// refs, or drawer properties in place, preserving its `:ID:`.
///
/// Headline nodes are not supported (their metadata lives on the headline,
/// not in the file preamble); use `append_to_node` / `get_node_section`
/// for those. With `preview: true` the new text is returned without writing.
///
/// # Errors
///
/// Returns an error if writes are disabled, the node is not found, the node
/// is a headline node, a reserved property is set, or the file cannot be
/// written.
pub fn update_node(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<UpdateNodeParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;
    if node.level.is_some() {
        return Err(McpError::invalid_params(
            format!(
                "update_node supports file-level nodes only; '{}' is a headline node",
                p.id
            ),
            None,
        ));
    }

    if p.preview {
        let original = std::fs::read_to_string(&node.file).map_err(|e| {
            McpError::internal_error(format!("reading {}: {e}", node.file.display()), None)
        })?;
        let mut updated = original.clone();
        apply_node_edits(&mut updated, &p)?;
        let payload = serde_json::json!({
            "id": p.id,
            "file": node.file,
            "changed": original != updated,
            "preview": updated,
        });
        return Ok(json_result(&payload));
    }

    rewrite_file(&node.file, |text| apply_node_edits(text, &p))?;
    let payload = serde_json::json!({ "id": p.id, "file": node.file, "updated": true });
    Ok(json_result(&payload))
}

/// `delete_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteNodeParams {
    /// The node's :ID:.
    pub id: String,
}

/// `delete_node` — remove a node. For a file-level node the whole file is
/// deleted; for a headline node only that headline's subtree is removed
/// from the file.
///
/// # Errors
///
/// Returns an error if writes are disabled, the node is not found, an Emacs
/// lockfile is present, or the file cannot be modified.
pub fn delete_node(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<DeleteNodeParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;

    let kind = if node.is_file() {
        remove_file_unlocked(&node.file).map_err(|e| {
            McpError::internal_error(format!("deleting {}: {e}", node.file.display()), None)
        })?;
        "file"
    } else {
        let id = p.id.clone();
        let file = node.file.clone();
        rewrite_file(&node.file, |text| {
            let doc = OrgDoc::from_text(text.clone());
            let headline = doc.headline_by_id(&id).ok_or_else(|| {
                McpError::invalid_params(
                    format!("headline no longer present in {}", file.display()),
                    None,
                )
            })?;
            let (begin, end) = doc.subtree_range(&headline);
            text.replace_range(begin..end, "");
            Ok(())
        })?;
        "headline"
    };

    let payload =
        serde_json::json!({ "id": p.id, "file": node.file, "deleted": true, "kind": kind });
    Ok(json_result(&payload))
}

/// `prepend_to_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrependParams {
    /// The node's :ID:.
    pub id: String,

    /// Content to insert at the start of the body.
    pub content: String,

    /// If set, prepend to this headline's body within the node instead of
    /// the node's own body.
    #[serde(default)]
    pub headline: Option<String>,
}

/// `prepend_to_node` — insert content at the *start* of a node's body
/// (the symmetric counterpart to `append_to_node`), after the property
/// drawer and header keywords so metadata is never disturbed.
///
/// # Errors
///
/// Returns an error if writes are disabled, the node (or named headline) is
/// not found, or the file cannot be written.
pub fn prepend_to_node(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<PrependParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;

    rewrite_file(&node.file, |text| {
        let offset = prepend_offset(text, &node, p.headline.as_deref())?;
        let insertion = format!("{}\n\n", p.content.trim_end());
        text.insert_str(offset, &insertion);
        Ok(())
    })?;
    Ok(CallToolResult::success(vec![Content::text("ok")]))
}

/// Byte offset at which `prepend_to_node` should insert content.
fn prepend_offset(
    text: &str,
    node: &crate::index::NodeMeta,
    headline: Option<&str>,
) -> Result<usize, McpError> {
    if let Some(title) = headline {
        let doc = OrgDoc::from_text(text.to_string());
        let h = doc
            .headlines()
            .into_iter()
            .find(|hl| hl.title_raw().trim() == title.trim())
            .ok_or_else(|| {
                McpError::invalid_params(format!("headline not found: {title}"), None)
            })?;
        Ok(edit::headline_body_offset(text, h.start().into()))
    } else if node.level.is_some() {
        let doc = OrgDoc::from_text(text.to_string());
        let h = doc
            .headline_by_id(&node.id)
            .ok_or_else(|| McpError::invalid_params("headline no longer present in file", None))?;
        Ok(edit::headline_body_offset(text, h.start().into()))
    } else {
        Ok(edit::body_start_offset(text))
    }
}

/// `rename_node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenameNodeParams {
    /// The node's :ID:.
    pub id: String,

    /// The new title.
    pub title: String,

    /// Whether to also rename the file to match the new title (default
    /// true). Any leading `YYYYMMDDHHMMSS-` timestamp is preserved.
    #[serde(default)]
    pub rename_file: Option<bool>,
}

/// `rename_node` — change a file-level node's `#+title:` and, by default,
/// rename its file to a slug of the new title (preserving any leading
/// org-roam timestamp prefix). Backlinks are keyed on `:ID:`, so they are
/// unaffected.
///
/// # Errors
///
/// Returns an error if writes are disabled, the node is not found or is a
/// headline node, the target filename already exists, or IO fails.
pub fn rename_node(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<RenameNodeParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let node = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;
    if node.level.is_some() {
        return Err(McpError::invalid_params(
            "rename_node supports file-level nodes only",
            None,
        ));
    }

    rewrite_file(&node.file, |text| {
        edit::set_keyword(text, "title", Some(&p.title));
        Ok(())
    })?;

    let mut final_path = node.file.clone();
    if p.rename_file.unwrap_or(true) {
        if let Some(parent) = node.file.parent() {
            let new_path = parent.join(renamed_filename(&node.file, &p.title));
            if new_path != node.file {
                rename_unlocked(&node.file, &new_path).map_err(|e| {
                    McpError::internal_error(format!("renaming {}: {e}", node.file.display()), None)
                })?;
                final_path = new_path;
            }
        }
    }

    let payload = serde_json::json!({ "id": p.id, "file": final_path, "title": p.title });
    Ok(json_result(&payload))
}

/// Compute the new filename for a renamed node: a leading
/// `YYYYMMDDHHMMSS-` timestamp from the old name is preserved; the rest is
/// the slug of the new title.
fn renamed_filename(old: &Path, title: &str) -> String {
    let stem = old.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let slug = slugify(title);
    match leading_timestamp(stem) {
        Some(prefix) => format!("{prefix}{slug}.org"),
        None => format!("{slug}.org"),
    }
}

/// A leading `YYYYMMDDHHMMSS-` timestamp prefix (14 digits + `-`), if any.
fn leading_timestamp(stem: &str) -> Option<&str> {
    let bytes = stem.as_bytes();
    if bytes.len() >= 15 && bytes[14] == b'-' && bytes[..14].iter().all(u8::is_ascii_digit) {
        Some(&stem[..15])
    } else {
        None
    }
}

/// `add_link` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddLinkParams {
    /// The source node's :ID: (the link is written into this node).
    pub id: String,

    /// The destination node's :ID:.
    pub target: String,

    /// Link description. Defaults to the target node's title.
    #[serde(default)]
    pub description: Option<String>,

    /// If set, append the link under this headline within the source node.
    #[serde(default)]
    pub headline: Option<String>,
}

/// `add_link` — write an `[[id:...][desc]]` link from one node to another.
/// Both nodes must exist. The link is appended to the source node's body
/// (or under a named headline).
///
/// # Errors
///
/// Returns an error if writes are disabled, either node is missing, or the
/// file cannot be written.
pub fn add_link(
    cfg: &Config,
    index: &Arc<dyn RoamIndex>,
    p: Parameters<AddLinkParams>,
) -> Result<CallToolResult, McpError> {
    ensure_writable(cfg)?;
    let p = p.0;
    let source = index
        .node(&p.id)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("source node not found", None))?;
    let target = index
        .node(&p.target)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .ok_or_else(|| McpError::invalid_params("target node not found", None))?;

    let description = p.description.unwrap_or(target.title);
    let link = format!("[[id:{}][{}]]", p.target, description);
    rewrite_file(&source.file, |text| {
        insert_under_headline(text, p.headline.as_deref(), &link);
        Ok(())
    })?;

    let payload = serde_json::json!({ "source": p.id, "target": p.target, "link": link });
    Ok(json_result(&payload))
}

// ── daily reads ─────────────────────────────────────────────────────────────

/// `get_daily_note` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct GetDailyParams {
    /// ISO date `YYYY-MM-DD`. Defaults to today.
    #[serde(default)]
    pub date: Option<String>,
}

/// `get_daily_note` — read the daily note for a date (default today)
/// without creating it. Returns whether it exists and, if so, its `:ID:`
/// and body.
///
/// # Errors
///
/// Returns an error if the date is malformed, the dailies pattern is
/// invalid, or the file cannot be read.
pub fn get_daily_note(
    cfg: &Config,
    p: Parameters<GetDailyParams>,
) -> Result<CallToolResult, McpError> {
    let date = match p.0.date {
        Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d")
            .map_err(|_| McpError::invalid_params("date must be YYYY-MM-DD", None))?,
        None => Local::now().date_naive(),
    };
    let stem = format_date(&cfg.dailies_format, date)?;
    let path = dailies_dir(cfg).join(format!("{stem}.org"));
    let iso = date.format("%Y-%m-%d").to_string();

    if !path.exists() {
        let payload = serde_json::json!({ "date": iso, "file": path, "exists": false });
        return Ok(json_result(&payload));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| McpError::internal_error(format!("reading {}: {e}", path.display()), None))?;
    let payload = serde_json::json!({
        "date": iso,
        "file": path,
        "exists": true,
        "id": extract_file_id(&text),
        "body": text,
    });
    Ok(json_result(&payload))
}

/// `list_dailies` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ListDailiesParams {
    /// Maximum number of notes to return, newest first. Defaults to 30.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `list_dailies` — list the `.org` notes in the dailies directory,
/// newest-first by filename, with each note's id and title.
///
/// # Errors
///
/// Returns an error if the dailies directory cannot be read.
pub fn list_dailies(
    cfg: &Config,
    p: Parameters<ListDailiesParams>,
) -> Result<CallToolResult, McpError> {
    let p = p.0;
    let dir = dailies_dir(cfg);
    let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
    if dir.is_dir() {
        let entries = std::fs::read_dir(&dir).map_err(|e| {
            McpError::internal_error(format!("reading {}: {e}", dir.display()), None)
        })?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("org") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let title = OrgDoc::from_text(text.clone())
                .document()
                .title()
                .unwrap_or_else(|| stem.clone());
            rows.push((
                stem.clone(),
                serde_json::json!({
                    "date": stem,
                    "file": path,
                    "id": extract_file_id(&text),
                    "title": title,
                }),
            ));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.truncate(p.limit.unwrap_or(30));
    let dailies: Vec<serde_json::Value> = rows.into_iter().map(|(_, v)| v).collect();
    let payload = serde_json::json!({ "dir": dir, "count": dailies.len(), "dailies": dailies });
    Ok(json_result(&payload))
}

/// The directory daily notes live in: `dailies_dir` under the roam dir, or
/// the roam dir itself.
fn dailies_dir(cfg: &Config) -> PathBuf {
    match &cfg.dailies_dir {
        Some(d) => cfg.roam_dir.join(d),
        None => cfg.roam_dir.clone(),
    }
}

/// Render any serializable value as a pretty-printed JSON tool result.
fn json_result(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
}

/// Render `date` with a user-supplied strftime pattern, rejecting (rather
/// than panicking on) patterns chrono can't parse or that need fields a
/// date doesn't have (e.g. `%H`).
fn format_date(pattern: &str, date: chrono::NaiveDate) -> Result<String, McpError> {
    use chrono::format::{Item, StrftimeItems};
    let items: Vec<Item<'_>> = StrftimeItems::new(pattern).collect();
    if items.iter().any(|i| matches!(i, Item::Error)) {
        return Err(McpError::internal_error(
            format!("invalid --dailies-format pattern: {pattern:?}"),
            None,
        ));
    }
    let mut out = String::new();
    write!(out, "{}", date.format_with_items(items.iter())).map_err(|_| {
        McpError::internal_error(
            format!("--dailies-format pattern {pattern:?} is not formattable for a date"),
            None,
        )
    })?;
    Ok(out)
}

/// Extract the file-level `:ID:` from an org property drawer. Only the
/// drawer before the first headline counts — a nested headline's `:ID:`
/// must not be mistaken for the file's.
fn extract_file_id(text: &str) -> Option<String> {
    for line in text.lines() {
        if line.starts_with('*') {
            return None;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(":ID:") {
            let val = rest.trim().to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_under_headline_appends_at_end_without_headline() {
        let mut text = String::from("#+title: T\n\nbody\n");
        insert_under_headline(&mut text, None, "new content");
        assert!(text.ends_with("\nnew content\n"));
    }

    #[test]
    fn insert_under_headline_inserts_before_next_sibling() {
        let mut text = String::from("#+title: T\n\n* First\nbody\n\n* Second\nother\n");
        insert_under_headline(&mut text, Some("First"), "added");
        let first_pos = text.find("added").unwrap();
        let second_pos = text.find("* Second").unwrap();
        assert!(
            first_pos < second_pos,
            "content must land inside First's subtree: {text}"
        );
    }

    #[test]
    fn insert_under_last_headline_terminates_and_appends() {
        // Regression: this used to loop forever (the last headline has no
        // next sibling).
        let mut text = String::from("#+title: T\n\n* First\nbody\n\n* Last\nbody\n");
        insert_under_headline(&mut text, Some("Last"), "tail content");
        assert!(text.contains("tail content"));
        assert!(text.ends_with("\ntail content\n"));
    }

    #[test]
    fn insert_under_missing_headline_appends_at_end() {
        let mut text = String::from("#+title: T\n\n* Only\nbody\n");
        insert_under_headline(&mut text, Some("No Such"), "fallback");
        assert!(text.ends_with("\nfallback\n"));
    }

    #[test]
    fn anchor_name_validation() {
        assert!(validate_anchor_name("para-1").is_ok());
        assert!(validate_anchor_name("with spaces").is_ok());
        assert!(validate_anchor_name("").is_err());
        assert!(validate_anchor_name("   ").is_err());
        assert!(validate_anchor_name("a>>b").is_err());
        assert!(validate_anchor_name("a<b").is_err());
        assert!(validate_anchor_name("a]b").is_err());
        assert!(validate_anchor_name("a\nb").is_err());
    }

    #[test]
    fn rewrite_file_refuses_when_file_changes_during_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.org");
        std::fs::write(&path, "original\n").unwrap();
        let err = rewrite_file(&path, |text| {
            // Simulate a concurrent writer (e.g. an Emacs save) landing
            // between our read and our write.
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(&path, "concurrent edit\n").unwrap();
            text.push_str("ours\n");
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "got: {err}");
        // The concurrent writer's content must survive.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "concurrent edit\n");
    }

    #[test]
    fn rewrite_file_applies_edit_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.org");
        std::fs::write(&path, "body\n").unwrap();
        rewrite_file(&path, |text| {
            text.push_str("more\n");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "body\nmore\n");
    }

    #[test]
    fn format_date_renders_valid_patterns() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        assert_eq!(format_date("%Y%m%d", d).unwrap(), "20260611");
        assert_eq!(format_date("%Y-%m-%d", d).unwrap(), "2026-06-11");
        assert_eq!(format_date("%Y/%m/%d", d).unwrap(), "2026/06/11");
    }

    #[test]
    fn format_date_rejects_bad_patterns_instead_of_panicking() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        // %Q is not a chrono specifier; %H needs a time component.
        assert!(format_date("%Q", d).is_err());
        assert!(format_date("%H%M", d).is_err());
    }

    #[test]
    fn daily_capture_honors_dailies_dir_and_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::from_args(dir.path(), false, true, None).unwrap();
        cfg.dailies_dir = Some(PathBuf::from("daily"));
        cfg.dailies_format = "%Y-%m-%d".to_string();

        let result = daily_capture(
            &cfg,
            Parameters(DailyCaptureParams {
                content: Some("captured".to_string()),
                headline: None,
            }),
        )
        .expect("daily_capture");
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let file = PathBuf::from(v["file"].as_str().unwrap());

        let today = Local::now().date_naive();
        let expected = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("daily")
            .join(format!("{}.org", today.format("%Y-%m-%d")));
        assert_eq!(file, expected);
        let body = std::fs::read_to_string(&file).unwrap();
        assert!(body.contains(":ID:"));
        assert!(body.contains("captured"));
    }

    #[test]
    fn extract_file_id_reads_top_drawer() {
        let text = ":PROPERTIES:\n:ID: abc-123\n:END:\n#+title: T\n";
        assert_eq!(extract_file_id(text).as_deref(), Some("abc-123"));
    }

    #[test]
    fn extract_file_id_ignores_headline_drawers() {
        // No file-level drawer; the only :ID: belongs to a headline.
        let text = "#+title: T\n\n* H\n:PROPERTIES:\n:ID: headline-id\n:END:\n";
        assert_eq!(extract_file_id(text), None);
    }
}
