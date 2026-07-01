//! Emacs integration test for the native DB populator.
//!
//! Verifies that a database built by `populate_database` can be read
//! directly by Emacs org-roam, without Emacs rebuilding it. This test is
//! gated by the `emacs-tests` feature and auto-skips when Emacs or org-roam
//! are not installed.

#![cfg(feature = "emacs-tests")]

use std::fs;
use std::path::PathBuf;

use org_roam_mcp::index::populate::{populate_database, PopulateOptions};
use sha1::Digest;

mod emacs_helper;

fn populate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-vault")
}

fn file_hash(path: &std::path::Path) -> String {
    use std::io::Read;
    let mut file = fs::File::open(path).expect("open db for hashing");
    let mut hasher = sha1::Sha1::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).expect("read db for hashing");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
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

    // Remember the exact DB bytes. Emacs must not modify the DB while
    // reading it; if it syncs/rebuilds, the hash will change and the test
    // fails.
    let hash_before = file_hash(&db_path);

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

    let hash_after = file_hash(&db_path);
    assert_eq!(
        hash_before, hash_after,
        "Emacs modified the database; org-roam-db-sync should not have run"
    );

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
