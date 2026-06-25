//! MCP tools for node validation.
//!
//! Two tools live here:
//!
//! - [`validate_node`] (overloaded): either validate a known node by `:ID:`
//!   (cross-node check against the index — stale `:ID:`, empty title,
//!   dangling `id:` links), or validate a raw source body that the caller
//!   is about to write (org-roam spec + structural well-formedness).
//!   The dispatcher picks a branch based on which input fields are set.
//!
//! - [`find_invalid_nodes`]: bulk-scan every `.org` file in the vault and
//!   return a flat list of validation issues. Read-only — never writes to
//!   disk or DB.
//!
//! Both tools return a JSON payload via `json_result` and follow the same
//! conventions as the rest of the crate (look at `query.rs` and `write.rs`
//! for the model).
//!
//! The source-body path is what the create/update tools use as a gate:
//! [`crate::tools::write::create_node`] and
//! [`crate::tools::write::update_node`] call
//! [`crate::validation::validate_node_source`] before writing and refuse
//! to write when the report is non-empty.

use std::path::Path;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::index::RoamIndex;
use crate::org::OrgDoc;
use crate::validation::{
    self, scan_directory_for_invalid, InvalidNodeEntry, ValidationIssue, ValidationReport,
    BULK_ISSUE_CAP,
};

// ── Overloaded `validate_node` ─────────────────────────────────────────────

/// `validate_node` parameters. The tool is overloaded on which fields
/// are set:
///
/// - If `body` is set, the body is validated directly (the new
///   source-validation branch). `id` is ignored.
/// - Otherwise, `id` is required and the tool runs the cross-node
///   check (existing behavior: stale `:ID:`, empty title, dangling
///   `id:` links).
///
/// The overload keeps the tool name stable for LLM clients that already
/// know `validate_node` for the ID-based check.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ValidateNodeParams {
    /// The node's `:ID:`. Required when validating an existing node
    /// (cross-node check); ignored when `body` is set.
    #[serde(default)]
    pub id: Option<String>,

    /// Raw org-mode text to validate against the org-roam spec. When
    /// set, `id` is ignored and the tool returns a list of issues
    /// (empty when the source is valid).
    #[serde(default)]
    pub body: Option<String>,
}

/// `validate_node` — overloaded source-or-ID validator. See
/// [`ValidateNodeParams`] for the dispatch rules.
///
/// # Errors
///
/// Returns an error if neither `id` nor `body` is provided, if the
/// `id` branch cannot find the node, or if the index query fails.
pub fn validate_node(
    config: &crate::config::Config,
    index: &Arc<dyn RoamIndex>,
    p: &Parameters<ValidateNodeParams>,
) -> Result<CallToolResult, McpError> {
    let p = &p.0;
    if let Some(body) = p.body.as_deref() {
        Ok(validate_source(body))
    } else {
        let id = p.id.as_deref().ok_or_else(|| {
            McpError::invalid_params("validate_node requires either `id` or `body`", None)
        })?;
        validate_by_id(config, index, id)
    }
}

/// Source-body branch: validate `body` against the org-roam spec and
/// the structural well-formedness rules. Returns
/// `isError: Some(true)` when the report is non-empty so the MCP
/// client surfaces the issues as a tool error.
fn validate_source(body: &str) -> CallToolResult {
    let report = validation::validate_node_source(body);
    let payload = serde_json::json!({
        "ok": report.is_ok(),
        "issues": report.issues,
    });
    if report.is_ok() {
        CallToolResult::structured(payload)
    } else {
        CallToolResult::structured_error(payload)
    }
}

/// ID branch: keep the original cross-node semantics. Refactored out
/// of `tools/query.rs` so the same entry-point handles both branches.
fn validate_by_id(
    config: &crate::config::Config,
    index: &Arc<dyn RoamIndex>,
    id: &str,
) -> Result<CallToolResult, McpError> {
    let internal =
        |e: &dyn std::fmt::Display| -> McpError { McpError::internal_error(e.to_string(), None) };
    let node = index
        .node(id)
        .map_err(|e| internal(&e))?
        .ok_or_else(|| McpError::invalid_params("node not found", None))?;

    let text = std::fs::read_to_string(&node.file).map_err(|e| internal(&e))?;
    let mut report = validation::validate_node_source(&text);
    validation::validate_node_with_context(
        &text,
        &config.roam_dir,
        &node.file,
        Some(index.as_ref()),
        &mut report,
    );

    let mut issues: Vec<String> = report.issues.into_iter().map(|i| i.message).collect();
    let doc = OrgDoc::from_text(text);
    let id_in_file = if node.is_file() {
        doc.document()
            .properties()
            .and_then(|props| props.get("ID"))
            .is_some_and(|v| v.trim() == id)
    } else {
        doc.headline_by_id(id).is_some()
    };
    if !id_in_file {
        issues
            .push("index references this :ID: but it is no longer present in the file".to_string());
    }
    if node.title.trim().is_empty() {
        issues.push("node has an empty title".to_string());
    }

    let mut dangling: Vec<String> = Vec::new();
    for l in index.forward_links(id).map_err(|e| internal(&e))? {
        if l.kind == "id" {
            if let Some(dest) = &l.dest {
                if index.node(dest).map_err(|e| internal(&e))?.is_none() {
                    dangling.push(dest.clone());
                }
            }
        }
    }
    if !dangling.is_empty() {
        issues.push(format!("{} dangling id link(s)", dangling.len()));
    }

    let payload = serde_json::json!({
        "id": id,
        "ok": issues.is_empty(),
        "issues": issues,
        "dangling_links": dangling,
    });
    Ok(json_result(&payload))
}

// ── `find_invalid_nodes` ───────────────────────────────────────────────────

/// `find_invalid_nodes` parameters. Currently empty — the tool walks
/// the org-roam directory and returns every file that has at least one
/// issue. Reserved for future filters (e.g. tag / file-path globs).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct FindInvalidNodesParams {}

/// Bulk-validate every `.org` file in the vault. Read-only: never
/// writes to disk or DB, never rebuilds the index.
///
/// The result is a flat per-issue list. When the number of issues
/// exceeds [`BULK_ISSUE_CAP`], the result is truncated and the
/// `truncated` flag is set.
///
/// # Errors
///
/// Returns an error if the org-roam directory cannot be read.
pub fn find_invalid_nodes(
    cfg: &crate::config::Config,
    p: &Parameters<FindInvalidNodesParams>,
) -> Result<CallToolResult, McpError> {
    let _ = p; // no params for now; reserved for future filters
    let report = scan_directory_for_invalid(&cfg.roam_dir)
        .map_err(|e| McpError::internal_error(format!("scan: {e}"), None))?;
    Ok(json_result_bulk(&report))
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Render a `serde_json::Value` as a structured tool result.
/// Private to this module — duplicated from `tools/query.rs` /
/// `tools/write.rs` rather than promoting the originals.
fn json_result(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::structured(value.clone())
}

fn json_result_bulk(report: &validation::BulkValidationReport) -> CallToolResult {
    let value = serde_json::json!({
        "scanned": report.scanned,
        "with_issues": report.with_issues,
        "issue_count": report.issues.len(),
        "truncated": report.truncated,
        "cap": BULK_ISSUE_CAP,
        "issues": report.issues,
    });
    json_result(&value)
}

// Re-exported for tests.
#[allow(dead_code)]
pub(crate) type _ReexportInvalidNodeEntry = InvalidNodeEntry;
#[allow(dead_code)]
pub(crate) type _ReexportValidationReport = ValidationReport;
#[allow(dead_code)]
pub(crate) type _ReexportValidationIssue = ValidationIssue;
#[allow(dead_code)]
pub(crate) fn _reexport_scan_directory_for_invalid(
    root: &Path,
) -> std::io::Result<validation::BulkValidationReport> {
    scan_directory_for_invalid(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_entry_serializes_flat() {
        // Lock in the wire shape: node_id/file_path/issue are flat
        // siblings, not nested, so a downstream client can iterate
        // uniformly with the per-node `validate_node` body.
        let e = InvalidNodeEntry {
            node_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            file_path: "/tmp/x.org".to_string(),
            issue: ValidationIssue {
                kind_group: crate::validation::IssueGroup::OrgRoam,
                variant: "malformed_id_drawer".to_string(),
                message: "x".to_string(),
                line: Some(3),
                column: Some(1),
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"node_id\""));
        assert!(s.contains("\"file_path\""));
        assert!(s.contains("\"kind_group\""));
        assert!(s.contains("\"variant\":\"malformed_id_drawer\""));
        // No nesting of `issue`:
        assert!(s.contains("\"message\":\"x\""));
    }
}
