//! The `sync_database` tool: make the otherwise-invisible db-sync engine
//! (see [`crate::sync::DbSyncer`]) observable and controllable from MCP.
//!
//! The tool reports the operational state of the sync subsystem (mode,
//! active backend, last sync, db presence) and — its headline feature —
//! the *drift* between the filesystem-scanner view of the vault and the
//! `org-roam.db` view, so an agent that has been writing notes can tell
//! whether its writes have actually propagated to the database Emacs reads.
//!
//! The handler itself lives on `RoamServer` (it needs the live index cell
//! and the syncer); this module holds the wire types and the backend-
//! independent drift computation.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::scan::ScanIndex;
use crate::index::sqlite::SqliteIndex;
use crate::index::{NodeQuery, RoamIndex};

/// Parameters for `sync_database`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SyncDatabaseParams {
    /// `false` (default): report the current sync state without syncing.
    /// `true`: force a sync now.
    #[serde(default)]
    pub force: bool,

    /// `true` (default): block until the sync completes. `false`: enqueue
    /// the sync and return immediately with a `sync_id`; poll its result by
    /// calling `sync_database` again with `force:false`.
    #[serde(default = "default_wait")]
    pub wait: bool,

    /// Maximum time to wait for a blocking sync, in milliseconds. `0` waits
    /// indefinitely. Defaults to 30000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Which index to sync: `"auto"` (default — whichever backend is
    /// active), `"scanner"` (rebuild the filesystem index), or `"sqlite"`
    /// (force an `org-roam-db-sync`).
    #[serde(default)]
    pub backend: Option<String>,
}

fn default_wait() -> bool {
    true
}

/// Which index a forced sync acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncBackend {
    /// Whichever backend is currently active.
    Auto,
    /// The filesystem scanner (re-walk the vault).
    Scanner,
    /// The `org-roam.db` `SQLite` database (via `org-roam-db-sync`).
    Sqlite,
}

impl SyncBackend {
    /// Parse the `backend` parameter. An empty / missing value is `Auto`.
    ///
    /// # Errors
    ///
    /// Returns an error string for any unrecognized value.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "scanner" => Ok(Self::Scanner),
            "sqlite" => Ok(Self::Sqlite),
            other => Err(format!(
                "unknown backend '{other}'; expected 'auto', 'scanner', or 'sqlite'"
            )),
        }
    }
}

/// Difference between the scanner view and the `SQLite` view of the vault.
#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    /// Number of nodes the filesystem scanner sees right now.
    pub scanner_node_count: usize,
    /// Number of nodes `org-roam.db` holds, or `null` when no db exists.
    pub sqlite_node_count: Option<usize>,
    /// Node IDs present on disk but missing from the db (un-synced writes).
    pub missing_in_sqlite: Vec<String>,
    /// Node IDs in the db but no longer on disk (stale db rows).
    pub missing_in_scanner: Vec<String>,
}

/// The `sync_database` response.
#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    /// Whether the requested operation completed without error.
    pub ok: bool,
    /// The configured sync mode (`ClientOnly` / `Full` / `Never`).
    pub mode: String,
    /// The backend reads currently go through (`scanner` / `sqlite`).
    pub active_backend: String,
    /// Resolved `org-roam.db` path.
    pub db_path: Option<String>,
    /// Whether that db file currently exists.
    pub db_exists: bool,
    /// RFC3339 timestamp of the last *successful* sync, if any.
    pub last_sync: Option<String>,
    /// Wall-clock time this call spent, in milliseconds.
    pub duration_ms: u64,
    /// Whether this call actually performed a sync (vs. reporting / skipping).
    pub synced: bool,
    /// Human-readable outcome of the sync (or last sync, for `force:false`).
    pub outcome: Option<String>,
    /// Set for `wait:false`: an id for the enqueued background sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_id: Option<String>,
    /// Set for `wait:false`: when the background sync was enqueued (RFC3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    /// Scanner-vs-sqlite drift.
    pub drift: DriftReport,
    /// Non-fatal notes (e.g. emacsclient unreachable, no db present).
    pub warnings: Vec<String>,
}

/// Compute drift between the on-disk scanner view and the `org-roam.db`
/// view. Opens both backends fresh so the result reflects current state,
/// never a cached snapshot. Returns the report plus any warnings.
#[must_use]
pub fn compute_drift(config: &Config) -> (DriftReport, Vec<String>) {
    let mut warnings = Vec::new();

    let scanner_set: HashSet<String> = match ScanIndex::open(&config.roam_dir) {
        Ok(idx) => all_node_ids(&idx).into_iter().collect(),
        Err(e) => {
            warnings.push(format!("scanner scan for drift failed: {e}"));
            HashSet::new()
        }
    };

    let (sqlite_node_count, missing_in_sqlite, missing_in_scanner) = if config.has_db() {
        match SqliteIndex::open(&config.db_path()) {
            Ok(idx) => {
                let sqlite_set: HashSet<String> = all_node_ids(&idx).into_iter().collect();
                let mut missing_in_sqlite: Vec<String> =
                    scanner_set.difference(&sqlite_set).cloned().collect();
                let mut missing_in_scanner: Vec<String> =
                    sqlite_set.difference(&scanner_set).cloned().collect();
                missing_in_sqlite.sort();
                missing_in_scanner.sort();
                (
                    Some(sqlite_set.len()),
                    missing_in_sqlite,
                    missing_in_scanner,
                )
            }
            Err(e) => {
                warnings.push(format!("opening org-roam.db for drift failed: {e}"));
                (None, Vec::new(), Vec::new())
            }
        }
    } else {
        // No `org-roam.db` to compare against. This is the steady state
        // in `--no-db` mode and in any scanner-only deployment; it is
        // not a problem to flag, just the absence of a second view to
        // reconcile. `server_info` already reports `has_db: false` for
        // callers that care, and the scanner still picks up the
        // authoritative file set on its own.
        (None, Vec::new(), Vec::new())
    };

    (
        DriftReport {
            scanner_node_count: scanner_set.len(),
            sqlite_node_count,
            missing_in_sqlite,
            missing_in_scanner,
        },
        warnings,
    )
}

/// All node IDs an index knows about (empty on query error).
fn all_node_ids<R: RoamIndex + ?Sized>(idx: &R) -> Vec<String> {
    idx.find_nodes(&NodeQuery {
        query: None,
        tags: &[],
        limit: None,
    })
    .map(|nodes| nodes.into_iter().map(|n| n.id).collect())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parse_accepts_known_values() {
        assert_eq!(SyncBackend::parse("auto").unwrap(), SyncBackend::Auto);
        assert_eq!(SyncBackend::parse("").unwrap(), SyncBackend::Auto);
        assert_eq!(
            SyncBackend::parse("  Scanner ").unwrap(),
            SyncBackend::Scanner
        );
        assert_eq!(SyncBackend::parse("SQLITE").unwrap(), SyncBackend::Sqlite);
    }

    #[test]
    fn backend_parse_rejects_unknown() {
        assert!(SyncBackend::parse("postgres").is_err());
    }
}
