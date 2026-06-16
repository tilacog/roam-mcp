//! Runtime configuration for the MCP server.
//!
//! Built from command-line arguments in `main.rs`. Kept here so the same
//! configuration can be constructed in tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::sync::{SyncConfig, SyncMode};

/// Transport to use for MCP communication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Standard input/output — what Claude Desktop / Claude Code expect.
    Stdio,
    /// Streamable HTTP server at the given address (e.g. `127.0.0.1:8080`).
    Http(String),
}

/// Server configuration.
///
/// `RoamServer::new` takes a `&Config` and uses it to pick an index backend
/// (`SQLite` or scanner) and to enforce the write policy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory of the org-roam vault.
    pub roam_dir: PathBuf,

    /// Override the location of `org-roam.db`. Default: `<roam_dir>/org-roam.db`.
    pub db_path: Option<PathBuf>,

    /// If true, force the filesystem scanner backend even if `org-roam.db` exists.
    pub no_db: bool,

    /// If true, no write tools are registered. The whole server becomes read-only.
    pub read_only: bool,

    /// Transport for MCP communication.
    pub transport: Transport,

    // -- dailies --
    /// Subdirectory (relative to `roam_dir`) where `daily_capture` puts
    /// daily notes. `None` keeps them in the vault root.
    pub dailies_dir: Option<PathBuf>,

    /// strftime pattern for the daily-note file stem (without `.org`).
    /// Default `%Y%m%d`. Use `%Y-%m-%d` together with `--dailies-dir
    /// daily` to match org-roam-dailies' default layout.
    pub dailies_format: String,

    // -- db-sync --
    /// When and how to trigger `org-roam-db-sync` after a write.
    pub sync_mode: SyncMode,

    /// Timeout for `emacsclient` / `emacs --batch` sync commands (seconds).
    pub sync_timeout_s: u64,

    /// Debounce window: multiple writes within this many ms produce one sync.
    pub sync_debounce_ms: u64,

    /// Extra arguments forwarded verbatim to `emacsclient`.
    pub sync_emacsclient_args: Vec<String>,

    /// Custom `sync.el` path for batch mode. A minimal one is generated if `None`.
    pub sync_batch_init: Option<PathBuf>,
}

impl Config {
    /// Construct a config from raw args. Performs path validation but does not
    /// touch the index — `RoamServer::new` does that.
    ///
    /// # Errors
    ///
    /// Returns an error if `roam_dir` does not exist or is not a directory, or
    /// if the path cannot be canonicalized.
    pub fn from_args(
        roam_dir: &Path,
        read_only: bool,
        no_db: bool,
        http: Option<String>,
    ) -> Result<Self> {
        if !roam_dir.exists() {
            bail!("roam directory does not exist: {}", roam_dir.display());
        }
        if !roam_dir.is_dir() {
            bail!("roam path is not a directory: {}", roam_dir.display());
        }
        Ok(Self {
            roam_dir: roam_dir.canonicalize().context("canonicalizing roam dir")?,
            db_path: None,
            no_db,
            read_only,
            transport: match http {
                Some(addr) => Transport::Http(addr),
                None => Transport::Stdio,
            },
            dailies_dir: None,
            dailies_format: "%Y%m%d".to_string(),
            sync_mode: SyncMode::default(),
            sync_timeout_s: 30,
            sync_debounce_ms: 2000,
            sync_emacsclient_args: vec![],
            sync_batch_init: None,
        })
    }

    /// Resolve the path to `org-roam.db`, honoring `db_path` if set.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.roam_dir.join("org-roam.db"))
    }

    /// True if the `org-roam.db` file exists at the resolved path.
    #[must_use]
    pub fn has_db(&self) -> bool {
        !self.no_db && self.db_path().exists()
    }

    /// True if write operations are permitted.
    #[must_use]
    pub fn can_write(&self) -> bool {
        !self.read_only
    }

    /// Build the [`SyncConfig`] for the db-sync subsystem.
    #[must_use]
    pub fn sync_config(&self) -> SyncConfig {
        SyncConfig {
            mode: self.sync_mode.clone(),
            timeout: Duration::from_secs(self.sync_timeout_s),
            debounce: Duration::from_millis(self.sync_debounce_ms),
            emacsclient_args: self.sync_emacsclient_args.clone(),
            batch_init: self.sync_batch_init.clone(),
            roam_dir: self.roam_dir.clone(),
            db_path: self.db_path(),
        }
    }
}
