//! Post-write `org-roam-db-sync` triggering via `emacsclient` or headless Emacs.
//!
//! After any successful write, call [`DbSyncer::schedule`]. Multiple rapid
//! writes within the debounce window are coalesced into a single sync.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// How the server triggers an `org-roam-db-sync` after a write.
#[derive(Debug, Clone, PartialEq, Default, clap::ValueEnum)]
pub enum SyncMode {
    /// Try `emacsclient` only; no-op if the daemon is unreachable.
    #[default]
    ClientOnly,
    /// Try `emacsclient`, fall back to headless `emacs --batch`.
    Full,
    /// Never attempt a sync; the user handles it manually.
    Never,
}

/// Runtime parameters for the db-sync subsystem.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub mode: SyncMode,
    /// Timeout for `emacsclient` / batch-Emacs commands.
    pub timeout: Duration,
    /// How long to wait after the last write before running the sync.
    pub debounce: Duration,
    /// Extra args forwarded to `emacsclient` (e.g. `--socket-name`).
    pub emacsclient_args: Vec<String>,
    /// Path to a custom `sync.el` for batch mode. Generated if `None`.
    pub batch_init: Option<PathBuf>,
    pub roam_dir: PathBuf,
    pub db_path: PathBuf,
}

/// Outcome of a single db-sync attempt.
#[derive(Debug, Clone)]
pub enum SyncOutcome {
    ViaClient,
    ViaBatch,
    Skipped(String),
}

impl std::fmt::Display for SyncOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ViaClient => f.write_str("synced via emacsclient"),
            Self::ViaBatch => f.write_str("synced via emacs --batch"),
            Self::Skipped(msg) => write!(f, "skipped — {msg}"),
        }
    }
}

impl SyncOutcome {
    /// Whether a real sync (not a skip) actually ran.
    #[must_use]
    pub fn synced(&self) -> bool {
        matches!(self, Self::ViaClient | Self::ViaBatch)
    }
}

/// Observable state of the sync subsystem, exposed to the `sync_database`
/// tool so MCP clients can see whether the db is settled.
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    /// When the last *successful* sync completed.
    pub last_sync: Option<DateTime<Utc>>,
    /// Human-readable outcome of the last sync attempt.
    pub last_outcome: Option<String>,
}

/// Manages debounced, serialized `org-roam-db-sync` calls after writes.
///
/// Internally keeps a generation counter: each [`schedule`](Self::schedule)
/// call bumps the counter and spawns a debounce task. Only the task whose
/// generation still matches when the debounce window expires actually runs the
/// sync, so rapid writes coalesce into one call. The `run_lock` prevents two
/// syncs from running concurrently.
pub struct DbSyncer {
    config: SyncConfig,
    generation: AtomicU64,
    /// The generation covered by the most recently completed run. Lets a
    /// forced sync coalesce with one that ran while it waited for the lock.
    completed: AtomicU64,
    run_lock: tokio::sync::Mutex<()>,
    state: Mutex<SyncState>,
}

impl DbSyncer {
    /// Create a new syncer wrapped in an `Arc`.
    #[must_use]
    pub fn new(config: SyncConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            generation: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            run_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(SyncState::default()),
        })
    }

    /// The configured sync mode.
    #[must_use]
    pub fn mode(&self) -> &SyncMode {
        &self.config.mode
    }

    /// A snapshot of the observable sync state (last sync, last outcome).
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    #[must_use]
    pub fn state(&self) -> SyncState {
        self.state
            .lock()
            .expect("sync state mutex poisoned")
            .clone()
    }

    /// Whether an `emacsclient` is currently reachable. Exposed so the
    /// `sync_database` tool can warn when `client-only` mode has no daemon
    /// to talk to.
    pub async fn emacsclient_reachable(&self) -> bool {
        self.emacsclient_alive().await
    }

    /// Schedule a debounced sync. Fire-and-forget; resets the debounce window
    /// so that rapid successive writes produce a single sync call.
    pub fn schedule(self: &Arc<Self>) {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(this.config.debounce).await;
            // Bail out if a later write has already superseded us.
            if this.generation.load(Ordering::SeqCst) != gen {
                return;
            }
            let _lock = this.run_lock.lock().await;
            // Re-check after acquiring the lock — another task may have snuck in.
            if this.generation.load(Ordering::SeqCst) != gen {
                return;
            }
            let outcome = this.run_and_record().await;
            tracing::info!("org-roam db sync: {outcome}");
        });
    }

    /// Run a sync *now*, bypassing the debounce window. Serializes against
    /// the debounced path and other forced syncs via the run lock. If a
    /// sync that already accounts for this request completed while this
    /// call waited for the lock, it coalesces instead of syncing twice.
    pub async fn sync_now(&self) -> SyncOutcome {
        let req = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _lock = self.run_lock.lock().await;
        if self.completed.load(Ordering::SeqCst) >= req {
            return SyncOutcome::Skipped("coalesced with concurrent sync".into());
        }
        self.run_and_record().await
    }

    /// Run a sync and record its outcome (and, on success, the timestamp)
    /// in the observable state. The caller must hold `run_lock`.
    async fn run_and_record(&self) -> SyncOutcome {
        let outcome = self.run().await;
        {
            let mut st = self.state.lock().expect("sync state mutex poisoned");
            st.last_outcome = Some(outcome.to_string());
            if outcome.synced() {
                st.last_sync = Some(Utc::now());
            }
        }
        // Mark every request up to now as covered by this run.
        self.completed
            .store(self.generation.load(Ordering::SeqCst), Ordering::SeqCst);
        outcome
    }

    async fn run(&self) -> SyncOutcome {
        if self.config.mode == SyncMode::Never {
            return SyncOutcome::Skipped("sync disabled".into());
        }
        if self.emacsclient_alive().await {
            match self.run_emacsclient_sync().await {
                Ok(o) => return o,
                Err(e) => tracing::debug!("emacsclient sync failed: {e}"),
            }
        }
        if self.config.mode == SyncMode::Full {
            match self.run_batch_sync().await {
                Ok(o) => return o,
                Err(e) => tracing::debug!("batch emacs sync failed: {e}"),
            }
        }
        SyncOutcome::Skipped("emacs unreachable — run M-x org-roam-db-sync".into())
    }

    async fn emacsclient_alive(&self) -> bool {
        let mut cmd = tokio::process::Command::new("emacsclient");
        for arg in &self.config.emacsclient_args {
            cmd.arg(arg);
        }
        cmd.arg("--eval").arg("t").kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(1), cmd.output())
            .await
            .is_ok_and(|r| r.is_ok_and(|o| o.status.success()))
    }

    async fn run_emacsclient_sync(&self) -> Result<SyncOutcome, String> {
        let mut cmd = tokio::process::Command::new("emacsclient");
        for arg in &self.config.emacsclient_args {
            cmd.arg(arg);
        }
        // `(when ...)` returns nil when org-roam isn't loaded — treat as failure.
        cmd.arg("--eval")
            .arg("(when (featurep 'org-roam) (org-roam-db-sync))")
            .kill_on_drop(true);
        let out = tokio::time::timeout(self.config.timeout, cmd.output())
            .await
            .map_err(|_| "timeout".to_string())?
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("exit {:?}", out.status.code()));
        }
        if String::from_utf8_lossy(&out.stdout).trim() == "nil" {
            return Err("org-roam not loaded in Emacs session".into());
        }
        Ok(SyncOutcome::ViaClient)
    }

    async fn run_batch_sync(&self) -> Result<SyncOutcome, String> {
        let init = match &self.config.batch_init {
            Some(p) => p.clone(),
            None => self.write_sync_el().map_err(|e| e.to_string())?,
        };
        let out = tokio::time::timeout(
            self.config.timeout,
            tokio::process::Command::new("emacs")
                .arg("--batch")
                .arg("-Q")
                .arg("-l")
                .arg(&init)
                .arg("--eval")
                .arg("(org-roam-db-sync)")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(SyncOutcome::ViaBatch)
        } else {
            Err(format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn write_sync_el(&self) -> std::io::Result<PathBuf> {
        let dir = std::env::temp_dir().join("org-roam-mcp");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("sync.el");
        let roam_dir = elisp_string(&self.config.roam_dir.display().to_string());
        let db_path = elisp_string(&self.config.db_path.display().to_string());
        std::fs::write(
            &path,
            format!(
                "(package-initialize)\n\
                 (require 'org-roam)\n\
                 (setq org-roam-directory {roam_dir}\n\
                       org-roam-db-location {db_path})\n"
            ),
        )?;
        Ok(path)
    }
}

/// Render `s` as a quoted elisp string literal (escaping `\` and `"`),
/// so a path containing either can't break the generated `sync.el`.
fn elisp_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_config(mode: SyncMode) -> SyncConfig {
        SyncConfig {
            mode,
            timeout: Duration::from_millis(500),
            debounce: Duration::from_millis(10),
            emacsclient_args: vec![
                "--socket-name".to_string(),
                "nonexistent-CRAPTEST-socket".to_string(),
            ],
            batch_init: Some(PathBuf::from("/nonexistent/craptest-sync.el")),
            roam_dir: std::env::temp_dir(),
            db_path: std::env::temp_dir().join("craptest.db"),
        }
    }

    #[tokio::test]
    async fn run_never_mode_skips() {
        let syncer = DbSyncer::new(test_config(SyncMode::Never));
        let outcome = syncer.run().await;
        assert!(matches!(outcome, SyncOutcome::Skipped(_)));
        assert!(outcome.to_string().contains("sync disabled"));
        assert!(!outcome.synced());
    }

    #[tokio::test]
    async fn run_client_only_without_daemon_skips() {
        let syncer = DbSyncer::new(test_config(SyncMode::ClientOnly));
        let outcome = syncer.run().await;
        assert!(matches!(outcome, SyncOutcome::Skipped(_)));
        assert!(outcome.to_string().contains("emacs unreachable"));
    }

    #[tokio::test]
    async fn run_full_without_emacs_skips() {
        let syncer = DbSyncer::new(test_config(SyncMode::Full));
        let outcome = syncer.run().await;
        assert!(matches!(outcome, SyncOutcome::Skipped(_)));
    }

    #[tokio::test]
    async fn emacsclient_sync_errs_without_daemon() {
        let syncer = DbSyncer::new(test_config(SyncMode::ClientOnly));
        let result = syncer.run_emacsclient_sync().await;
        assert!(
            result.is_err(),
            "expected error without daemon, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn batch_sync_errs_with_missing_init() {
        let syncer = DbSyncer::new(test_config(SyncMode::Full));
        let result = syncer.run_batch_sync().await;
        assert!(
            result.is_err(),
            "expected error with missing init, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn sync_now_never_mode_skips() {
        let syncer = DbSyncer::new(test_config(SyncMode::Never));
        let outcome = syncer.sync_now().await;
        assert!(matches!(outcome, SyncOutcome::Skipped(_)));
    }

    #[test]
    fn outcome_display_and_synced() {
        assert_eq!(SyncOutcome::ViaClient.to_string(), "synced via emacsclient");
        assert_eq!(
            SyncOutcome::ViaBatch.to_string(),
            "synced via emacs --batch"
        );
        assert!(SyncOutcome::ViaClient.synced());
        assert!(SyncOutcome::ViaBatch.synced());
        assert!(!SyncOutcome::Skipped("x".into()).synced());
    }
}
