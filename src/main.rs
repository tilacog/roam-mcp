//! CLI entry point for the org-roam MCP server.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use org_roam_mcp::config::Transport;
use org_roam_mcp::sync::SyncMode;
use org_roam_mcp::{Config, RoamServer};

#[derive(Parser, Debug)]
#[command(
    name = "org-roam-mcp",
    about = "MCP server for org-roam knowledge bases",
    version
)]
struct Cli {
    /// Path to the org-roam directory (required).
    #[arg(short = 'd', long = "roam-dir")]
    roam_dir: PathBuf,

    /// Disable all write tools. Read tools are still available.
    #[arg(short = 'r', long = "read-only", default_value_t = false)]
    read_only: bool,

    /// Force the filesystem-scanner index backend (skip org-roam.db).
    #[arg(long = "no-db", default_value_t = false)]
    no_db: bool,

    /// Override the location of org-roam.db.
    #[arg(long = "db-path")]
    db_path: Option<PathBuf>,

    /// Serve over streamable HTTP at the given address (e.g. 127.0.0.1:8080).
    /// Default: stdio.
    #[arg(long = "http")]
    http: Option<String>,

    /// Verbosity for stderr logging. Use `RUST_LOG` to override.
    #[arg(long = "verbose", short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Subdirectory (relative to the roam dir) for daily notes created by
    /// `daily_capture`. Default: the roam dir itself.
    #[arg(long = "dailies-dir")]
    dailies_dir: Option<PathBuf>,

    /// strftime pattern for daily-note filenames (without `.org`).
    /// Use `%Y-%m-%d` with `--dailies-dir daily` to match org-roam-dailies.
    #[arg(long = "dailies-format", default_value = "%Y%m%d")]
    dailies_format: String,

    /// When to trigger `org-roam-db-sync` after a write.
    #[arg(long = "sync-mode", default_value = "client-only")]
    sync_mode: SyncMode,

    /// Timeout for sync commands in seconds.
    #[arg(long = "sync-timeout", default_value_t = 30)]
    sync_timeout: u64,

    /// Debounce window in milliseconds: multiple writes within this window produce one sync.
    #[arg(long = "sync-debounce", default_value_t = 2000)]
    sync_debounce_ms: u64,

    /// Extra argument forwarded to `emacsclient` (repeatable, e.g. `--socket-name foo`).
    #[arg(long = "emacsclient-arg")]
    emacsclient_args: Vec<String>,

    /// Path to a custom `sync.el` for `--sync-mode full` batch fallback.
    #[arg(long = "sync-init")]
    sync_init: Option<PathBuf>,
}

fn build_config(cli: &Cli) -> Result<Config> {
    let mut config = Config::from_args(&cli.roam_dir, cli.read_only, cli.no_db, cli.http.clone())
        .context("building config")?;
    if let Some(p) = cli.db_path.clone() {
        config.db_path = Some(p);
    }
    config.dailies_dir.clone_from(&cli.dailies_dir);
    config.dailies_format.clone_from(&cli.dailies_format);
    config.sync_mode = cli.sync_mode.clone();
    config.sync_timeout_s = cli.sync_timeout;
    config.sync_debounce_ms = cli.sync_debounce_ms;
    config
        .sync_emacsclient_args
        .clone_from(&cli.emacsclient_args);
    config.sync_batch_init.clone_from(&cli.sync_init);
    Ok(config)
}

fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("org_roam_mcp={default_level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

async fn serve_stdio(server: RoamServer) -> Result<()> {
    use rmcp::ServiceExt;
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("serving on stdio: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("stdio: {e}"))?;
    Ok(())
}

async fn serve_http(server: RoamServer, addr: &str) -> Result<()> {
    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use std::sync::Arc;
    // One server (one index, one file watcher) shared by all
    // sessions; each session gets its own subscription identity.
    let service = Arc::new(StreamableHttpService::new(
        move || Ok(server.for_new_session()),
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default(),
    ));
    let app = axum::Router::new().fallback_service(tower::service_fn(
        move |req: axum::http::Request<axum::body::Body>| {
            let svc = service.clone();
            async move { Ok::<_, std::convert::Infallible>(svc.handle(req).await) }
        },
    ));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let config = build_config(&cli).context("building config")?;
    tracing::info!(
        "starting org-roam-mcp; roam_dir={} read_only={} transport={:?}",
        config.roam_dir.display(),
        config.read_only,
        config.transport,
    );
    let server =
        RoamServer::new(config.clone()).map_err(|e| anyhow::anyhow!("initializing server: {e}"))?;
    match &config.transport {
        Transport::Stdio => serve_stdio(server).await,
        Transport::Http(addr) => serve_http(server, addr).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn minimal_cli(dir: &std::path::Path) -> Cli {
        Cli {
            roam_dir: dir.to_path_buf(),
            read_only: false,
            no_db: true,
            db_path: None,
            http: None,
            verbose: 0,
            dailies_dir: None,
            dailies_format: "%Y%m%d".to_string(),
            sync_mode: SyncMode::Never,
            sync_timeout: 5,
            sync_debounce_ms: 100,
            emacsclient_args: vec![],
            sync_init: None,
        }
    }

    #[test]
    fn build_config_works_with_valid_dir() {
        let dir = TempDir::new().unwrap();
        let cli = minimal_cli(dir.path());
        let config = build_config(&cli).unwrap();
        assert!(!config.has_db());
        assert!(!config.read_only);
    }

    #[test]
    fn build_config_propagates_db_path() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("custom.db");
        let mut cli = minimal_cli(dir.path());
        cli.db_path = Some(db.clone());
        let config = build_config(&cli).unwrap();
        assert_eq!(config.db_path, Some(db));
    }

    #[test]
    fn build_config_fails_on_nonexistent_dir() {
        let cli = minimal_cli(std::path::Path::new(
            "/tmp/this-dir-does-not-exist-craptest",
        ));
        let result = build_config(&cli);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn serve_http_fails_with_invalid_address() {
        let dir = TempDir::new().unwrap();
        let cfg = org_roam_mcp::Config::from_args(dir.path(), true, true, None).unwrap();
        let server = org_roam_mcp::RoamServer::new(cfg).unwrap();
        let result = serve_http(server, "this-is-not-a-valid-addr:0").await;
        assert!(result.is_err(), "bind to invalid addr must fail");
    }
}
