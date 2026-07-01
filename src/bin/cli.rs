//! `org-roam-cli` — a command-line companion for testing and exploring an
//! org-roam vault without needing a running MCP client.
//!
//! All subcommands call the same library code that the MCP server uses, so
//! the output is identical to what Claude would receive.
//!
//! ```
//! org-roam-cli --roam-dir ~/org search "zettelkasten"
//! org-roam-cli --roam-dir ~/org node <id>
//! org-roam-cli --roam-dir ~/org tasks --state TODO
//! org-roam-cli --roam-dir ~/org outline <id>
//! org-roam-cli --roam-dir ~/org files
//! org-roam-cli --roam-dir ~/org tags
//! org-roam-cli --roam-dir ~/org backlinks <id>
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

use org_roam_mcp::index::{self, LinkRecord, RoamIndex};
use org_roam_mcp::tools::{content, populate, query};
use org_roam_mcp::Config;

/// Extract all text content from a `CallToolResult` and print it.
fn print_result(r: &CallToolResult) {
    for c in &r.content {
        if let Some(t) = c.as_text() {
            println!("{}", t.text);
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "org-roam-cli",
    about = "Command-line tool for exploring an org-roam vault",
    version
)]
struct Cli {
    /// Path to the org-roam vault directory.
    #[arg(short = 'd', long = "roam-dir")]
    roam_dir: PathBuf,

    /// Force the filesystem scanner; skip org-roam.db.
    #[arg(long = "no-db", default_value_t = false)]
    no_db: bool,

    /// Override the location of org-roam.db.
    #[arg(long = "db-path")]
    db_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Liveness check: confirm the index can be opened.
    Ping,

    /// Show vault statistics (node count, tag count, backend).
    Info,

    /// Search nodes by title, alias, or tag (fuzzy).
    Search {
        /// Query string.
        query: String,
        /// Maximum results.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },

    /// Get a node's metadata and body by :ID:.
    Node {
        /// Node ID.
        id: String,
    },

    /// List nodes with a TODO keyword.
    Tasks {
        /// Filter to these TODO states (repeatable).
        #[arg(long = "state", short = 's')]
        states: Vec<String>,
        /// Filter by priority letter (A, B, C).
        #[arg(long = "priority", short = 'p')]
        priority: Option<String>,
        /// Maximum results.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Sort: title (default), `title_desc`, priority.
        #[arg(long, default_value = "title")]
        sort: String,
    },

    /// Show the heading outline of the file that contains a node.
    Outline {
        /// Node ID.
        id: String,
    },

    /// List all .org files in the vault.
    Files {
        /// Maximum results.
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: usize,
    },

    /// List all tags and their node counts.
    Tags,

    /// Show backlinks for a node.
    Backlinks {
        /// Node ID.
        id: String,
    },

    /// Show outgoing links from a node.
    Forward {
        /// Node ID.
        id: String,
    },

    /// Create org-roam.db from the .org files without Emacs.
    CreateDb {
        /// Path to write the database to (default: configured db path).
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Overwrite an existing database.
        #[arg(long, default_value_t = false)]
        overwrite: bool,
    },
}

fn open_index(cli: &Cli) -> Result<Arc<dyn RoamIndex>> {
    let mut config =
        Config::from_args(&cli.roam_dir, true, cli.no_db, None).context("building config")?;
    if let Some(p) = cli.db_path.clone() {
        config.db_path = Some(p);
    }
    index::open(&config).map_err(|e| anyhow::anyhow!("opening index: {e}"))
}

fn print_json(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn cmd_ping(cli: &Cli) -> Result<()> {
    let idx = open_index(cli)?;
    let count = idx.node_count().context("node_count")?;
    println!("pong — {count} nodes");
    Ok(())
}

fn cmd_info(cli: &Cli, config: &Config) -> Result<()> {
    let idx = open_index(cli)?;
    let node_count = idx.node_count().unwrap_or(0);
    let tags = idx.tags().unwrap_or_default();
    print_json(&serde_json::json!({
        "roam_dir": config.roam_dir,
        "backend": idx.source(),
        "node_count": node_count,
        "tag_count": tags.len(),
    }));
    Ok(())
}

fn cmd_search(cli: &Cli, q: &str, limit: usize) -> Result<()> {
    let idx = open_index(cli)?;
    let p = Parameters(query::SearchParams {
        query: Some(q.to_owned()),
        tags: vec![],
        limit: Some(limit),
    });
    print_result(&query::search_nodes(&idx, p)?);
    Ok(())
}

fn cmd_node(cli: &Cli, id: &str) -> Result<()> {
    let idx = open_index(cli)?;
    let body =
        content::read_node_body(&idx, id).map_err(|e| anyhow::anyhow!("read_node_body: {e}"))?;
    let mut out = serde_json::to_value(&body.node)?;
    out["body"] = body.body.into();
    print_json(&out);
    Ok(())
}

fn cmd_tasks(
    cli: &Cli,
    states: &[String],
    priority: Option<&str>,
    limit: usize,
    sort: &str,
) -> Result<()> {
    let idx = open_index(cli)?;
    let p = Parameters(query::ListTasksParams {
        todo_states: states.to_vec(),
        priority: priority.map(str::to_owned),
        tags: vec![],
        limit: Some(limit),
        offset: None,
        sort: Some(sort.to_owned()),
    });
    print_result(&query::list_tasks(&idx, p)?);
    Ok(())
}

fn cmd_outline(cli: &Cli, id: &str) -> Result<()> {
    let idx = open_index(cli)?;
    let p = Parameters(query::GetOutlineParams { id: id.to_owned() });
    print_result(&query::get_outline(&idx, &p)?);
    Ok(())
}

fn cmd_files(cli: &Cli, config: &Config, limit: usize) -> Result<()> {
    let idx = open_index(cli)?;
    let p = Parameters(query::ListFilesParams {
        limit: Some(limit),
        offset: None,
    });
    print_result(&query::list_files(&idx, &config.roam_dir, p)?);
    Ok(())
}

fn cmd_tags(cli: &Cli) -> Result<()> {
    let idx = open_index(cli)?;
    print_json(&idx.tags().context("listing tags")?);
    Ok(())
}

fn backlink_entry(idx: &Arc<dyn RoamIndex>, l: &LinkRecord) -> Result<serde_json::Value> {
    let mut entry = serde_json::json!({ "link": l });
    if let Ok(Some(meta)) = idx.node(&l.source) {
        entry["node"] = serde_json::to_value(meta)?;
    }
    Ok(entry)
}

fn cmd_backlinks(cli: &Cli, id: &str) -> Result<()> {
    let idx = open_index(cli)?;
    let links = idx.backlinks(id).context("backlinks")?;
    let mut out = Vec::new();
    for l in &links {
        out.push(backlink_entry(&idx, l)?);
    }
    print_json(&out);
    Ok(())
}

fn forward_entry(idx: &Arc<dyn RoamIndex>, l: &LinkRecord) -> Result<serde_json::Value> {
    let mut entry = serde_json::json!({ "link": l });
    if let Some(dest) = &l.dest {
        if let Ok(Some(meta)) = idx.node(dest) {
            entry["node"] = serde_json::to_value(meta)?;
        }
    }
    Ok(entry)
}

fn cmd_forward(cli: &Cli, id: &str) -> Result<()> {
    let idx = open_index(cli)?;
    let links = idx.forward_links(id).context("forward_links")?;
    let mut out = Vec::new();
    for l in &links {
        out.push(forward_entry(&idx, l)?);
    }
    print_json(&out);
    Ok(())
}

fn cmd_create_db(cli: &Cli, db_path: Option<&PathBuf>, overwrite: bool) -> Result<()> {
    let config = build_cli_config(cli)?;
    let target = db_path.cloned().unwrap_or_else(|| config.db_path());
    let params = populate::CreateDatabaseParams {
        db_path: Some(target.display().to_string()),
        overwrite,
        validate: true,
    };
    let report = populate::create_database(&config, params)
        .map_err(|e| anyhow::anyhow!("create_database: {e}"))?;
    print_json(&report);
    Ok(())
}

fn build_cli_config(cli: &Cli) -> Result<Config> {
    let mut c =
        Config::from_args(&cli.roam_dir, true, cli.no_db, None).context("building config")?;
    if let Some(p) = cli.db_path.clone() {
        c.db_path = Some(p);
    }
    Ok(c)
}

fn try_dispatch_a(cli: &Cli, config: &Config) -> Option<Result<()>> {
    Some(match &cli.command {
        Cmd::Ping => cmd_ping(cli),
        Cmd::Info => cmd_info(cli, config),
        Cmd::Search { query: q, limit } => cmd_search(cli, q, *limit),
        _ => return None,
    })
}

fn try_dispatch_b(cli: &Cli) -> Option<Result<()>> {
    Some(match &cli.command {
        Cmd::Node { id } => cmd_node(cli, id),
        Cmd::Tags => cmd_tags(cli),
        Cmd::Tasks {
            states,
            priority,
            limit,
            sort,
        } => cmd_tasks(cli, states, priority.as_deref(), *limit, sort),
        _ => return None,
    })
}

fn try_dispatch_c(cli: &Cli, config: &Config) -> Option<Result<()>> {
    Some(match &cli.command {
        Cmd::Outline { id } => cmd_outline(cli, id),
        Cmd::Files { limit } => cmd_files(cli, config, *limit),
        Cmd::Backlinks { id } => cmd_backlinks(cli, id),
        _ => return None,
    })
}

fn try_dispatch_d(cli: &Cli) -> Option<Result<()>> {
    Some(match &cli.command {
        Cmd::Forward { id } => cmd_forward(cli, id),
        Cmd::CreateDb { db_path, overwrite } => cmd_create_db(cli, db_path.as_ref(), *overwrite),
        _ => return None,
    })
}

fn dispatch(cli: &Cli, config: &Config) -> Result<()> {
    if let Some(r) = try_dispatch_a(cli, config) {
        return r;
    }
    if let Some(r) = try_dispatch_b(cli) {
        return r;
    }
    if let Some(r) = try_dispatch_c(cli, config) {
        return r;
    }
    try_dispatch_d(cli).unwrap_or_else(|| unreachable!("unhandled Cmd variant"))
}

fn run(cli: &Cli) -> Result<()> {
    let config = build_cli_config(cli)?;
    dispatch(cli, &config)
}

fn main() -> Result<()> {
    run(&Cli::parse())
}
