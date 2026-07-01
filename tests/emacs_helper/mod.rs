//! Helpers for Emacs integration tests behind the `emacs-tests` feature.
//!
//! These tests verify that org-roam inside Emacs can read files and databases
//! produced by the Rust server. They are gated by a Cargo feature because
//! Emacs and the org-roam package are not required dependencies of the crate.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Directory used by Emacs for packages, if the `ORG_ROAM_MCP_EMACS_USER_DIR`
/// environment variable is set. This lets CI install org-roam into an isolated
/// directory and have the test probe discover it without touching the real
/// `~/.emacs.d`.
fn emacs_user_dir() -> Option<String> {
    std::env::var("ORG_ROAM_MCP_EMACS_USER_DIR").ok()
}

/// Build the `--eval` forms that point Emacs at the right package directory.
/// Returns an empty iterator when no override is configured.
fn package_dir_args() -> Vec<String> {
    let Some(dir) = emacs_user_dir() else {
        return Vec::new();
    };
    let dir_escaped = elisp_string(&dir);
    vec![
        format!("(setq user-emacs-directory (file-name-as-directory {dir_escaped}))"),
        "(setq package-user-dir (expand-file-name \"elpa\" user-emacs-directory))".to_string(),
    ]
}

/// Find an Emacs executable on PATH. Prefer `emacs`, but accept `emacs-nox`
/// (it is lighter and common in CI images). Returns `None` if neither exists.
pub fn find_emacs() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            for name in ["emacs", "emacs-nox"] {
                if Command::new(name).arg("--version").output().is_ok() {
                    return Some(name.to_string());
                }
            }
            None
        })
        .as_deref()
}

/// `Some(bin)` only if Emacs is present *and* `(require 'org-roam)` succeeds.
/// Cached because the probe takes a non-trivial Emacs startup. Prints the
/// Emacs stderr to aid CI debugging when the probe fails.
pub fn usable_emacs() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let bin = find_emacs()?;
            let mut cmd = Command::new(bin);
            cmd.arg("--batch")
                .arg("--no-window-system")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY");
            for arg in package_dir_args() {
                cmd.arg("--eval").arg(arg);
            }
            let probe = cmd
                .arg("--eval")
                .arg("(require 'package)")
                .arg("--eval")
                .arg("(package-initialize)")
                .arg("--eval")
                .arg("(require 'org-roam)")
                .arg("--eval")
                .arg("(princ \"ok\")")
                .output()
                .ok()?;
            let stdout = String::from_utf8_lossy(&probe.stdout);
            let stderr = String::from_utf8_lossy(&probe.stderr);
            if probe.status.success() && stdout.contains("ok") {
                Some(bin.to_string())
            } else {
                eprintln!("Emacs org-roam probe failed (status={:?}):", probe.status);
                eprintln!("stdout:\n{stdout}");
                eprintln!("stderr:\n{stderr}");
                None
            }
        })
        .as_deref()
}

/// Escape a string for safe use inside an Emacs Lisp string literal.
pub fn elisp_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Run an Emacs batch script, injecting the vault directory, DB path, and
/// target node id first. All org-roam/id state is isolated under `vault_dir`
/// so the test does not touch the developer's `~/.emacs.d`.
///
/// # Panics
///
/// Panics if `usable_emacs()` is `None`. Callers should skip the test first.
pub fn run_emacs_script(
    vault_dir: &Path,
    db_path: &Path,
    target_node_id: &str,
    script_path: &Path,
) -> std::process::Output {
    let bin = usable_emacs().expect("run_emacs_script called without usable Emacs");
    let vault_str = elisp_string(&vault_dir.to_string_lossy());
    let db_str = elisp_string(&db_path.to_string_lossy());
    let id_locations = elisp_string(&vault_dir.join(".org-id-locations").to_string_lossy());
    let id_str = elisp_string(target_node_id);

    let mut cmd = Command::new(bin);
    cmd.arg("--batch")
        .arg("--no-window-system")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    for arg in package_dir_args() {
        cmd.arg("--eval").arg(arg);
    }
    cmd.arg("--eval")
        .arg(format!("(setq org-roam-directory {vault_str})"))
        .arg("--eval")
        .arg(format!("(setq org-roam-db-location {db_str})"))
        .arg("--eval")
        .arg(format!("(setq org-id-locations-file {id_locations})"))
        .arg("--eval")
        .arg(format!("(setq target-node-id {id_str})"))
        .arg("--load")
        .arg(script_path)
        .output()
        .expect("failed to run Emacs")
}

/// Read the last non-empty line of Emacs stdout. Emacs package init can print
/// warnings before our script's output; the last line is the JSON we asked for.
pub fn last_stdout_line(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

/// Return the path to an embedded Emacs Lisp script.
pub fn elisp_script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("emacs")
        .join(name)
}
