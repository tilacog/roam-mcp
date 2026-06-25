//! MCP server: the `RoamServer` struct, `ServerHandler` impl, tool/prompt
//! routers, file watching, and resource subscriptions.
//!
//! Read-side tools are always registered. In read-only mode the write
//! tools are removed from the tool router at construction time, so they
//! are neither listed nor callable; the write tools additionally guard
//! themselves at runtime for direct library callers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Annotated, CallToolResult, CompleteRequestParams, CompleteResult, CompletionInfo, Content,
    GetPromptRequestParams, GetPromptResult, Implementation, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, PromptMessage,
    PromptMessageRole, ProtocolVersion, RawResourceTemplate, ReadResourceRequestParams,
    ReadResourceResult, Reference, ResourceContents, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::prompt;
use rmcp::prompt_handler;
use rmcp::prompt_router;
use rmcp::service::{Peer, RequestContext};
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::{NodeMeta, NodeQuery, RoamIndex};
use crate::sync::{DbSyncer, SyncMode};
use crate::tools::content;
use crate::tools::query;
use crate::tools::retrieval;
use crate::tools::sync_tool::{self, SyncBackend, SyncDatabaseParams, SyncReport};
use crate::tools::validation_tools;
use crate::tools::write as write_tools;

/// Subscribed peers: URI → (session id → peer). The MCP unsubscribe
/// request only carries a URI, so peers are keyed by the session that
/// registered them — one session unsubscribing must not evict another
/// session's subscription to the same resource.
type Subscriptions = Arc<Mutex<HashMap<String, HashMap<u64, Peer<RoleServer>>>>>;

/// Peers (with their session ids) subscribed to one URI.
type SessionPeers = Vec<(u64, Peer<RoleServer>)>;

/// Process-wide source of session ids (see [`RoamServer::for_new_session`]).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// The names of the tools removed from the router in read-only mode.
const WRITE_TOOLS: &[&str] = &[
    "create_node",
    "append_to_node",
    "prepend_to_node",
    "insert_anchor",
    "daily_capture",
    "update_node",
    "delete_node",
    "rename_node",
    "add_link",
    "add_tag",
    "remove_tag",
    "set_tags",
];

/// The org-roam MCP server.
///
/// Wraps a [`RoamIndex`] for metadata queries; content reads go through
/// `tools::content` (which parses files with `org::parse::OrgDoc`).
///
/// The index is held behind an `RwLock` so it can be hot-reloaded: the
/// file-watcher background task swaps it when the database (or, in
/// scanner mode, any `.org` file) changes, and the write tools swap it
/// after a successful write so clients immediately read their own writes.
#[derive(Clone)]
pub struct RoamServer {
    pub config: Arc<Config>,
    /// The live index. Acquired via `get_index()`.
    index_cell: Arc<RwLock<Arc<dyn RoamIndex>>>,
    pub tool_router: ToolRouter<Self>,
    pub prompt_router: PromptRouter<Self>,
    /// Subscriptions shared across sessions (the watcher notifies all).
    subscriptions: Subscriptions,
    /// This instance's session key into `subscriptions`.
    session_id: u64,
    /// Debounced `org-roam-db-sync` trigger; called after every successful write.
    syncer: Arc<DbSyncer>,
}

/// Result of the `force:true` branch of `sync_database`, folded into the
/// final [`SyncReport`] by `run_sync_database`.
struct ForcedSync {
    ok: bool,
    did_sync: bool,
    outcome: Option<String>,
    job_id: Option<String>,
    queued_at: Option<String>,
    warnings: Vec<String>,
}

impl ForcedSync {
    /// A neutral starting point: no sync performed yet, no error.
    fn pending() -> Self {
        Self {
            ok: true,
            did_sync: false,
            outcome: None,
            job_id: None,
            queued_at: None,
            warnings: Vec::new(),
        }
    }
}

impl RoamServer {
    /// Build a new server. Picks an index backend based on the config and
    /// spawns the file-watcher background task.
    ///
    /// # Errors
    ///
    /// Returns an error if the index backend cannot be opened.
    pub fn new(config: Config) -> Result<Self, crate::index::IndexError> {
        let index = crate::index::open(&config)?;
        let server = Self::assemble(config, index);
        server.spawn_watcher();
        Ok(server)
    }

    /// Like `new` but with an explicit index (used in tests; no watcher spawned).
    pub fn with_index(config: Config, index: Arc<dyn RoamIndex>) -> Self {
        Self::assemble(config, index)
    }

    fn assemble(config: Config, index: Arc<dyn RoamIndex>) -> Self {
        let syncer = DbSyncer::new(config.sync_config());
        let mut tool_router = Self::tool_router();
        if !config.can_write() {
            for name in WRITE_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self {
            config: Arc::new(config),
            index_cell: Arc::new(RwLock::new(index)),
            tool_router,
            prompt_router: Self::prompt_router(),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            syncer,
        }
    }

    /// A handle for serving one more session (HTTP transport): shares the
    /// config, index, watcher, and subscription map, but gets its own
    /// session id so its subscriptions are tracked separately.
    #[must_use]
    pub fn for_new_session(&self) -> Self {
        let mut clone = self.clone();
        clone.session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        clone
    }

    /// Return a clone of the current index Arc, briefly holding the read lock.
    ///
    /// # Panics
    ///
    /// Panics if the index `RwLock` is poisoned.
    #[must_use]
    pub fn get_index(&self) -> Arc<dyn RoamIndex> {
        self.index_cell.read().expect("index lock poisoned").clone()
    }

    /// Rebuild the index after a successful write so the next tool call
    /// sees the change. Only needed in scanner mode — with a database,
    /// Emacs's `org-roam-db-sync` updates the file and the watcher
    /// reloads from it.
    fn refresh_index_after_write(&self) {
        if self.config.has_db() {
            return;
        }
        match crate::index::open(&self.config) {
            Ok(idx) => *self.index_cell.write().expect("index lock poisoned") = idx,
            Err(e) => tracing::warn!("index refresh after write failed: {e}"),
        }
    }

    /// Force a rebuild of the filesystem-scanner index and swap it in.
    /// Returns the rebuilt node count.
    fn rebuild_scanner_index(&self) -> Result<usize, crate::index::IndexError> {
        let idx = crate::index::scan::ScanIndex::open(&self.config.roam_dir)?;
        let count = idx.node_count()?;
        *self.index_cell.write().expect("index lock poisoned") = Arc::new(idx);
        Ok(count)
    }

    /// Implementation of the `sync_database` tool. Reports the operational
    /// state of the sync subsystem and the scanner-vs-sqlite drift, and —
    /// when `force` is set — triggers a sync of the requested backend.
    async fn run_sync_database(&self, p: SyncDatabaseParams) -> Result<SyncReport, McpError> {
        let backend = SyncBackend::parse(p.backend.as_deref().unwrap_or("auto"))
            .map_err(|e| McpError::invalid_params(e, None))?;
        let cfg = &self.config;
        let mode = self.syncer.mode().clone();
        let start = std::time::Instant::now();

        // `auto` resolves to whichever backend reads currently go through.
        let effective = match backend {
            SyncBackend::Auto if cfg.has_db() => SyncBackend::Sqlite,
            SyncBackend::Auto => SyncBackend::Scanner,
            other => other,
        };

        let forced = if p.force {
            self.force_sync(effective, &mode, p.wait, p.timeout_ms.unwrap_or(30_000))
                .await
        } else {
            ForcedSync::pending()
        };

        let (drift, drift_warnings) = sync_tool::compute_drift(cfg);
        let mut warnings = forced.warnings;
        warnings.extend(drift_warnings);

        let state = self.syncer.state();
        let active_backend = if cfg.has_db() { "sqlite" } else { "scanner" };

        Ok(SyncReport {
            ok: forced.ok,
            mode: format!("{mode:?}"),
            active_backend: active_backend.to_string(),
            db_path: Some(cfg.db_path().display().to_string()),
            db_exists: cfg.has_db(),
            last_sync: state.last_sync.map(|t| t.to_rfc3339()),
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            synced: forced.did_sync,
            outcome: forced.outcome.or(state.last_outcome),
            sync_id: forced.job_id,
            queued_at: forced.queued_at,
            drift,
            warnings,
        })
    }

    /// Perform a forced sync of the (already auto-resolved) `effective`
    /// backend. Never returns an `Err`: failures and skips are recorded in
    /// the returned [`ForcedSync`] so the report can still carry drift.
    async fn force_sync(
        &self,
        effective: SyncBackend,
        mode: &SyncMode,
        wait: bool,
        timeout_ms: u64,
    ) -> ForcedSync {
        let mut f = ForcedSync::pending();
        match effective {
            SyncBackend::Scanner => {
                if self.config.has_db() {
                    f.warnings.push(
                        "rebuilt the scanner index while an org-roam.db is active; the next db \
                         change will swap the sqlite backend back in"
                            .to_string(),
                    );
                }
                match self.rebuild_scanner_index() {
                    Ok(_) => {
                        f.did_sync = true;
                        f.outcome = Some("scanner index rebuilt".to_string());
                    }
                    Err(e) => {
                        f.ok = false;
                        f.outcome = Some("scanner rebuild failed".to_string());
                        f.warnings.push(format!("scanner rebuild failed: {e}"));
                    }
                }
            }
            SyncBackend::Sqlite if *mode == SyncMode::Never => {
                f.warnings.push(
                    "sync-mode is 'never'; sync_database is a no-op. Restart with \
                     --sync-mode=client-only or --sync-mode=full to enable syncing."
                        .to_string(),
                );
                f.outcome = Some("skipped — sync-mode never".to_string());
            }
            SyncBackend::Sqlite => {
                if *mode == SyncMode::ClientOnly && !self.syncer.emacsclient_reachable().await {
                    f.warnings.push(
                        "emacsclient not reachable; a client-only sync will be skipped. Pass \
                         backend='scanner' for a scanner rebuild, or restart with \
                         --sync-mode=full."
                            .to_string(),
                    );
                }
                if wait {
                    self.run_blocking_sync(&mut f, timeout_ms).await;
                } else {
                    f.queued_at = Some(chrono::Utc::now().to_rfc3339());
                    let syncer_handle = Arc::clone(&self.syncer);
                    tokio::spawn(async move {
                        let result = syncer_handle.sync_now().await;
                        tracing::info!("org-roam db sync (queued): {result}");
                    });
                    f.outcome = Some("queued".to_string());
                    f.job_id = Some(uuid::Uuid::new_v4().to_string());
                }
            }
            SyncBackend::Auto => unreachable!("auto resolved before force_sync"),
        }
        f
    }

    /// Run an `org-roam-db-sync` now and fold the outcome into `f`,
    /// honoring `timeout_ms` (0 = wait indefinitely).
    async fn run_blocking_sync(&self, f: &mut ForcedSync, timeout_ms: u64) {
        let result = if timeout_ms == 0 {
            Ok(self.syncer.sync_now().await)
        } else {
            tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                self.syncer.sync_now(),
            )
            .await
        };
        if let Ok(outcome) = result {
            f.did_sync = outcome.synced();
            let text = outcome.to_string();
            // A real sync, or a coalesce onto a concurrent one, is success;
            // anything else means we tried but could not reach Emacs.
            if !f.did_sync && !text.contains("coalesced") {
                f.ok = false;
            }
            f.outcome = Some(text);
        } else {
            f.ok = false;
            f.outcome = Some("timed out".to_string());
            f.warnings.push(format!(
                "sync timed out after {timeout_ms} ms; it may still be running in the background"
            ));
        }
    }

    /// Spawn a background task that watches the roam directory and the DB file.
    ///
    /// - `.org` file changes → send `notifications/resources/updated` to
    ///   subscribers; in scanner mode, also rebuild the index (external
    ///   edits would otherwise never become visible).
    /// - `org-roam.db` changes → reload the index.
    fn spawn_watcher(&self) {
        let config = self.config.clone();
        let index_cell = self.index_cell.clone();
        let subscriptions = self.subscriptions.clone();

        tokio::spawn(async move {
            use notify::{RecursiveMode, Watcher};

            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("file watcher init failed: {e}");
                    return;
                }
            };

            if let Err(e) = watcher.watch(&config.roam_dir, RecursiveMode::Recursive) {
                tracing::warn!("file watcher watch failed: {e}");
                return;
            }

            tracing::debug!("file watcher active on {}", config.roam_dir.display());
            let db_path = config.db_path();

            while let Some(first) = rx.recv().await {
                // Editor saves and git operations emit bursts of events;
                // wait a beat, then drain whatever queued so the whole
                // burst is handled with at most one index reload.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                handle_fs_events(batch, &db_path, &config, &index_cell, &subscriptions).await;
            }

            // Keep watcher alive until the channel is closed.
            drop(watcher);
        });
    }
}

fn events_to_paths(events: Vec<notify::Result<notify::Event>>) -> Vec<std::path::PathBuf> {
    use notify::EventKind;
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for res in events {
        match res {
            Ok(event) => match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                    paths.extend(event.paths);
                }
                _ => {}
            },
            Err(e) => tracing::warn!("watch error: {e}"),
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn maybe_reload_index(
    paths: &[std::path::PathBuf],
    db_path: &Path,
    config: &Config,
    index_cell: &Arc<RwLock<Arc<dyn RoamIndex>>>,
) {
    let is_org = |p: &Path| p.extension().and_then(|e| e.to_str()) == Some("org");
    if paths
        .iter()
        .any(|p| p == db_path || (is_org(p) && !config.has_db()))
    {
        match crate::index::open(config) {
            Ok(new_idx) => {
                *index_cell.write().expect("index lock") = new_idx;
                tracing::debug!("index reloaded ({} changed paths)", paths.len());
            }
            Err(e) => tracing::warn!("index reload failed: {e}"),
        }
    }
}

fn collect_to_notify(
    path: &Path,
    index: &Arc<dyn RoamIndex>,
    subs_map: &HashMap<String, HashMap<u64, Peer<RoleServer>>>,
) -> Vec<(String, SessionPeers)> {
    let mut to_notify: Vec<(String, SessionPeers)> = Vec::new();
    for (uri, peers) in subs_map {
        if let Some(id) = uri
            .strip_prefix("org-roam://node/")
            .map(|s| s.split_once('#').map_or(s, |(id, _)| id))
        {
            if let Ok(Some(meta)) = index.node(id) {
                if meta.file == *path {
                    to_notify.push((
                        uri.clone(),
                        peers.iter().map(|(s, p)| (*s, p.clone())).collect(),
                    ));
                }
            }
        }
    }
    to_notify
}

fn remove_dead_peers_for_uri(uri: &str, dead: Vec<u64>, subscriptions: &Subscriptions) {
    let mut subs = subscriptions.lock().expect("subscriptions lock");
    if let Some(list) = subs.get_mut(uri) {
        for session in dead {
            list.remove(&session);
        }
        if list.is_empty() {
            subs.remove(uri);
        }
    }
}

async fn dispatch_notifications(
    to_notify: Vec<(String, SessionPeers)>,
    subscriptions: &Subscriptions,
) {
    for (uri, peers) in to_notify {
        let mut dead = Vec::new();
        for (session, peer) in &peers {
            let param = ResourceUpdatedNotificationParam { uri: uri.clone() };
            if peer.notify_resource_updated(param).await.is_err() {
                dead.push(*session);
            }
        }
        if !dead.is_empty() {
            remove_dead_peers_for_uri(&uri, dead, subscriptions);
        }
    }
}

async fn notify_org_path(
    path: &Path,
    index_cell: &Arc<RwLock<Arc<dyn RoamIndex>>>,
    subscriptions: &Subscriptions,
) {
    let index = index_cell.read().expect("index lock").clone();
    let to_notify = {
        let subs = subscriptions.lock().expect("subscriptions lock");
        collect_to_notify(path, &index, &subs)
    };
    dispatch_notifications(to_notify, subscriptions).await;
}

/// Handle a coalesced batch of filesystem events from `notify`: at most
/// one index reload for the batch, then per-file subscriber notifications.
async fn handle_fs_events(
    events: Vec<notify::Result<notify::Event>>,
    db_path: &Path,
    config: &Config,
    index_cell: &Arc<RwLock<Arc<dyn RoamIndex>>>,
    subscriptions: &Subscriptions,
) {
    let paths = events_to_paths(events);
    maybe_reload_index(&paths, db_path, config, index_cell);
    let is_org = |p: &Path| p.extension().and_then(|e| e.to_str()) == Some("org");
    for path in paths.iter().filter(|p| is_org(p)) {
        notify_org_path(path, index_cell, subscriptions).await;
    }
}

// ── Prompt params ─────────────────────────────────────────────────────────────

/// Upper bound on the node body interpolated into the `summarize-node`
/// prompt, in characters. Past this the body is truncated so a single
/// huge note can't produce an unbounded prompt.
const MAX_SUMMARY_BODY_CHARS: usize = 50_000;

/// Default number of link candidates surfaced by `link-suggestions`.
const DEFAULT_LINK_CANDIDATES: usize = 50;

/// Hard cap on link candidates, so a caller can't dump the whole vault
/// into one prompt by passing a huge `limit`.
const MAX_LINK_CANDIDATES: usize = 200;

/// Default / hard cap on orphans listed by `orphan-triage`.
const DEFAULT_TRIAGE_ORPHANS: usize = 50;
const MAX_TRIAGE_ORPHANS: usize = 200;

/// Default / hard cap on the existing-tag vocabulary shown by
/// `tag-suggestions` (most-used tags first).
const DEFAULT_TAG_VOCAB: usize = 100;
const MAX_TAG_VOCAB: usize = 500;

/// `summarize-node` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SummarizeNodeParams {
    /// The node's :ID:.
    pub id: String,
}

/// `link-suggestions` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkSuggestionsParams {
    /// Draft text to find link targets for.
    pub draft: String,

    /// Maximum number of candidate nodes to include. Defaults to 50,
    /// capped at 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `orphan-triage` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrphanTriageParams {
    /// Maximum number of orphan notes to include. Defaults to 50, capped
    /// at 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `tag-suggestions` parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TagSuggestionsParams {
    /// The node's :ID:.
    pub id: String,

    /// Maximum number of existing vault tags to show as vocabulary
    /// (most-used first). Defaults to 100, capped at 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Truncate `s` to at most `max` characters, on a char boundary. Returns
/// the (possibly shortened) slice and whether truncation occurred.
fn truncate_chars(s: &str, max: usize) -> (&str, bool) {
    match s.char_indices().nth(max) {
        Some((idx, _)) => (&s[..idx], true),
        None => (s, false),
    }
}

/// Render a node as a Markdown bullet for a prompt: `- [[id:UUID][Title]]`,
/// with a trailing `(tags: …)` when the node carries tags.
fn node_bullet(node: &NodeMeta) -> String {
    if node.tags.is_empty() {
        format!("- [[id:{}][{}]]", node.id, node.title)
    } else {
        format!(
            "- [[id:{}][{}]] (tags: {})",
            node.id,
            node.title,
            node.tags.join(", ")
        )
    }
}

/// Completion suggestions for a prompt's `id` argument: node ids whose id
/// prefix-matches `value`, or whose title/aliases contain it (both
/// case-insensitive). An empty `value` matches every node. Capped at
/// [`CompletionInfo::MAX_VALUES`], with `total`/`has_more` reported so the
/// client knows when the list was clipped.
fn node_id_completions(index: &Arc<dyn RoamIndex>, value: &str) -> CompletionInfo {
    let Ok(nodes) = index.find_nodes(&NodeQuery::default()) else {
        return CompletionInfo::default();
    };
    let needle = value.to_lowercase();
    let mut values: Vec<String> = nodes
        .into_iter()
        .filter(|n| {
            needle.is_empty()
                || n.id.to_lowercase().starts_with(&needle)
                || n.title.to_lowercase().contains(&needle)
                || n.aliases.iter().any(|a| a.to_lowercase().contains(&needle))
        })
        .map(|n| n.id)
        .collect();
    let total = u32::try_from(values.len()).unwrap_or(u32::MAX);
    let has_more = values.len() > CompletionInfo::MAX_VALUES;
    values.truncate(CompletionInfo::MAX_VALUES);
    CompletionInfo {
        values,
        total: Some(total),
        has_more: Some(has_more),
    }
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

/// `ping` — simple liveness tool. Useful for `mcp inspector` and CI.
#[derive(Debug, Serialize, Deserialize, JsonSchema, Default)]
pub struct PingParams {}

#[tool_router]
impl RoamServer {
    #[tool(description = "Liveness check; returns 'pong'.")]
    async fn ping(&self, _p: Parameters<PingParams>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }

    #[tool(
        description = "Information about this org-roam index: backend, file count, version, sync and dailies config."
    )]
    async fn server_info(&self) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let node_count = index
            .node_count()
            .map_err(|e| McpError::internal_error(format!("index error: {e}"), None))?;
        let cfg = &self.config;
        let backend = if cfg.has_db() { "sqlite" } else { "scanner" };
        // Surface a hint when dailies_dir is unset: `daily_capture` will
        // land daily notes at the roam-dir root in that case, which
        // diverges from the org-roam-dailies convention of `notes/daily/`.
        // The hint names the flag to use so the caller can fix the
        // config without reading the source.
        let dailies_dir = cfg.dailies_dir.as_ref().map(|d| d.display().to_string());
        let dailies_hint = if cfg.dailies_dir.is_none() {
            Some(
                "dailies.dir is unset: daily_capture will write to the roam-dir root. \
                 Pass --dailies-dir (e.g. --dailies-dir daily) and \
                 --dailies-format %Y-%m-%d to match the org-roam-dailies default layout."
                    .to_string(),
            )
        } else {
            None
        };
        let info = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "backend": backend,
            "index_source": index.source(),
            "node_count": node_count,
            "read_only": !cfg.can_write(),
            "roam_dir": cfg.roam_dir,
            "db_path": cfg.db_path(),
            "has_db": cfg.has_db(),
            "dailies": {
                "dir": dailies_dir,
                "format": cfg.dailies_format,
                "hint": dailies_hint,
            },
            "sync": {
                "mode": format!("{:?}", cfg.sync_mode),
                "debounce_ms": cfg.sync_debounce_ms,
                "timeout_s": cfg.sync_timeout_s,
                "last_sync": self.syncer.state().last_sync.map(|t| t.to_rfc3339()),
            },
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Force a sync of org-roam.db from the on-disk vault, or report the current sync state. With force:false (default) it reports mode, active backend, last sync, and the drift between the scanner view and the sqlite view (un-synced writes show up as missing_in_sqlite). With force:true it triggers a sync of the chosen backend. In --sync-mode never it is a no-op with a warning."
    )]
    async fn sync_database(
        &self,
        p: Parameters<SyncDatabaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let report = self.run_sync_database(p.0).await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Search org-roam nodes by title, alias, or tag")]
    async fn search_nodes(
        &self,
        p: Parameters<query::SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::search_nodes(&index, p)
    }

    #[tool(description = "Look up a single node by its :ID:; returns its metadata and full body")]
    async fn get_node(
        &self,
        p: Parameters<query::GetNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_node(&index, &p)
    }

    #[tool(description = "Return a random node from the index")]
    async fn random_node(
        &self,
        p: Parameters<query::RandomNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::random_node(&index, p)
    }

    #[tool(
        description = "Get a sub-section of a node by anchor: CUSTOM_ID, headline title, dedicated target, or free text"
    )]
    async fn get_node_section(
        &self,
        p: Parameters<content::GetNodeSectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        content::get_node_section(&index, &p)
    }

    #[tool(description = "List nodes that link to a given node (backlinks)")]
    async fn get_backlinks(
        &self,
        p: Parameters<query::GetNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_backlinks(&index, &p)
    }

    #[tool(description = "List outgoing links from a node")]
    async fn get_forward_links(
        &self,
        p: Parameters<query::GetNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_forward_links(&index, &p)
    }

    #[tool(description = "Find nodes by ROAM_REFS value (URL or @citekey)")]
    async fn find_by_ref(
        &self,
        p: Parameters<query::FindByRefParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::find_by_ref(&index, &p)
    }

    #[tool(description = "List all tags and the count of nodes that have each")]
    async fn list_tags(&self) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_tags(&index)
    }

    #[tool(
        description = "Find plain-text occurrences of a node's title or aliases elsewhere in the vault (capped)"
    )]
    async fn unlinked_references(
        &self,
        p: Parameters<query::UnlinkedParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::unlinked_references(&index, &self.config.roam_dir, &p)
    }

    #[tool(
        description = "Enumerate vault nodes with pagination (limit/offset) and sorting; returns the total count"
    )]
    async fn list_nodes(
        &self,
        p: Parameters<query::ListNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_nodes(&index, p)
    }

    #[tool(
        description = "List notes that have no edges in the id link graph: no outgoing id: links and no incoming id: links (backlinks). These notes exist in the vault but are unreachable from any other note, so they are prime candidates for triage (merge, link, or delete). URL, file, citation, and fuzzy links do not count as edges. Returns a paginated page sorted by title (ascending by default)."
    )]
    async fn list_orphans(
        &self,
        p: Parameters<query::ListOrphansParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_orphans(&index, p)
    }

    #[tool(description = "Full-text search across node bodies (not just titles/aliases/tags)")]
    async fn search_text(
        &self,
        p: Parameters<query::SearchTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::search_text(&index, &self.config.roam_dir, p)
    }

    #[tool(description = "Look up a node by its file path (absolute or relative to the roam dir)")]
    async fn get_node_by_path(
        &self,
        p: Parameters<query::GetNodeByPathParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_node_by_path(&index, &self.config.roam_dir, &p)
    }

    #[tool(
        description = "List the ROAM_REFS (and v1 ROAM_KEY) values a node declares; inverse of find_by_ref"
    )]
    async fn get_refs(
        &self,
        p: Parameters<query::GetNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_refs(&index, &p)
    }

    #[tool(
        description = "List a node's addressable anchors: dedicated targets, headlines, and CUSTOM_IDs"
    )]
    async fn list_anchors(
        &self,
        p: Parameters<query::GetNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_anchors(&index, &p)
    }

    #[tool(description = "Count tags that co-occur with a given tag across the vault")]
    async fn tag_cooccurrences(
        &self,
        p: Parameters<query::TagCooccurrenceParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::tag_cooccurrences(&index, &p)
    }

    #[tool(description = "List every node that contains external links (file, http, https, cite)")]
    async fn list_external_links(
        &self,
        p: Parameters<query::ListExternalLinksParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_external_links(&index, p)
    }

    #[tool(
        description = "Validate a node. Pass `body` (raw org text) to check the source against the org-roam spec (returned issues include line/column). Pass `id` to check an existing node against the index (stale :ID:, empty title, dangling id links, and broken external links)."
    )]
    async fn validate_node(
        &self,
        p: Parameters<validation_tools::ValidateNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        validation_tools::validate_node(&self.config, &index, &p)
    }

    #[tool(
        description = "Walk every .org file in the vault and return a flat list of validation issues. Read-only."
    )]
    async fn find_invalid_nodes(
        &self,
        p: Parameters<validation_tools::FindInvalidNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        validation_tools::find_invalid_nodes(&self.config, &p)
    }

    #[tool(
        description = "List org-roam nodes that have a TODO keyword. Optionally filter by state (e.g. [\"TODO\",\"IN-PROGRESS\"]), priority (\"A\"/\"B\"/\"C\"), and tags. Supports pagination and sort by title or priority."
    )]
    async fn list_tasks(
        &self,
        p: Parameters<query::ListTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_tasks(&index, p)
    }

    #[tool(
        description = "Return the hierarchical heading outline of the file that contains a given node. Includes every headline's level, TODO state, priority, and tags."
    )]
    async fn get_outline(
        &self,
        p: Parameters<query::GetOutlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::get_outline(&index, &p)
    }

    #[tool(
        description = "List every .org file in the vault regardless of whether it has a file-level :ID:. Each entry includes path, size, mtime, and the node ID/title if known."
    )]
    async fn list_files(
        &self,
        p: Parameters<query::ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_files(&index, &self.config.roam_dir, p)
    }

    #[tool(description = "Read the daily note for a date (default today) without creating it")]
    async fn get_daily_note(
        &self,
        p: Parameters<write_tools::GetDailyParams>,
    ) -> Result<CallToolResult, McpError> {
        write_tools::get_daily_note(&self.config, p)
    }

    #[tool(description = "List the notes in the dailies directory, newest first")]
    async fn list_dailies(
        &self,
        p: Parameters<write_tools::ListDailiesParams>,
    ) -> Result<CallToolResult, McpError> {
        write_tools::list_dailies(&self.config, p)
    }

    // -- Write tools (removed from the router when read-only) --

    #[tool(description = "Create a new org-roam node (.org file) with a fresh :ID:")]
    async fn create_node(
        &self,
        p: Parameters<write_tools::CreateNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = write_tools::create_node(&self.config, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Append content to an existing node, optionally under a specific headline"
    )]
    async fn append_to_node(
        &self,
        p: Parameters<write_tools::AppendParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::append_to_node(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Insert a dedicated target <<name>> before a matched paragraph; returns [[id:UUID::name]]"
    )]
    async fn insert_anchor(
        &self,
        p: Parameters<write_tools::InsertAnchorParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::insert_anchor(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Create or retrieve today's daily note and optionally append content to it"
    )]
    async fn daily_capture(
        &self,
        p: Parameters<write_tools::DailyCaptureParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = write_tools::daily_capture(&self.config, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Edit a file-level node's title/body/tags/aliases/refs/properties in place (idempotent, keyed on :ID:). Pass preview:true for a dry run."
    )]
    async fn update_node(
        &self,
        p: Parameters<write_tools::UpdateNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let preview = p.0.preview;
        let result = write_tools::update_node(&self.config, &index, p)?;
        if !preview {
            self.refresh_index_after_write();
            self.syncer.schedule();
        }
        Ok(result)
    }

    #[tool(
        description = "Delete a node: the whole file for a file node, or just the subtree for a headline node"
    )]
    async fn delete_node(
        &self,
        p: Parameters<write_tools::DeleteNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::delete_node(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Insert content at the start of a node's body (counterpart to append_to_node)"
    )]
    async fn prepend_to_node(
        &self,
        p: Parameters<write_tools::PrependParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::prepend_to_node(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Change a file-level node's title and rename its file to match (backlinks keyed on :ID: are unaffected)"
    )]
    async fn rename_node(
        &self,
        p: Parameters<write_tools::RenameNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::rename_node(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(description = "Write an [[id:...]] link from one node to another; both must exist")]
    async fn add_link(
        &self,
        p: Parameters<write_tools::AddLinkParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::add_link(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "List the file-level #+filetags: (plus v1 #+ROAM_TAGS:) tags on a node, read from disk"
    )]
    async fn list_node_tags(
        &self,
        p: Parameters<query::ListNodeTagsParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::list_node_tags(&index, &p)
    }

    #[tool(
        description = "Check whether a node has a specific #+filetags: tag (exact, case-sensitive)"
    )]
    async fn has_tag(
        &self,
        p: Parameters<query::HasTagParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::has_tag(&index, &p)
    }

    #[tool(
        description = "Find nodes whose file-level tags include a given tag (exact, case-sensitive), with pagination"
    )]
    async fn search_by_tag(
        &self,
        p: Parameters<query::SearchByTagParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        query::search_by_tag(&index, p)
    }

    #[tool(
        description = "Add one or more tags to a node's #+filetags: without overwriting existing tags (dedup, case-sensitive)"
    )]
    async fn add_tag(
        &self,
        p: Parameters<write_tools::AddTagParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::add_tag(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(
        description = "Remove one or more tags from a node's #+filetags:; absent tags are silently ignored (case-sensitive)"
    )]
    async fn remove_tag(
        &self,
        p: Parameters<write_tools::RemoveTagParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::remove_tag(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }

    #[tool(description = "Replace a node's entire #+filetags: set; an empty list removes all tags")]
    async fn set_tags(
        &self,
        p: Parameters<write_tools::SetTagParams>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.get_index();
        let result = write_tools::set_tags(&self.config, &index, p)?;
        self.refresh_index_after_write();
        self.syncer.schedule();
        Ok(result)
    }
}

// ── Prompt dispatch ───────────────────────────────────────────────────────────

#[prompt_router]
impl RoamServer {
    #[prompt(
        name = "summarize-node",
        description = "Build a prompt asking Claude to summarize an org-roam node"
    )]
    async fn summarize_node(
        &self,
        p: Parameters<SummarizeNodeParams>,
    ) -> Result<GetPromptResult, McpError> {
        let index = self.get_index();
        let body = content::read_node_body(&index, &p.0.id).map_err(McpError::from)?;

        let stale = body.stale_warning();
        let (excerpt, truncated) = truncate_chars(&body.body, MAX_SUMMARY_BODY_CHARS);
        let mut suffix = String::new();
        if truncated {
            suffix.push_str(
                "\n\n[note: the note body above was truncated for length; summarize what is shown]",
            );
        }
        if let Some(w) = stale {
            suffix.push_str("\n\n[note: ");
            suffix.push_str(w);
            suffix.push(']');
        }
        let user_text = format!(
            "Please write a concise summary of the following org-roam note titled \"{}\".\n\n{}{}",
            body.node.title, excerpt, suffix
        );
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            user_text,
        )])
        .with_description("Summarize an org-roam node"))
    }

    #[prompt(
        name = "link-suggestions",
        description = "Build a prompt asking Claude to suggest org-roam links for a draft text"
    )]
    async fn link_suggestions(
        &self,
        p: Parameters<LinkSuggestionsParams>,
    ) -> Result<GetPromptResult, McpError> {
        let index = self.get_index();
        let limit =
            p.0.limit
                .unwrap_or(DEFAULT_LINK_CANDIDATES)
                .min(MAX_LINK_CANDIDATES);
        let candidates = retrieval::relevant_candidates(&index, &p.0.draft, limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let user_text = if candidates.is_empty() {
            format!(
                "No notes in the org-roam vault lexically match the draft below, so there are \
                 no link suggestions to offer. The relevant notes may not exist yet, or may use \
                 different wording than the draft.\n\n## Draft\n{}",
                p.0.draft
            )
        } else {
            let node_list = candidates
                .iter()
                .map(|c| node_bullet(&c.node))
                .collect::<Vec<_>>()
                .join("\n");

            format!(
                "Given the draft text below, decide which of the candidate org-roam notes \
                 genuinely belong as links, and where. The candidates were selected because \
                 their titles or aliases overlap the draft's wording, so the list is a starting \
                 point, not a verdict — omit any that do not actually fit. Format each \
                 suggestion as [[id:UUID][Title]] with a one-sentence reason.\n\n\
                 ## Draft\n{}\n\n## Candidate notes\n{}",
                p.0.draft, node_list
            )
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            user_text,
        )])
        .with_description("Suggest org-roam links for a draft text"))
    }

    #[prompt(
        name = "orphan-triage",
        description = "Build a prompt asking Claude to triage orphan notes (merge / link / delete / keep)"
    )]
    async fn orphan_triage(
        &self,
        p: Parameters<OrphanTriageParams>,
    ) -> Result<GetPromptResult, McpError> {
        let index = self.get_index();
        let limit =
            p.0.limit
                .unwrap_or(DEFAULT_TRIAGE_ORPHANS)
                .min(MAX_TRIAGE_ORPHANS);
        let mut orphans = index
            .orphans()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let total = orphans.len();
        orphans.truncate(limit);

        let user_text = if orphans.is_empty() {
            "Every note in the org-roam vault has at least one incoming or outgoing id: link, \
             so there are no orphans to triage."
                .to_string()
        } else {
            let node_list = orphans
                .iter()
                .map(node_bullet)
                .collect::<Vec<_>>()
                .join("\n");
            let scope = if total > orphans.len() {
                format!("Below are {} of {} orphan notes", orphans.len(), total)
            } else {
                format!("Below are the vault's {} orphan notes", orphans.len())
            };
            format!(
                "{scope} — notes with no incoming or outgoing id: links, so they are \
                 unreachable from the rest of the graph. For each, recommend one of: \
                 **merge** into another note (name which), **link** to/from specific notes \
                 (name which), **delete**, or **keep as-is** — with a one-sentence reason. \
                 Investigate before deciding: use the search_nodes, search_text, get_node, \
                 and get_backlinks tools to find related notes rather than guessing.\n\n\
                 ## Orphan notes\n{node_list}"
            )
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            user_text,
        )])
        .with_description("Triage orphan org-roam notes"))
    }

    #[prompt(
        name = "tag-suggestions",
        description = "Build a prompt asking Claude to suggest tags for a node, reusing the vault's existing tag vocabulary"
    )]
    async fn tag_suggestions(
        &self,
        p: Parameters<TagSuggestionsParams>,
    ) -> Result<GetPromptResult, McpError> {
        let index = self.get_index();
        let body = content::read_node_body(&index, &p.0.id).map_err(McpError::from)?;

        let vocab_limit = p.0.limit.unwrap_or(DEFAULT_TAG_VOCAB).min(MAX_TAG_VOCAB);
        let mut tags = index
            .tags()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Most-used tags first, then alphabetical, so the vocabulary shown
        // is stable and leads with the vault's established conventions.
        tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        tags.truncate(vocab_limit);

        let vocab = if tags.is_empty() {
            "(the vault has no tags yet)".to_string()
        } else {
            tags.iter()
                .map(|(tag, count)| format!("- {tag} ({count})"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let current = if body.node.tags.is_empty() {
            "(none)".to_string()
        } else {
            body.node.tags.join(", ")
        };
        let stale = body.stale_warning();
        let (excerpt, truncated) = truncate_chars(&body.body, MAX_SUMMARY_BODY_CHARS);
        let mut note = String::new();
        if truncated {
            note.push_str("\n\n[note: the note body above was truncated for length]");
        }
        if let Some(w) = stale {
            note.push_str("\n\n[note: ");
            note.push_str(w);
            note.push(']');
        }

        let user_text = format!(
            "Suggest tags for the org-roam note titled \"{}\". Prefer tags from the vault's \
             existing vocabulary (listed below, with usage counts) so tagging stays consistent; \
             propose a genuinely new tag only when nothing existing fits, and flag it as new \
             with a brief justification. Do not repeat tags the note already has.\n\n\
             ## Current tags\n{}\n\n## Existing tag vocabulary\n{}\n\n## Note body\n{}{}",
            body.node.title, current, vocab, excerpt, note
        );
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            user_text,
        )])
        .with_description("Suggest tags for an org-roam node"))
    }
}

// ── ServerHandler ─────────────────────────────────────────────────────────────

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for RoamServer {
    fn get_info(&self) -> ServerInfo {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .enable_resources_list_changed()
            .enable_prompts()
            .enable_completions()
            .build();
        ServerInfo::new(caps)
            .with_server_info(Implementation::new(
                "org-roam-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_instructions(
                "Query and extend an org-roam knowledge base. Read tools: search_nodes, \
                 list_nodes, list_orphans, search_text, get_node, get_node_by_path, \
                 get_node_section, get_backlinks, get_forward_links, find_by_ref, get_refs, \
                 list_tags, tag_cooccurrences, list_anchors, unlinked_references, \
                 list_node_tags, has_tag, search_by_tag, validate_node, get_daily_note, \
                 list_dailies, random_node, server_info, sync_database. \
                 Write tools (removed in --read-only): create_node, update_node, delete_node, \
                 rename_node, append_to_node, prepend_to_node, add_link, insert_anchor, \
                 daily_capture, add_tag, remove_tag, set_tags. \
                 Resources: org-roam://node/{id}. \
                 Prompts: summarize-node, link-suggestions, orphan-triage, tag-suggestions."
                    .to_string(),
            )
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        // The only argument with a discoverable value space is a prompt's
        // `id` (summarize-node, tag-suggestions): the vault's node ids.
        // Everything else (draft, limit, resource refs) yields nothing.
        let completion = match (&request.r#ref, request.argument.name.as_str()) {
            (Reference::Prompt(_), "id") => {
                let index = self.get_index();
                node_id_completions(&index, &request.argument.value)
            }
            _ => CompletionInfo::default(),
        };
        Ok(CompleteResult::new(completion))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        use rmcp::model::{Annotated, RawResource};
        let vault = Annotated::<RawResource> {
            raw: RawResource {
                uri: "org-roam://vault/".to_string(),
                name: "Vault index".to_string(),
                title: None,
                description: Some(
                    "JSON summary of this org-roam vault: node count, tag count, and roam_dir."
                        .to_string(),
                ),
                mime_type: Some("application/json".to_string()),
                size: None,
                icons: None,
                meta: None,
            },
            annotations: None,
        };
        Ok(ListResourcesResult {
            resources: vec![vault],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.clone();

        // org-roam://vault/ — lightweight vault summary.
        if uri == "org-roam://vault/" {
            let index = self.get_index();
            let node_count = index.node_count().unwrap_or(0);
            let tag_count = index.tags().map_or(0, |t| t.len());
            let summary = serde_json::json!({
                "roam_dir": self.config.roam_dir,
                "node_count": node_count,
                "tag_count": tag_count,
                "backend": index.source(),
            });
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                serde_json::to_string_pretty(&summary).unwrap_or_default(),
                uri,
            )]));
        }

        // org-roam://node/{id}[#anchor]
        let path = uri
            .strip_prefix("org-roam://node/")
            .ok_or_else(|| McpError::invalid_params(format!("unsupported uri: {uri}"), None))?;
        let (id, anchor) = match path.split_once('#') {
            Some((id, anc)) => (id, Some(anc.to_string())),
            None => (path, None),
        };
        let index = self.get_index();
        let body = content::read_node_body(&index, id).map_err(|e| match e {
            content::NodeBodyError::NotFound(_) => {
                McpError::resource_not_found(e.to_string(), None)
            }
            _ => McpError::internal_error(e.to_string(), None),
        })?;
        let text = if let Some(anc) = anchor {
            let doc = crate::org::OrgDoc::from_text(body.body);
            let section = crate::org::AnchorResolver::resolve(&doc, &anc).ok_or_else(|| {
                McpError::resource_not_found(format!("anchor not found: {anc}"), None)
            })?;
            section.text
        } else {
            body.body
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let template = Annotated::<RawResourceTemplate> {
            raw: RawResourceTemplate {
                uri_template: "org-roam://node/{id}".to_string(),
                name: "Org-roam node".to_string(),
                title: None,
                description: Some(
                    "Full body of an org-roam node. Add #anchor for a sub-section.".to_string(),
                ),
                mime_type: Some("text/org".to_string()),
                icons: None,
            },
            annotations: None,
        };
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![template],
            next_cursor: None,
            meta: None,
        })
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let uri = request.uri;
        tracing::debug!("subscribe: {uri} (session {})", self.session_id);
        self.subscriptions
            .lock()
            .expect("subscriptions lock")
            .entry(uri)
            .or_default()
            .insert(self.session_id, ctx.peer);
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        tracing::debug!("unsubscribe: {} (session {})", request.uri, self.session_id);
        let mut subs = self.subscriptions.lock().expect("subscriptions lock");
        if let Some(list) = subs.get_mut(&request.uri) {
            list.remove(&self.session_id);
            if list.is_empty() {
                subs.remove(&request.uri);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    fn scanner_config(dir: &std::path::Path) -> Config {
        Config::from_args(dir, true, true, None).expect("scanner config")
    }

    fn make_index_cell(dir: &std::path::Path) -> Arc<RwLock<Arc<dyn RoamIndex>>> {
        let idx = crate::index::scan::ScanIndex::open(dir).expect("scan index");
        Arc::new(RwLock::new(Arc::new(idx) as Arc<dyn RoamIndex>))
    }

    // --- maybe_reload_index ---

    #[test]
    fn reload_skipped_for_empty_paths() {
        let dir = TempDir::new().unwrap();
        let config = scanner_config(dir.path());
        let db_path = config.db_path();
        let index_cell = make_index_cell(dir.path());
        let before_ptr = Arc::as_ptr(&*index_cell.read().unwrap());
        maybe_reload_index(&[], &db_path, &config, &index_cell);
        let after_ptr = Arc::as_ptr(&*index_cell.read().unwrap());
        assert_eq!(before_ptr, after_ptr, "no reload for empty paths");
    }

    #[test]
    fn reload_triggered_by_org_path_in_scanner_mode() {
        let dir = TempDir::new().unwrap();
        let config = scanner_config(dir.path());
        let db_path = config.db_path();
        let index_cell = make_index_cell(dir.path());
        assert_eq!(index_cell.read().unwrap().node_count().unwrap(), 0);
        // Add an org file after building the initial (empty) index.
        let org_path = dir.path().join("new.org");
        std::fs::write(
            &org_path,
            ":PROPERTIES:\n:ID: deadbeef-dead-beef-dead-beefdeadbeef\n:END:\n#+title: New\n",
        )
        .unwrap();
        maybe_reload_index(&[org_path], &db_path, &config, &index_cell);
        assert_eq!(index_cell.read().unwrap().node_count().unwrap(), 1);
    }

    #[test]
    fn reload_skipped_for_non_org_path_in_scanner_mode() {
        let dir = TempDir::new().unwrap();
        let config = scanner_config(dir.path());
        let db_path = config.db_path();
        let index_cell = make_index_cell(dir.path());
        let txt_path = dir.path().join("readme.txt");
        std::fs::write(&txt_path, "not org").unwrap();
        maybe_reload_index(&[txt_path], &db_path, &config, &index_cell);
        assert_eq!(index_cell.read().unwrap().node_count().unwrap(), 0);
    }

    // --- events_to_paths ---

    #[test]
    fn events_to_paths_keeps_modify_and_dedupes() {
        let p1 = std::path::PathBuf::from("/tmp/a.org");
        let p2 = std::path::PathBuf::from("/tmp/b.org");
        let events: Vec<notify::Result<notify::Event>> = vec![
            Ok(notify::Event {
                kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
                paths: vec![p1.clone(), p2.clone()],
                attrs: notify::event::EventAttributes::default(),
            }),
            Ok(notify::Event {
                kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
                paths: vec![p1.clone()],
                attrs: notify::event::EventAttributes::default(),
            }),
            Ok(notify::Event {
                kind: notify::EventKind::Access(notify::event::AccessKind::Any),
                paths: vec![std::path::PathBuf::from("/tmp/ignored.org")],
                attrs: notify::event::EventAttributes::default(),
            }),
            Err(notify::Error::generic("watch error")),
        ];
        let mut result = events_to_paths(events);
        result.sort();
        assert_eq!(result, vec![p1, p2]);
    }

    #[test]
    fn events_to_paths_includes_create_and_remove() {
        let p_create = std::path::PathBuf::from("/tmp/new.org");
        let p_remove = std::path::PathBuf::from("/tmp/old.org");
        let events: Vec<notify::Result<notify::Event>> = vec![
            Ok(notify::Event {
                kind: notify::EventKind::Create(notify::event::CreateKind::Any),
                paths: vec![p_create.clone()],
                attrs: notify::event::EventAttributes::default(),
            }),
            Ok(notify::Event {
                kind: notify::EventKind::Remove(notify::event::RemoveKind::Any),
                paths: vec![p_remove.clone()],
                attrs: notify::event::EventAttributes::default(),
            }),
        ];
        let mut result = events_to_paths(events);
        result.sort();
        assert_eq!(result, vec![p_create, p_remove]);
    }

    // --- collect_to_notify ---

    #[test]
    fn collect_empty_subs_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let index_cell = make_index_cell(dir.path());
        let index = index_cell.read().unwrap().clone();
        let subs_map: HashMap<String, HashMap<u64, Peer<RoleServer>>> = HashMap::new();
        let result = collect_to_notify(dir.path(), &index, &subs_map);
        assert!(result.is_empty());
    }

    #[test]
    fn collect_non_roam_uri_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let index_cell = make_index_cell(dir.path());
        let index = index_cell.read().unwrap().clone();
        let mut subs_map: HashMap<String, HashMap<u64, Peer<RoleServer>>> = HashMap::new();
        subs_map.insert("other://node/xyz".to_string(), HashMap::new());
        let result = collect_to_notify(dir.path(), &index, &subs_map);
        assert!(result.is_empty());
    }

    #[test]
    fn collect_roam_uri_but_unknown_node_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let index_cell = make_index_cell(dir.path());
        let index = index_cell.read().unwrap().clone();
        let mut subs_map: HashMap<String, HashMap<u64, Peer<RoleServer>>> = HashMap::new();
        subs_map.insert("org-roam://node/no-such-id".to_string(), HashMap::new());
        let result = collect_to_notify(dir.path(), &index, &subs_map);
        assert!(result.is_empty());
    }
}
