//! Build an `org-roam.db` directly from `.org` files, without Emacs.
//!
//! This is a self-contained Rust populator that creates the same `SQLite`
//! schema org-roam uses (version 20) and fills it by reusing the
//! filesystem scanner's parsing logic. It is intended as a fallback for
//! environments where Emacs is not available; when Emacs *is* available,
//! `org-roam-db-sync` remains the preferred builder because it writes the
//! canonical cache that org-roam itself will maintain.
//!
//! The produced database is read by [`crate::index::sqlite::SqliteIndex`].
//! Link and citation positions are extracted from the source files so the
//! graph matches Emacs more closely; full node property drawers are left as
//! `nil` until `org-roam-db-sync` refreshes them.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Transaction};
use serde::Serialize;
use sha1::{Digest, Sha1};

use super::scan::{walk_org_files, WalkOutcome};
use super::sqlite::emacsql;
use super::{IndexError, IndexResult};
use crate::index::LinkRecord;

/// Statistics returned by [`populate_database`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PopulateStats {
    pub files: usize,
    pub nodes: usize,
    pub links: usize,
    pub aliases: usize,
    pub tags: usize,
    pub refs: usize,
    pub citations: usize,
}

/// Options controlling the native database build.
#[derive(Debug, Clone)]
pub struct PopulateOptions {
    /// Path to write the database to.
    pub db_path: PathBuf,
    /// If true, overwrite an existing database; otherwise it is an error.
    pub overwrite: bool,
}

/// Build an `org-roam.db` at `options.db_path` from the `.org` files in
/// `roam_dir`.
///
/// Writes are atomic: the database is built in a temporary file next to
/// the target and renamed into place. If a file already exists at the
/// target and `overwrite` is false, an error is returned.
///
/// # Errors
///
/// Returns an error if the vault cannot be read, the database cannot be
/// written, or the target already exists and `overwrite` is false.
pub fn populate_database(roam_dir: &Path, options: &PopulateOptions) -> IndexResult<PopulateStats> {
    if roam_dir.is_file() {
        return Err(IndexError::Other(format!(
            "roam directory is a file: {}",
            roam_dir.display()
        )));
    }
    if !roam_dir.exists() {
        return Err(IndexError::Other(format!(
            "roam directory does not exist: {}",
            roam_dir.display()
        )));
    }

    let target = &options.db_path;
    if target.exists() && !options.overwrite {
        return Err(IndexError::Other(format!(
            "database already exists: {} (set overwrite:true to replace)",
            target.display()
        )));
    }

    // Build the in-memory scanner view. This gives us nodes, links, tags,
    // aliases, and refs parsed exactly like the scanner backend does.
    let walk = walk_org_files(roam_dir);

    // Prepare the parent directory and a sibling temp file so the final
    // rename is atomic and cheap.
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = temp_path(target);

    let stats = build_database(&tmp, roam_dir, &walk)?;

    // Atomically replace the target. If it already exists and overwrite
    // is true, rename it out of the way first so the temp file can take
    // its place without removing an open file on Windows.
    if target.exists() {
        let backup = backup_path(target);
        std::fs::rename(target, &backup)?;
    }
    std::fs::rename(&tmp, target)?;

    Ok(stats)
}

fn build_database(tmp: &Path, _roam_dir: &Path, walk: &WalkOutcome) -> IndexResult<PopulateStats> {
    let mut conn = Connection::open(tmp).map_err(IndexError::Sqlite)?;
    create_schema(&conn)?;

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(IndexError::Sqlite)?;

    let stats = populate_tables(&tx, walk)?;

    tx.commit().map_err(IndexError::Sqlite)?;
    Ok(stats)
}

fn create_schema(conn: &Connection) -> IndexResult<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE files (
             file TEXT PRIMARY KEY,
             title TEXT,
             hash TEXT NOT NULL,
             atime INTEGER NOT NULL,
             mtime INTEGER NOT NULL
         );
         CREATE TABLE nodes (
             id TEXT NOT NULL PRIMARY KEY,
             file TEXT NOT NULL,
             level INTEGER NOT NULL,
             pos INTEGER NOT NULL,
             todo TEXT,
             priority TEXT,
             scheduled TEXT,
             deadline TEXT,
             title TEXT,
             properties TEXT,
             olp TEXT,
             FOREIGN KEY(file) REFERENCES files(file) ON DELETE CASCADE
         );
         CREATE TABLE aliases (
             node_id TEXT NOT NULL,
             alias TEXT,
             FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE
         );
         CREATE INDEX alias_node_id ON aliases(node_id);
         CREATE TABLE citations (
             node_id TEXT NOT NULL,
             cite_key TEXT NOT NULL,
             pos INTEGER NOT NULL,
             properties TEXT,
             FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE
         );
         CREATE TABLE refs (
             node_id TEXT NOT NULL,
             ref TEXT NOT NULL,
             type TEXT NOT NULL,
             FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE
         );
         CREATE INDEX refs_node_id ON refs(node_id);
         CREATE TABLE tags (
             node_id TEXT NOT NULL,
             tag TEXT,
             FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE
         );
         CREATE INDEX tags_node_id ON tags(node_id);
         CREATE TABLE links (
             pos INTEGER NOT NULL,
             source TEXT NOT NULL,
             dest TEXT NOT NULL,
             type TEXT NOT NULL,
             properties TEXT NOT NULL,
             FOREIGN KEY(source) REFERENCES nodes(id) ON DELETE CASCADE
         );
         PRAGMA user_version = 20;",
    )
    .map_err(IndexError::Sqlite)
}

fn populate_tables(tx: &Transaction<'_>, walk: &WalkOutcome) -> IndexResult<PopulateStats> {
    let mut stats = PopulateStats::default();
    let now = now_seconds();

    // Collect the set of files that have at least one node. The files
    // table gets a row for each of these; files without IDs are ignored
    // (org-roam will pick them up on its next sync if needed).
    let mut files_with_nodes: HashMap<PathBuf, Vec<&crate::index::NodeMeta>> = HashMap::new();
    for node in walk.nodes.values() {
        files_with_nodes
            .entry(node.file.clone())
            .or_default()
            .push(node);
    }

    let mut inserted_nodes: HashSet<String> = HashSet::new();

    for (path, nodes) in &files_with_nodes {
        insert_file_with_nodes(tx, walk, path, nodes, now, &mut inserted_nodes, &mut stats)?;
    }

    insert_refs(tx, walk, &mut stats)?;
    insert_links(tx, walk, &inserted_nodes, &mut stats)?;
    insert_citations(tx, walk, &inserted_nodes, &mut stats)?;

    Ok(stats)
}

fn insert_file_with_nodes(
    tx: &Transaction<'_>,
    walk: &WalkOutcome,
    path: &Path,
    nodes: &[&crate::index::NodeMeta],
    now: i64,
    inserted_nodes: &mut HashSet<String>,
    stats: &mut PopulateStats,
) -> IndexResult<()> {
    let title = walk
        .file_titles
        .get(path)
        .cloned()
        .unwrap_or_else(|| file_stem_title(path));
    let (hash, mtime) = file_hash_and_mtime(path)?;
    tx.execute(
        "INSERT INTO files (file, title, hash, atime, mtime) VALUES (?, ?, ?, ?, ?)",
        [
            emacsql::quote(&path.to_string_lossy()),
            emacsql::quote(&title),
            hash,
            now.to_string(),
            mtime.to_string(),
        ],
    )
    .map_err(IndexError::Sqlite)?;
    stats.files += 1;

    for node in nodes {
        insert_node(tx, node)?;
        inserted_nodes.insert(node.id.clone());
        stats.nodes += 1;

        for alias in &node.aliases {
            tx.execute(
                "INSERT INTO aliases (node_id, alias) VALUES (?, ?)",
                [emacsql::quote(&node.id), emacsql::quote(alias)],
            )
            .map_err(IndexError::Sqlite)?;
            stats.aliases += 1;
        }

        for tag in &node.tags {
            tx.execute(
                "INSERT INTO tags (node_id, tag) VALUES (?, ?)",
                [emacsql::quote(&node.id), emacsql::quote(tag)],
            )
            .map_err(IndexError::Sqlite)?;
            stats.tags += 1;
        }
    }

    Ok(())
}

fn insert_refs(
    tx: &Transaction<'_>,
    walk: &WalkOutcome,
    stats: &mut PopulateStats,
) -> IndexResult<()> {
    // Refs (ROAM_REFS + in-body citations). The scanner merges them,
    // which is what the read side expects for `by_ref` lookups.
    for (raw_ref, node_ids) in &walk.refs {
        let rows = classify_ref(raw_ref);
        for node_id in node_ids {
            // A ref can declare multiple rows (e.g. a [cite:@a;@b] form).
            for (stored_ref, ref_type) in &rows {
                tx.execute(
                    "INSERT INTO refs (node_id, ref, type) VALUES (?, ?, ?)",
                    [
                        emacsql::quote(node_id),
                        emacsql::quote(stored_ref),
                        emacsql::quote(ref_type),
                    ],
                )
                .map_err(IndexError::Sqlite)?;
                stats.refs += 1;
            }
        }
    }
    Ok(())
}

fn insert_links(
    tx: &Transaction<'_>,
    walk: &WalkOutcome,
    inserted_nodes: &HashSet<String>,
    stats: &mut PopulateStats,
) -> IndexResult<()> {
    // We only insert links whose source node was actually inserted and
    // whose destination resolves to a known node when the link type is
    // `id`. Other link types are stored with their org-roam dest encoding.
    let mut inserted_links: HashSet<(String, String, String)> = HashSet::new();
    for (source, links) in &walk.forward {
        if !inserted_nodes.contains(source) {
            continue;
        }
        for link in links {
            let dest = link_dest_for_db(link);
            if link.kind == "id" && !walk.nodes.contains_key(&dest) {
                continue;
            }
            // org-roam's links table has no unique constraint, but the
            // graph semantics are one edge per (source, dest, type).
            let key = (source.clone(), dest.clone(), link.kind.clone());
            if !inserted_links.insert(key) {
                continue;
            }
            let pos = i64::try_from(link.pos.unwrap_or(0)).unwrap_or(0);
            tx.execute(
                "INSERT INTO links (pos, source, dest, type, properties) VALUES (?, ?, ?, ?, ?)",
                [
                    pos.to_string(),
                    emacsql::quote(source),
                    emacsql::quote(&dest),
                    emacsql::quote(&link.kind),
                    "nil".to_string(),
                ],
            )
            .map_err(IndexError::Sqlite)?;
            stats.links += 1;
        }
    }
    Ok(())
}

fn insert_citations(
    tx: &Transaction<'_>,
    walk: &WalkOutcome,
    inserted_nodes: &HashSet<String>,
    stats: &mut PopulateStats,
) -> IndexResult<()> {
    // In-body citations are represented as `cite` LinkRecords in the
    // forward map. Insert each one into the citations table with its
    // source position, matching org-roam's in-body citation tracking.
    let mut inserted: HashSet<(String, String, usize)> = HashSet::new();
    for (source, links) in &walk.forward {
        if !inserted_nodes.contains(source) {
            continue;
        }
        for link in links {
            if link.kind != "cite" {
                continue;
            }
            let Some(cite_key) = link
                .ref_target
                .as_ref()
                .map(|s| s.trim_start_matches('@').to_string())
            else {
                continue;
            };
            let pos = link.pos.unwrap_or(0);
            let key = (source.clone(), cite_key.clone(), pos);
            if !inserted.insert(key) {
                continue;
            }
            tx.execute(
                "INSERT INTO citations (node_id, cite_key, pos, properties) VALUES (?, ?, ?, ?)",
                [
                    emacsql::quote(source),
                    emacsql::quote(&cite_key),
                    i64::try_from(pos).unwrap_or(0).to_string(),
                    "nil".to_string(),
                ],
            )
            .map_err(IndexError::Sqlite)?;
            stats.citations += 1;
        }
    }
    Ok(())
}

fn insert_node(tx: &Transaction<'_>, node: &crate::index::NodeMeta) -> IndexResult<()> {
    let level = i64::try_from(node.level.unwrap_or(0)).unwrap_or(0);
    let pos = i64::try_from(node.pos.unwrap_or(0)).unwrap_or(0);
    let title = if node.title.is_empty() {
        "nil".to_string()
    } else {
        emacsql::quote(&node.title)
    };
    let todo = node
        .todo
        .as_ref()
        .map_or_else(|| "nil".to_string(), |s| emacsql::quote(s));
    let priority = node
        .priority
        .as_ref()
        .map_or_else(|| "nil".to_string(), |s| emacsql::quote(s));
    let olp = if node.olp.is_empty() {
        "nil".to_string()
    } else {
        emacsql::quote(&format_list(&node.olp))
    };

    tx.execute(
        "INSERT INTO nodes (id, file, level, pos, todo, priority, scheduled, deadline, title, properties, olp) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        [
            emacsql::quote(&node.id),
            emacsql::quote(&node.file.to_string_lossy()),
            level.to_string(),
            pos.to_string(),
            todo,
            priority,
            "nil".to_string(),
            "nil".to_string(),
            title,
            "nil".to_string(),
            olp,
        ],
    )
    .map_err(IndexError::Sqlite)?;
    Ok(())
}

/// Encode an outline path as a printed Lisp list of strings.
fn format_list(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|s| emacsql::quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    format!("({inner})")
}

/// Compute the org-roam-compatible SHA1 hash of a file's raw bytes, plus
/// its filesystem modification time as a Unix timestamp.
fn file_hash_and_mtime(path: &Path) -> IndexResult<(String, i64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = format!("{:x}", hasher.finalize());

    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));

    Ok((hash, mtime))
}

fn file_stem_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string()
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn backup_path(target: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    target.with_extension(format!("db.backup-{stamp}"))
}

/// Sibling temp file path for atomic database creation.
fn temp_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .map_or_else(|| "org-roam.db".into(), |s| s.to_string_lossy());
    parent.join(format!(".{}.{}.tmp", file_name, uuid::Uuid::new_v4()))
}

/// Convert a scanner [`LinkRecord`] into the dest string org-roam stores
/// in the `links` table.
fn link_dest_for_db(link: &LinkRecord) -> String {
    match link.kind.as_str() {
        "id" => link.dest.clone().unwrap_or_default(),
        "http" | "https" => strip_scheme(link.ref_target.as_ref().unwrap_or(&link.raw_dest)),
        "cite" => link.ref_target.as_ref().map_or_else(
            || {
                link.raw_dest
                    .strip_prefix('@')
                    .unwrap_or(&link.raw_dest)
                    .to_string()
            },
            |s| s.strip_prefix('@').unwrap_or(s).to_string(),
        ),
        "file" => link
            .raw_dest
            .strip_prefix("file:")
            .unwrap_or(&link.raw_dest)
            .to_string(),
        _ => link.raw_dest.clone(),
    }
}

fn strip_scheme(url: &str) -> String {
    url.split_once("://")
        .map_or_else(|| url.to_string(), |(_, rest)| format!("//{rest}"))
}

/// Classify a raw ref value the way `org-roam-db-insert-refs` does,
/// returning one or more `(stored_ref, type)` rows.
fn classify_ref(raw: &str) -> Vec<(String, String)> {
    let trimmed = raw.trim();

    // @citekey
    if let Some(key) = trimmed.strip_prefix('@') {
        if !key.is_empty() {
            return vec![(key.to_string(), "cite".to_string())];
        }
        return vec![];
    }

    // [cite:@key1; @key2]
    if trimmed.starts_with("[cite:") && trimmed.ends_with(']') {
        let inner = &trimmed[6..trimmed.len() - 1];
        // Accept an optional /style segment before the colon.
        let content = inner.split_once(':').map_or(inner, |(_, c)| c);
        let mut out = Vec::new();
        for key in content.split([';', ',']) {
            let key = key.trim();
            let first = key.split_whitespace().next().unwrap_or("");
            if let Some(k) = first.strip_prefix('@') {
                if !k.is_empty() {
                    out.push((k.to_string(), "cite".to_string()));
                }
            }
        }
        return out;
    }

    // URL-like refs: store scheme-less form with the scheme as type.
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        if !scheme.is_empty() && !rest.is_empty() {
            return vec![(format!("//{rest}"), scheme.to_string())];
        }
    }

    // Treat everything else as a generic "id" ref. This matches org-roam's
    // fallback for id: or roam: refs, and is harmless if the ref is not
    // actually resolvable.
    if !trimmed.is_empty() {
        return vec![(trimmed.to_string(), "id".to_string())];
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ref_citekey() {
        assert_eq!(
            classify_ref("@nora2023"),
            vec![("nora2023".into(), "cite".into())]
        );
    }

    #[test]
    fn classify_ref_cite_form() {
        assert_eq!(
            classify_ref("[cite:@nora2023; @smith2024 p. 42]"),
            vec![
                ("nora2023".into(), "cite".into()),
                ("smith2024".into(), "cite".into()),
            ]
        );
    }

    #[test]
    fn classify_ref_url() {
        assert_eq!(
            classify_ref("https://example.com/path"),
            vec![("//example.com/path".into(), "https".into())]
        );
    }

    #[test]
    fn classify_ref_fallback() {
        assert_eq!(
            classify_ref("id:11111111-1111-1111-1111-111111111111"),
            vec![(
                "id:11111111-1111-1111-1111-111111111111".into(),
                "id".into()
            )]
        );
    }

    #[test]
    fn link_dest_for_db_variants() {
        assert_eq!(
            link_dest_for_db(&LinkRecord {
                source: "a".into(),
                dest: Some("b".into()),
                raw_dest: "id:b".into(),
                kind: "id".into(),
                ref_target: None,
                pos: None,
            }),
            "b"
        );
        assert_eq!(
            link_dest_for_db(&LinkRecord {
                source: "a".into(),
                dest: None,
                raw_dest: "https://example.com".into(),
                kind: "https".into(),
                ref_target: Some("https://example.com".into()),
                pos: None,
            }),
            "//example.com"
        );
        assert_eq!(
            link_dest_for_db(&LinkRecord {
                source: "a".into(),
                dest: None,
                raw_dest: "@nora2023".into(),
                kind: "cite".into(),
                ref_target: Some("@nora2023".into()),
                pos: None,
            }),
            "nora2023"
        );
    }
}
