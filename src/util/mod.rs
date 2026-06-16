//! Small utilities used across the server.

use std::path::Path;

/// Slugify a string for use in a filename: ASCII-safe, lowercase, hyphenated.
///
/// Deliberately stricter than org-roam's own `org-roam-node-slug` (which
/// uses underscores and keeps normalized non-ASCII): we drop non-ASCII and
/// use hyphens so the generated filenames are portable everywhere.
/// org-roam identifies nodes by `:ID:`, not filename, so the difference
/// is cosmetic.
#[must_use]
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for c in input.chars() {
        let mapped = match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => Some(c.to_ascii_lowercase()),
            ' ' | '_' | '\t' | '\n' | '\r' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>'
            | '|' => Some('-'),
            _ => None,
        };
        match mapped {
            Some('-') => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                }
                last_dash = true;
            }
            Some(c) => {
                out.push(c);
                last_dash = false;
            }
            None => {
                // drop non-ASCII; we want ASCII-clean paths
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

/// Default file name for a new node, following org-roam's default
/// `%<%Y%m%d%H%M%S>-${slug}.org` capture template.
#[must_use]
pub fn default_filename(timestamp: chrono::NaiveDateTime, title: &str) -> String {
    format!(
        "{}-{}.org",
        timestamp.format("%Y%m%d%H%M%S"),
        slugify(title)
    )
}

/// Atomically write `content` to `path`: write to a temp file in the same
/// directory, then rename. Refuses if an Emacs lockfile (`.#<path>`) is
/// present.
///
/// # Errors
///
/// Returns an `io::Error` if the file cannot be written or the Emacs lockfile is present.
pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;

    if emacs_lockfile_present(path) {
        return Err(std::io::Error::other(format!(
            "refusing to write: Emacs lockfile present for {}",
            path.display()
        )));
    }

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?;

    // Sibling temp file: `<file_name>.<random-uuid>.tmp`. Keeps the
    // rename in the same directory (required for atomic replace on POSIX).
    let tmp_name = format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    );
    let mut tmp = parent.to_path_buf();
    tmp.push(tmp_name);

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Delete `path`, refusing if an Emacs lockfile (`.#<name>`) is present.
///
/// # Errors
///
/// Returns an `io::Error` if the Emacs lockfile is present or the file
/// cannot be removed.
pub fn remove_file_unlocked(path: &Path) -> std::io::Result<()> {
    if emacs_lockfile_present(path) {
        return Err(std::io::Error::other(format!(
            "refusing to delete: Emacs lockfile present for {}",
            path.display()
        )));
    }
    std::fs::remove_file(path)
}

/// Rename `from` to `to`, refusing if either path has an Emacs lockfile.
///
/// # Errors
///
/// Returns an `io::Error` if a lockfile is present or the rename fails.
pub fn rename_unlocked(from: &Path, to: &Path) -> std::io::Result<()> {
    if emacs_lockfile_present(from) {
        return Err(std::io::Error::other(format!(
            "refusing to rename: Emacs lockfile present for {}",
            from.display()
        )));
    }
    if to.exists() {
        return Err(std::io::Error::other(format!(
            "refusing to rename: target already exists: {}",
            to.display()
        )));
    }
    std::fs::rename(from, to)
}

/// True if an Emacs file lock (`.#<name>`) exists for `path`.
#[must_use]
pub fn emacs_lockfile_present(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    parent.join(format!(".#{name}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real titles extracted from typical org-roam note names. These are
    // the kind of inputs `create_node` will slugify when computing the
    // default filename.

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  Multi   Space  "), "multi-space");
        assert_eq!(slugify("a/b:c"), "a-b-c");
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("---"), "untitled");
    }

    #[test]
    fn slugify_drops_non_ascii() {
        assert_eq!(slugify("café"), "caf");
        assert_eq!(slugify("über-note"), "ber-note");
    }

    #[test]
    fn slugify_real_titles() {
        // Idiomatic org-roam node titles.
        assert_eq!(
            slugify("What is the meaning of life?"),
            "what-is-the-meaning-of-life"
        );
        assert_eq!(
            slugify("How to take smart notes"),
            "how-to-take-smart-notes"
        );
        assert_eq!(
            slugify("Project Euler #1: Multiples of 3 and 5"),
            "project-euler-1-multiples-of-3-and-5"
        );
        assert_eq!(
            slugify("TODO: Refactor the cache layer"),
            "todo-refactor-the-cache-layer"
        );
        assert_eq!(
            slugify("DONE 2024-01-15 Migrate to rmcp"),
            "done-2024-01-15-migrate-to-rmcp"
        );
    }

    #[test]
    fn slugify_file_unsafe_chars() {
        // Every character that's unsafe on Windows + POSIX, replaced by a dash.
        assert_eq!(slugify("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
    }

    #[test]
    fn slugify_collapse_runs() {
        assert_eq!(slugify("a   b"), "a-b");
        assert_eq!(slugify("a - b - c"), "a-b-c");
        assert_eq!(slugify("a------b"), "a-b");
    }

    #[test]
    fn slugify_trims_edge_dashes() {
        assert_eq!(slugify("---hello---"), "hello");
        assert_eq!(slugify("/leading"), "leading");
        assert_eq!(slugify("trailing/"), "trailing");
    }

    #[test]
    fn slugify_is_idempotent() {
        // Slugging a slug should yield the same slug (no trailing dashes introduced).
        for raw in [
            "Hello World",
            "What is the meaning of life?",
            "Project Euler #1: Multiples of 3 and 5",
            "café — Über die Größe",
        ] {
            let once = slugify(raw);
            let twice = slugify(&once);
            assert_eq!(
                once, twice,
                "slugify must be idempotent: {raw:?} -> {once:?} -> {twice:?}"
            );
        }
    }

    #[test]
    fn slugify_preserves_numbers() {
        assert_eq!(slugify("Rust 2024 edition"), "rust-2024-edition");
        assert_eq!(slugify("2024-01-15 standup"), "2024-01-15-standup");
        assert_eq!(slugify("v1.2.3 release notes"), "v1-2-3-release-notes");
    }

    // -- atomic_write / emacs_lockfile_present --

    #[test]
    fn lockfile_detected_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.org");
        std::fs::write(&target, "body").unwrap();
        // No lockfile: should be false.
        assert!(!emacs_lockfile_present(&target));
        // Create a fake lockfile.
        std::fs::write(dir.path().join(".#note.org"), "").unwrap();
        assert!(emacs_lockfile_present(&target));
    }

    #[test]
    fn atomic_write_refuses_under_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.org");
        std::fs::write(&target, "original").unwrap();
        std::fs::write(dir.path().join(".#note.org"), "").unwrap();
        let err = atomic_write(&target, "new content").unwrap_err();
        assert!(err.to_string().contains("Emacs lockfile"));
        // Original content unchanged.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn atomic_write_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.org");
        atomic_write(&target, "hello, world\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello, world\n");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.org");
        atomic_write(&target, "first\n").unwrap();
        atomic_write(&target, "second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second\n");
    }

    #[test]
    fn atomic_write_does_not_leave_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.org");
        atomic_write(&target, "ok\n").unwrap();
        // Temp files are named `.<target>.<uuid>.tmp`; only the target
        // itself may remain.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tempfile leaked: {leftovers:?}");
    }

    #[test]
    fn default_filename_has_single_timestamp() {
        let ts = chrono::NaiveDate::from_ymd_opt(2026, 6, 11)
            .unwrap()
            .and_hms_opt(15, 42, 12)
            .unwrap();
        assert_eq!(
            default_filename(ts, "Test note"),
            "20260611154212-test-note.org"
        );
    }
}
