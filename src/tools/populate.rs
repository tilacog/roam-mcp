//! The `create_database` MCP tool: build `org-roam.db` from `.org` files
//! without needing a running Emacs.
//!
//! This is the tool-level wrapper around [`crate::index::populate`]. It
//! validates parameters, calls the native populator, and returns a report
//! that the MCP client can consume.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::populate::{populate_database, PopulateOptions, PopulateStats};
use crate::index::RoamIndex;

/// Parameters for `create_database`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateDatabaseParams {
    /// Path to write the database to. Defaults to the configured
    /// `org-roam.db` location (`<roam_dir>/org-roam.db` or the value of
    /// `--db-path`).
    pub db_path: Option<String>,

    /// If true, replace an existing database. If false (default) and the
    /// database already exists, the call fails without touching the file.
    #[serde(default)]
    pub overwrite: bool,

    /// If true (default), open the newly created database with the `SQLite`
    /// backend and report its node count as a sanity check.
    #[serde(default = "default_validate")]
    pub validate: bool,
}

fn default_validate() -> bool {
    true
}

/// Result of `create_database`.
#[derive(Debug, Clone, Serialize)]
pub struct CreateDatabaseReport {
    pub ok: bool,
    pub db_path: String,
    pub created: bool,
    pub stats: PopulateStats,
    pub validated_node_count: Option<usize>,
    pub warnings: Vec<String>,
}

/// Run the native populator for the `create_database` tool.
///
/// # Errors
///
/// Returns an error string if parameters are invalid or the populator fails.
pub fn create_database(
    config: &Config,
    p: CreateDatabaseParams,
) -> Result<CreateDatabaseReport, String> {
    let db_path = p.db_path.map_or_else(|| config.db_path(), PathBuf::from);

    let options = PopulateOptions {
        db_path: db_path.clone(),
        overwrite: p.overwrite,
    };

    let stats = populate_database(&config.roam_dir, &options)
        .map_err(|e| format!("failed to create database: {e}"))?;

    let (validated_node_count, warnings) = validate_created_db(&db_path, p.validate);

    Ok(CreateDatabaseReport {
        ok: true,
        db_path: db_path.display().to_string(),
        created: true,
        stats,
        validated_node_count,
        warnings,
    })
}

fn validate_created_db(db_path: &std::path::Path, validate: bool) -> (Option<usize>, Vec<String>) {
    if !validate {
        return (None, Vec::new());
    }
    let mut warnings = Vec::new();
    let count = match crate::index::sqlite::SqliteIndex::open(db_path) {
        Ok(idx) => match idx.node_count() {
            Ok(n) => Some(n),
            Err(e) => {
                warnings.push(format!("validation node_count failed: {e}"));
                None
            }
        },
        Err(e) => {
            warnings.push(format!("validation open failed: {e}"));
            None
        }
    };
    (count, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config(dir: &std::path::Path) -> Config {
        Config::from_args(dir, false, false, None).expect("valid test config")
    }

    #[test]
    fn create_database_tool_populates_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(
            vault.join("note.org"),
            ":PROPERTIES:\n:ID: aaaa1111-1111-1111-1111-111111111111\n:END:\n#+title: Note\n",
        )
        .unwrap();

        let config = sample_config(&vault);
        let report = create_database(
            &config,
            CreateDatabaseParams {
                db_path: None,
                overwrite: false,
                validate: true,
            },
        )
        .expect("create_database should succeed");

        assert!(report.ok);
        assert!(report.created);
        assert_eq!(report.stats.files, 1);
        assert_eq!(report.stats.nodes, 1);
        assert_eq!(report.validated_node_count, Some(1));
        assert!(report.warnings.is_empty());
    }
}
