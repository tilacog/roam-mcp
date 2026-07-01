//! Emacs integration test for the native DB populator.
//!
//! Verifies that a database built by `populate_database` can be synced and read
//! by Emacs org-roam. This test is gated by the `emacs-tests` feature and
//! auto-skips when Emacs or org-roam are not installed.

#![cfg(feature = "emacs-tests")]

use std::path::PathBuf;

use org_roam_mcp::index::populate::{populate_database, PopulateOptions};

mod emacs_helper;

fn populate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-vault")
}

#[test]
fn emacs_reads_populated_db() {
    let Some(_bin) = emacs_helper::usable_emacs() else {
        eprintln!("skip: Emacs with org-roam not available");
        return;
    };

    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("org-roam.db");
    let vault = populate_dir();

    let stats = populate_database(
        &vault,
        &PopulateOptions {
            db_path: db_path.clone(),
            overwrite: false,
        },
    )
    .expect("populate should succeed");
    assert!(
        stats.nodes > 0,
        "expected at least one node in populated DB"
    );

    // Pick a known node from the sample vault to verify in Emacs.
    let target_id = "11111111-1111-1111-1111-111111111111";

    let output = emacs_helper::run_emacs_script(
        &vault,
        &db_path,
        target_id,
        &emacs_helper::elisp_script("verify_populated_db.el"),
    );
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!("Emacs verification failed:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }

    let line = emacs_helper::last_stdout_line(&output);
    let value: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("Emacs did not print valid JSON ('{line}'): {e}"));

    assert_eq!(
        value.get("found").and_then(serde_json::Value::as_bool),
        Some(true),
        "org-roam should find the target node; got: {value}"
    );
    assert_eq!(
        value.get("id").and_then(serde_json::Value::as_str),
        Some(target_id),
        "org-roam returned the wrong node id; got: {value}"
    );
    assert_eq!(
        value.get("title").and_then(serde_json::Value::as_str),
        Some("Pastafarian Canticle"),
        "org-roam returned the wrong title; got: {value}"
    );
}
