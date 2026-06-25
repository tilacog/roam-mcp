//! SQLite-backed index reading org-roam's own `org-roam.db`.
//!
//! `org-roam.db` is created by Emacs (via emacsql) and is the canonical
//! source of node metadata, backlinks, reflinks, and tags. We open it
//! read-only so a running Emacs is never disturbed, and tolerate
//! `SQLITE_BUSY` with a short retry loop.
//!
//! emacsql stores every text value as a *printed Lisp object* — strings
//! arrive quote-wrapped (`"Pastafarian Canticle"`), lists as `("a" "b")`, null as
//! `nil`. Two rules keep that encoding from leaking anywhere else:
//! every text column is decoded in [`row_to_node_meta`] / the row
//! mappers, and every equality bind goes through [`emacsql::quote`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Row};

use super::{IndexError, IndexResult, LinkRecord, NodeMeta, NodeQuery, RoamIndex};

/// Read-only `SQLite` index over an `org-roam.db` file.
pub struct SqliteIndex {
    /// Cached schema snapshot: table -> set of column names. We sample this
    /// once at open time so we can run a version-tolerant set of queries.
    schema: HashMap<String, Vec<String>>,
    /// `rusqlite::Connection` is `!Sync` (interior mutability via `RefCell`
    /// for its statement cache), so we wrap it in a `Mutex`. All queries
    /// are short; contention is minimal in practice.
    conn: Mutex<Connection>,
    path: PathBuf,
    /// Detected schema version (if `meta` table exposes it).
    version: Option<String>,
}

const BUSY_RETRIES: usize = 8;
const BUSY_BACKOFF_MS: u64 = 50;

impl SqliteIndex {
    /// Open the given `org-roam.db` in read-only mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or is malformed.
    pub fn open(path: &Path) -> IndexResult<Self> {
        let conn = open_ro(path)?;
        // query_only is belt-and-suspenders on top of SQLITE_OPEN_READ_ONLY.
        conn.execute_batch("PRAGMA query_only = 1;")
            .map_err(|e| IndexError::Other(format!("setting query_only: {e}")))?;

        let schema = introspect_schema(&conn)?;
        let version = read_meta_version(&conn).ok().flatten();
        Ok(Self {
            schema,
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            version,
        })
    }

    /// The detected `db-version` from the `meta` table, if any.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Whether the table exists and has *all* of the listed columns
    /// (the queries that follow a check reference every listed column).
    fn has_columns(&self, table: &str, cols: &[&str]) -> bool {
        match self.schema.get(table) {
            None => false,
            Some(present) => cols.iter().all(|c| present.iter().any(|p| p == *c)),
        }
    }

    /// Lock the connection. Returns `IndexError::Other` if poisoned.
    fn lock(&self) -> IndexResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| {
            IndexError::Other("sqlite connection mutex poisoned (panicked during query)".into())
        })
    }

    /// Known node-table columns we project (NULL-filled if absent), and
    /// the SELECT list that does so for this database's actual schema.
    fn node_select(&self) -> String {
        const COLS: &[&str] = &[
            "id", "file", "title", "level", "pos", "todo", "priority", "olp",
        ];
        let present = self.schema.get("nodes").cloned().unwrap_or_default();
        COLS.iter()
            .map(|c| {
                if present.iter().any(|x| x == *c) {
                    (*c).to_string()
                } else {
                    format!("NULL AS {c}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Open a connection, retrying `SQLITE_BUSY` a handful of times.
fn open_ro(path: &Path) -> IndexResult<Connection> {
    let mut last_err = None;
    for _ in 0..BUSY_RETRIES {
        match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => return Ok(c),
            Err(e) => match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::DatabaseBusy =>
                {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(BUSY_BACKOFF_MS));
                }
                _ => return Err(IndexError::Sqlite(e)),
            },
        }
    }
    Err(IndexError::Sqlite(
        last_err.expect("at least one error captured in retry loop"),
    ))
}

fn introspect_schema(conn: &Connection) -> IndexResult<HashMap<String, Vec<String>>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(IndexError::Sqlite)?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(IndexError::Sqlite)?
        .filter_map(Result::ok)
        .collect();

    let mut out = HashMap::new();
    for table in tables {
        let pragma = format!("PRAGMA table_info({table})");
        let mut stmt = conn.prepare(&pragma).map_err(IndexError::Sqlite)?;
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(IndexError::Sqlite)?
            .filter_map(Result::ok)
            .collect();
        out.insert(table, cols);
    }
    Ok(out)
}

fn read_meta_version(conn: &Connection) -> IndexResult<Option<String>> {
    let mut stmt = conn
        .prepare("SELECT value FROM meta WHERE key = 'db-version'")
        .map_err(IndexError::Sqlite)?;
    let mut rows = stmt.query([]).map_err(IndexError::Sqlite)?;
    if let Some(row) = rows.next().map_err(IndexError::Sqlite)? {
        let raw: String = row.get(0).map_err(IndexError::Sqlite)?;
        return Ok(Some(emacsql::unlisp_string(&raw).unwrap_or(raw)));
    }
    Ok(None)
}

impl RoamIndex for SqliteIndex {
    fn find_nodes(&self, q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>> {
        if !self.has_columns("nodes", &["id", "file"]) {
            return Err(IndexError::Malformed(
                "org-roam.db has no `nodes` table with id and file columns".into(),
            ));
        }
        let mut sql = format!("SELECT {} FROM nodes", self.node_select());
        let mut where_clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(query) = q.query.filter(|s| !s.is_empty()) {
            // Coarse substring prefilter in SQL — emacsql's quote-wrapping
            // doesn't disturb a LIKE, but its case rules differ from ours,
            // so the authoritative filter is reapplied on the decoded
            // values in Rust below. A query matches a node's title, an
            // alias, or a tag (the scanner backend matches all three, so
            // this one must too).
            let needle = format!("%{query}%");
            let mut clause = String::from(
                "(title LIKE ? OR id IN (SELECT node_id FROM aliases WHERE alias LIKE ?)",
            );
            binds.push(Box::new(needle.clone()));
            binds.push(Box::new(needle.clone()));
            if self.has_columns("tags", &["node_id", "tag"]) {
                clause.push_str(" OR id IN (SELECT node_id FROM tags WHERE tag LIKE ?)");
                binds.push(Box::new(needle.clone()));
            }
            clause.push(')');
            where_clauses.push(clause);
        }
        if !q.tags.is_empty() && self.has_columns("tags", &["node_id", "tag"]) {
            for t in q.tags {
                where_clauses.push("id IN (SELECT node_id FROM tags WHERE tag = ?)".to_string());
                binds.push(Box::new(emacsql::quote(t)));
            }
        } else if !q.tags.is_empty() {
            // Tag filter requested but this db has no usable tags table:
            // nothing can match.
            return Ok(vec![]);
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY title");

        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| &**b).collect();
        let rows = stmt
            .query_map(&bind_refs[..], row_to_node_meta)
            .map_err(IndexError::Sqlite)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        drop(stmt);
        drop(conn);

        // Attach aliases and tags before filtering: the needle filter
        // needs aliases, and the scanner backend returns both fields on
        // every hit, so this backend must too.
        self.attach_aliases_and_tags(&mut out)?;

        // Authoritative needle filter on the decoded title/aliases. The
        // limit is applied after this so a SQL prefilter can never starve
        // the result set.
        if let Some(query) = q.query.filter(|s| !s.is_empty()) {
            let q_lower = query.to_lowercase();
            out.retain(|n| {
                n.title.to_lowercase().contains(&q_lower)
                    || n.aliases
                        .iter()
                        .any(|a| a.to_lowercase().contains(&q_lower))
                    || n.tags.iter().any(|t| t.to_lowercase().contains(&q_lower))
            });
        }
        if let Some(lim) = q.limit {
            out.truncate(lim);
        }
        Ok(out)
    }

    fn node(&self, id: &str) -> IndexResult<Option<NodeMeta>> {
        if !self.has_columns("nodes", &["id"]) {
            return Ok(None);
        }
        let sql = format!("SELECT {} FROM nodes WHERE id = ?", self.node_select());
        let meta = {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
            let mut rows = stmt
                .query([emacsql::quote(id)])
                .map_err(IndexError::Sqlite)?;
            match rows.next().map_err(IndexError::Sqlite)? {
                Some(r) => Some(row_to_node_meta(r)?),
                None => None,
            }
        };
        let Some(meta) = meta else { return Ok(None) };
        let mut nodes = vec![meta];
        self.attach_aliases_and_tags(&mut nodes)?;
        Ok(nodes.pop())
    }

    fn node_by_path(&self, path: &Path) -> IndexResult<Option<NodeMeta>> {
        if !self.has_columns("nodes", &["id", "file"]) {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {} FROM nodes WHERE file = ? AND level = 0",
            self.node_select()
        );
        let meta = {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
            // Paths in sqlite are absolute and quoted.
            let path_str = path.to_str().unwrap_or_default();
            let mut rows = stmt
                .query([emacsql::quote(path_str)])
                .map_err(IndexError::Sqlite)?;
            match rows.next().map_err(IndexError::Sqlite)? {
                Some(r) => Some(row_to_node_meta(r)?),
                None => None,
            }
        };
        let Some(meta) = meta else { return Ok(None) };
        let mut nodes = vec![meta];
        self.attach_aliases_and_tags(&mut nodes)?;
        Ok(nodes.pop())
    }

    fn backlinks(&self, id: &str) -> IndexResult<Vec<LinkRecord>> {
        if !self.has_columns("links", &["source", "dest", "type"]) {
            return Ok(vec![]);
        }
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT source, dest, type FROM links WHERE dest = ?")
            .map_err(IndexError::Sqlite)?;
        let rows = stmt
            .query_map([emacsql::quote(id)], |r| {
                let source: String = r.get(0)?;
                let dest: String = r.get(1)?;
                let kind: Option<String> = r.get(2)?;
                Ok(link_record(&source, &dest, kind.as_deref()))
            })
            .map_err(IndexError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::Sqlite)
    }

    fn forward_links(&self, id: &str) -> IndexResult<Vec<LinkRecord>> {
        let mut out: Vec<LinkRecord> = Vec::new();

        // Primary: links table (org-roam's canonical link graph).
        if self.has_columns("links", &["source", "dest", "type"]) {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare("SELECT source, dest, type FROM links WHERE source = ?")
                .map_err(IndexError::Sqlite)?;
            let rows = stmt
                .query_map([emacsql::quote(id)], |r| {
                    let source: String = r.get(0)?;
                    let dest: String = r.get(1)?;
                    let kind: Option<String> = r.get(2)?;
                    Ok(link_record(&source, &dest, kind.as_deref()))
                })
                .map_err(IndexError::Sqlite)?;
            out.extend(
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(IndexError::Sqlite)?,
            );
        }

        // Secondary: citations table (in-body [cite:@key] declarations that
        // org-roam tracks separately from ROAM_REFS). Each citation becomes a
        // `cite` LinkRecord with no node destination (same shape as the
        // scanner backend's in-body citation records).
        if self.has_columns("citations", &["node_id", "cite_key"]) {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare("SELECT cite_key FROM citations WHERE node_id = ?")
                .map_err(IndexError::Sqlite)?;
            let rows = stmt
                .query_map([emacsql::quote(id)], |r| {
                    let cite_key: String = r.get(0)?;
                    Ok(emacsql::decode(&cite_key))
                })
                .map_err(IndexError::Sqlite)?;
            for row in rows {
                let key = row.map_err(IndexError::Sqlite)?;
                let raw_dest = format!("@{key}");
                out.push(LinkRecord {
                    source: id.to_string(),
                    dest: None,
                    raw_dest: raw_dest.clone(),
                    kind: "cite".to_string(),
                    ref_target: Some(raw_dest),
                });
            }
        }

        Ok(out)
    }

    fn by_ref(&self, r: &str) -> IndexResult<Vec<NodeMeta>> {
        let mut out: Vec<NodeMeta> = Vec::new();

        // Primary path: refs table (ROAM_REFS property-drawer declarations).
        // org-roam splits a ref before storing it: `@citekey` loses its `@`
        // (type "cite"), and a URL loses its scheme (type "https",
        // ref "//host/path"). Accept the user-facing form and match any
        // stored variant.
        if self.has_columns("refs", &["node_id", "ref"]) {
            let mut candidates = vec![emacsql::quote(r)];
            if let Some(key) = r.strip_prefix('@') {
                candidates.push(emacsql::quote(key));
            }
            if let Some((_scheme, rest)) = r.split_once("://") {
                candidates.push(emacsql::quote(&format!("//{rest}")));
            }
            let placeholders = vec!["?"; candidates.len()].join(", ");
            let sql = format!(
                "SELECT {} FROM nodes WHERE id IN \
                 (SELECT node_id FROM refs WHERE ref IN ({placeholders}))",
                self.node_select()
            );
            let conn = self.lock()?;
            let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
            let bind_refs: Vec<&dyn rusqlite::ToSql> = candidates
                .iter()
                .map(|c| c as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt
                .query_map(&bind_refs[..], row_to_node_meta)
                .map_err(IndexError::Sqlite)?;
            out.extend(
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(IndexError::Sqlite)?,
            );
        }

        // Secondary path: citations table (in-body [cite:@key] declarations).
        // org-roam stores these separately from ROAM_REFS, with the bare key
        // (without the `@` sigil). Skip URL-shaped inputs — citations never
        // use URL syntax.
        if self.has_columns("citations", &["node_id", "cite_key"]) {
            let cite_key = r.strip_prefix('@').unwrap_or(r);
            if !cite_key.contains("://") {
                let sql = format!(
                    "SELECT {} FROM nodes WHERE id IN \
                     (SELECT node_id FROM citations WHERE cite_key = ?)",
                    self.node_select()
                );
                let conn = self.lock()?;
                let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
                let extra: Vec<NodeMeta> = stmt
                    .query_map([emacsql::quote(cite_key)], row_to_node_meta)
                    .map_err(IndexError::Sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(IndexError::Sqlite)?;
                // Merge without duplicates (a node may appear in both refs
                // and citations for the same key).
                let new_nodes: Vec<NodeMeta> = {
                    let existing: std::collections::HashSet<&str> =
                        out.iter().map(|n| n.id.as_str()).collect();
                    extra
                        .into_iter()
                        .filter(|n| !existing.contains(n.id.as_str()))
                        .collect()
                };
                out.extend(new_nodes);
            }
        }

        self.attach_aliases_and_tags(&mut out)?;
        Ok(out)
    }

    fn tags(&self) -> IndexResult<Vec<(String, usize)>> {
        if !self.has_columns("tags", &["node_id", "tag"]) {
            return Ok(vec![]);
        }
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT tag, COUNT(DISTINCT node_id) FROM tags \
                 GROUP BY tag ORDER BY tag",
            )
            .map_err(IndexError::Sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                let tag: String = r.get(0)?;
                let count: usize = r.get(1)?;
                Ok((emacsql::decode(&tag), count))
            })
            .map_err(IndexError::Sqlite)?;
        let out: Vec<(String, usize)> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::Sqlite)?
            .into_iter()
            .filter(|(t, _)| !t.is_empty())
            .collect();
        Ok(out)
    }

    fn node_count(&self) -> IndexResult<usize> {
        if !self.has_columns("nodes", &["id"]) {
            return Ok(0);
        }
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .map_err(IndexError::Sqlite)?;
        usize::try_from(n).map_err(|_| IndexError::Other("node count overflow".into()))
    }

    fn random_node(&self) -> IndexResult<NodeMeta> {
        if !self.has_columns("nodes", &["id"]) {
            return Err(IndexError::NotFound("no nodes table".into()));
        }
        let node = {
            let conn = self.lock()?;
            let sql = format!(
                "SELECT {} FROM nodes ORDER BY RANDOM() LIMIT 1",
                self.node_select()
            );
            let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
            let mut rows = stmt
                .query_map([], row_to_node_meta)
                .map_err(IndexError::Sqlite)?;
            match rows.next() {
                Some(Ok(n)) => n,
                Some(Err(e)) => return Err(IndexError::Sqlite(e)),
                None => return Err(IndexError::NotFound("index is empty".into())),
            }
        };
        let mut nodes = vec![node];
        self.attach_aliases_and_tags(&mut nodes)?;
        Ok(nodes.pop().unwrap())
    }

    fn orphans(&self) -> IndexResult<Vec<NodeMeta>> {
        if !self.has_columns("nodes", &["id"]) {
            return Ok(vec![]);
        }
        // `NOT IN` against `links.type = "id"` filters to the id-link
        // graph only. URL / file / cite links live in the same `links`
        // table but never resolve to a node id, so they are not edges
        // in the note graph and do not disqualify a node from being an
        // orphan. The `id` literal is emacsql-encoded like every other
        // string column we filter on.
        let id_type = emacsql::quote("id");
        let sql = if self.has_columns("links", &["source", "dest", "type"]) {
            format!(
                "SELECT {} FROM nodes \
                 WHERE id NOT IN (SELECT source FROM links WHERE type = ?) \
                   AND id NOT IN (SELECT dest FROM links WHERE type = ?) \
                 ORDER BY title",
                self.node_select()
            )
        } else {
            // No link table means we have no edge data: treat every
            // node as a potential orphan so the caller is prompted to
            // investigate (or run an emacsclient sync).
            format!("SELECT {} FROM nodes ORDER BY title", self.node_select())
        };
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
        let rows = if self.has_columns("links", &["source", "dest", "type"]) {
            stmt.query_map(rusqlite::params![&id_type, &id_type], row_to_node_meta)
                .map_err(IndexError::Sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(IndexError::Sqlite)?
        } else {
            stmt.query_map([], row_to_node_meta)
                .map_err(IndexError::Sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(IndexError::Sqlite)?
        };
        drop(stmt);
        drop(conn);
        let mut out = rows;
        self.attach_aliases_and_tags(&mut out)?;
        Ok(out)
    }

    fn source(&self) -> &str {
        self.path.to_str().unwrap_or("(sqlite)")
    }

    fn nodes_with_external_links(&self) -> IndexResult<Vec<(NodeMeta, Vec<LinkRecord>)>> {
        if !self.has_columns("links", &["source", "dest", "type"]) {
            return Ok(vec![]);
        }

        let types = [
            emacsql::quote("file"),
            emacsql::quote("http"),
            emacsql::quote("https"),
            emacsql::quote("cite"),
        ];

        let mut grouped: HashMap<String, Vec<LinkRecord>> = HashMap::new();
        {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT source, dest, type FROM links \
                     WHERE type IN ('{}', '{}', '{}', '{}') \
                     ORDER BY source",
                    types[0], types[1], types[2], types[3]
                ))
                .map_err(IndexError::Sqlite)?;

            let rows = stmt
                .query_map([], |r| {
                    let source: String = r.get(0)?;
                    let dest: String = r.get(1)?;
                    let kind: Option<String> = r.get(2)?;
                    Ok(link_record(&source, &dest, kind.as_deref()))
                })
                .map_err(IndexError::Sqlite)?;

            for row in rows {
                let l = row.map_err(IndexError::Sqlite)?;
                grouped.entry(l.source.clone()).or_default().push(l);
            }
        }

        if grouped.is_empty() {
            return Ok(vec![]);
        }

        let mut ids: Vec<String> = grouped.keys().cloned().collect();
        ids.sort();

        let mut nodes = Vec::new();
        {
            let conn = self.lock()?;
            for chunk in ids.chunks(500) {
                let placeholders = vec!["?"; chunk.len()].join(", ");
                let sql = format!(
                    "SELECT {} FROM nodes WHERE id IN ({})",
                    self.node_select(),
                    placeholders
                );
                let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
                let bind_refs: Vec<String> = chunk.iter().map(|id| emacsql::quote(id)).collect();
                let bind_refs_borrowed: Vec<&dyn rusqlite::ToSql> = bind_refs
                    .iter()
                    .map(|c| c as &dyn rusqlite::ToSql)
                    .collect();

                let rows = stmt
                    .query_map(&bind_refs_borrowed[..], row_to_node_meta)
                    .map_err(IndexError::Sqlite)?;

                for row in rows {
                    nodes.push(row.map_err(IndexError::Sqlite)?);
                }
            }
        }

        self.attach_aliases_and_tags(&mut nodes)?;

        let mut out = Vec::new();
        for node in nodes {
            if let Some(links) = grouped.remove(&node.id) {
                out.push((node, links));
            }
        }

        // Sort by title to match scanner backend and other tools.
        out.sort_by(|a, b| a.0.title.cmp(&b.0.title));
        Ok(out)
    }
}

impl SqliteIndex {
    /// Populate `aliases` and `tags` on every node, with one batched
    /// query per side table instead of two queries per node.
    fn attach_aliases_and_tags(&self, nodes: &mut [NodeMeta]) -> IndexResult<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let encoded: Vec<String> = nodes.iter().map(|n| emacsql::quote(&n.id)).collect();
        let aliases = self.values_by_node("aliases", "alias", &encoded)?;
        let tags = self.values_by_node("tags", "tag", &encoded)?;
        for n in nodes {
            if let Some(v) = aliases.get(&n.id) {
                n.aliases.clone_from(v);
            }
            if let Some(v) = tags.get(&n.id) {
                n.tags.clone_from(v);
            }
        }
        Ok(())
    }

    /// `node id → decoded values` from a side table (`aliases`/`tags`),
    /// restricted to the given emacsql-encoded node ids. Chunked to stay
    /// under `SQLite`'s host-parameter limit.
    fn values_by_node(
        &self,
        table: &str,
        column: &str,
        encoded_ids: &[String],
    ) -> IndexResult<HashMap<String, Vec<String>>> {
        if !self.has_columns(table, &["node_id", column]) {
            return Ok(HashMap::new());
        }
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let conn = self.lock()?;
        for chunk in encoded_ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT node_id, {column} FROM {table} \
                 WHERE node_id IN ({placeholders}) ORDER BY {column}"
            );
            let mut stmt = conn.prepare(&sql).map_err(IndexError::Sqlite)?;
            let bind_refs: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|c| c as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(&bind_refs[..], |r| {
                    let node_id: String = r.get(0)?;
                    let value: String = r.get(1)?;
                    Ok((emacsql::decode(&node_id), emacsql::decode(&value)))
                })
                .map_err(IndexError::Sqlite)?;
            for row in rows {
                let (id, value) = row.map_err(IndexError::Sqlite)?;
                out.entry(id).or_default().push(value);
            }
        }
        Ok(out)
    }
}

/// Build a [`LinkRecord`] from decoded `links` table values.
fn link_record(source: &str, dest: &str, kind: Option<&str>) -> LinkRecord {
    let source = emacsql::decode(source);
    let dest = emacsql::decode(dest);
    let kind = kind.map_or_else(|| "id".to_string(), emacsql::decode);
    let ref_target = match kind.as_str() {
        "cite" => Some(format!("@{dest}")),
        "http" | "https" => Some(format!("{kind}:{dest}")),
        "file" => Some(dest.clone()),
        _ => None,
    };
    LinkRecord {
        source,
        dest: (kind == "id").then(|| dest.clone()),
        raw_dest: dest,
        kind,
        ref_target,
    }
}

fn row_to_node_meta(r: &Row<'_>) -> rusqlite::Result<NodeMeta> {
    let id: String = r.get("id")?;
    let file: String = r.get("file")?;
    let title: Option<String> = r.get("title")?;
    let level: Option<i64> = r.get("level")?;
    let pos: Option<i64> = r.get("pos")?;
    let todo: Option<String> = r.get("todo")?;
    let priority: Option<String> = r.get("priority")?;
    let olp: Option<String> = r.get("olp")?;

    // org-roam stores file-level nodes as level 0; we model them as None.
    let level = level
        .and_then(|l| usize::try_from(l).ok())
        .filter(|l| *l > 0);

    Ok(NodeMeta {
        id: emacsql::decode(&id),
        file: PathBuf::from(emacsql::decode(&file)),
        title: title.map(|t| emacsql::decode(&t)).unwrap_or_default(),
        level,
        pos: pos.map(|p| usize::try_from(p).unwrap_or(0)),
        todo: todo.and_then(|t| emacsql::unlisp_string(&t)),
        priority: priority.and_then(|p| emacsql::unlisp_string(&p)),
        olp: olp
            .map(|o| emacsql::unlisp_string_list(&o))
            .unwrap_or_default(),
        aliases: vec![],
        tags: vec![],
    })
}

/// emacsql encoding helpers.
///
/// emacsql prints strings wrapped in double quotes, with embedded
/// characters escaped in a Lisp-ish way. Lists are printed as
/// `("a" "b")` and `nil` is used for null. This module is the single
/// place that knows about that encoding: row mappers decode with
/// [`decode`] / [`unlisp`], and query binds encode with [`quote`].
pub mod emacsql {
    use serde_json::Value;

    /// Encode a Rust string the way emacsql stores it, for use as an
    /// equality bind parameter.
    #[must_use]
    pub fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// Decode a printed Lisp string, falling back to the raw input when it
    /// isn't quote-wrapped (tolerates hand-built or older databases).
    #[must_use]
    pub fn decode(input: &str) -> String {
        unlisp_string(input).unwrap_or_else(|| input.to_string())
    }

    /// Decode a printed Lisp list of strings (e.g. an `olp` column value).
    /// `nil` and malformed input decode to an empty list.
    #[must_use]
    pub fn unlisp_string_list(input: &str) -> Vec<String> {
        match unlisp(input) {
            Value::Array(items) => items
                .into_iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            Value::String(s) => vec![s],
            _ => vec![],
        }
    }

    /// Decode a printed Lisp scalar/sequence into a JSON value.
    #[must_use]
    pub fn unlisp(input: &str) -> Value {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "nil" || trimmed == "()" {
            return Value::Null;
        }
        if let Some(s) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            return Value::String(unescape(s));
        }
        if trimmed.starts_with('(') && trimmed.ends_with(')') {
            let inner = &trimmed[1..trimmed.len() - 1];
            let items = split_top_level(inner);
            return Value::Array(items.into_iter().map(|s| unlisp(&s)).collect());
        }
        Value::String(trimmed.to_string())
    }

    /// Decode a printed Lisp string. Returns `None` if the input doesn't
    /// look like a quoted string.
    #[must_use]
    pub fn unlisp_string(input: &str) -> Option<String> {
        let trimmed = input.trim();
        let inner = trimmed.strip_prefix('"')?.strip_suffix('"')?;
        Some(unescape(inner))
    }

    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') | None => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Split a sequence at top-level whitespace, respecting nested parens and strings.
    fn split_top_level(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = String::new();
        let mut depth: i32 = 0;
        let mut in_str = false;
        let mut escape = false;
        for c in s.chars() {
            if escape {
                buf.push(c);
                escape = false;
                continue;
            }
            match c {
                '\\' if in_str => {
                    buf.push(c);
                    escape = true;
                }
                '"' => {
                    in_str = !in_str;
                    buf.push(c);
                }
                '(' if !in_str => {
                    depth += 1;
                    buf.push(c);
                }
                ')' if !in_str => {
                    depth -= 1;
                    buf.push(c);
                }
                c if c.is_whitespace() && depth == 0 && !in_str => {
                    if !buf.is_empty() {
                        out.push(std::mem::take(&mut buf));
                    }
                }
                c => buf.push(c),
            }
        }
        if !buf.is_empty() {
            out.push(buf);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // --- Synthetic unit tests ---------------------------------------

        #[test]
        fn decodes_simple_string() {
            assert_eq!(unlisp("\"hello\""), Value::String("hello".into()));
        }

        #[test]
        fn decodes_empty_string() {
            assert_eq!(unlisp("\"\""), Value::String(String::new()));
        }

        #[test]
        fn decodes_escapes() {
            assert_eq!(unlisp("\"a\\\"b\\nc\""), Value::String("a\"b\nc".into()));
        }

        #[test]
        fn decodes_nil() {
            assert_eq!(unlisp("nil"), Value::Null);
        }

        #[test]
        fn decodes_paren_list() {
            let v = unlisp("(\"a\" \"b\")");
            assert_eq!(
                v,
                Value::Array(vec![Value::String("a".into()), Value::String("b".into()),])
            );
        }

        #[test]
        fn decodes_nested_list() {
            let v = unlisp("(\"a\" (\"b\" \"c\"))");
            assert_eq!(
                v,
                Value::Array(vec![
                    Value::String("a".into()),
                    Value::Array(vec![Value::String("b".into()), Value::String("c".into()),]),
                ])
            );
        }

        #[test]
        fn decodes_bare_atom() {
            assert_eq!(unlisp("hello"), Value::String("hello".into()));
        }

        #[test]
        fn string_helper_strips_quotes() {
            assert_eq!(unlisp_string("\"x\"").as_deref(), Some("x"));
            // A bare atom is not a string; the helper returns None.
            assert_eq!(unlisp_string("plain"), None);
        }

        // --- quote / decode round-trips ----------------------------------

        #[test]
        fn quote_then_decode_round_trips() {
            for s in [
                "11111111-1111-1111-1111-111111111111",
                "/home/user/notes/file.org",
                "title with \"quotes\" and \\backslash",
                "multi\nline",
                "",
            ] {
                assert_eq!(decode(&quote(s)), s, "round-trip failed for {s:?}");
            }
        }

        #[test]
        fn decode_falls_back_to_raw_for_unquoted_input() {
            assert_eq!(decode("bare-id"), "bare-id");
        }

        #[test]
        fn olp_list_decodes_to_strings() {
            assert_eq!(
                unlisp_string_list("(\"Parent\" \"Child\")"),
                vec!["Parent".to_string(), "Child".to_string()]
            );
            assert!(unlisp_string_list("nil").is_empty());
        }

        // --- Real-world emacsql payloads --------------------------------
        //
        // These are copy-pasted from a real org-roam.db to test the shim
        // against actual emacsql output, not just hand-rolled cases.

        #[test]
        fn real_title_quote_wrapped() {
            // emacsql prints a title as a quoted string.
            assert_eq!(
                unlisp_string("\"Pastafarian Canticle\"").as_deref(),
                Some("Pastafarian Canticle")
            );
            assert_eq!(
                unlisp_string("\"Noodly Appendage imagery\"").as_deref(),
                Some("Noodly Appendage imagery")
            );
        }

        #[test]
        fn real_title_with_embedded_quote() {
            // A title containing a `"` is encoded with `\"`.
            assert_eq!(
                unlisp_string(r#""He said \"hello\"""#).as_deref(),
                Some("He said \"hello\"")
            );
        }

        #[test]
        fn real_alias_list() {
            // aliases are stored as a list of quoted strings.
            let v = unlisp("(\"Ps FSM\" \"The Noodly Psalm\")");
            assert_eq!(
                v,
                Value::Array(vec![
                    Value::String("Ps FSM".into()),
                    Value::String("The Noodly Psalm".into()),
                ])
            );
        }

        #[test]
        fn real_ref_url() {
            // URLs contain colons, slashes, and query strings; emacsql
            // does not escape those. We need to round-trip cleanly.
            assert_eq!(
                unlisp_string(r#""https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster""#)
                    .as_deref(),
                Some("https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster")
            );
        }

        #[test]
        fn real_ref_at_citekey() {
            // @citekeys start with @, which emacsql stores verbatim
            // (the @ is not special Lisp syntax).
            assert_eq!(
                unlisp_string(r#""@nora2023""#).as_deref(),
                Some("@nora2023")
            );
        }

        #[test]
        fn real_tag_list_with_colons() {
            // tags in org are written ":tag1:tag2:"; emacsql stores
            // them with the surrounding colons.
            assert_eq!(
                unlisp_string(r#"":pastafarianism:canticles:""#).as_deref(),
                Some(":pastafarianism:canticles:")
            );
        }

        #[test]
        fn real_path_string() {
            // file paths contain slashes and a drive letter, no escaping.
            assert_eq!(
                unlisp_string(r#""/home/user/notes/20240115120000-fsm-canticle.org""#).as_deref(),
                Some("/home/user/notes/20240115120000-fsm-canticle.org")
            );
        }

        #[test]
        fn lorem_ipsum_in_title() {
            // org-roam doesn't care what a title contains, as long as
            // the emacsql encoding round-trips.
            let lorem = "Lorem ipsum dolor sit amet, consectetur adipiscing elit.";
            let encoded = format!("\"{}\"", lorem.replace('"', "\\\""));
            assert_eq!(unlisp_string(&encoded).as_deref(), Some(lorem));
        }

        #[test]
        fn multiline_escaped_string() {
            let multiline = "line1\nline2\nline3";
            let encoded = format!("\"{}\"", multiline.replace('"', "\\\""));
            assert_eq!(unlisp_string(&encoded).as_deref(), Some(multiline));
        }

        #[test]
        fn nil_means_null_for_optional_columns() {
            // In SQL rows, emacsql prints NULL as the symbol nil.
            // We use Value::Null so the JSON output reads cleanly.
            assert_eq!(unlisp("nil"), Value::Null);
            assert_eq!(unlisp("()"), Value::Null);
        }

        #[test]
        fn split_respects_paren_depth() {
            // split_top_level is internal, but exercised via unlisp.
            // This is the case that breaks naive whitespace splitting.
            let v = unlisp("(\"a (literal)\" \"b\")");
            assert_eq!(
                v,
                Value::Array(vec![
                    Value::String("a (literal)".into()),
                    Value::String("b".into()),
                ])
            );
        }
    }

    #[tokio::test]
    async fn sqlite_nodes_with_external_links() {
        use super::SqliteIndex;
        use crate::index::RoamIndex;
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("org-roam.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id TEXT PRIMARY KEY, file TEXT, title TEXT, level INTEGER, pos INTEGER, todo TEXT, priority TEXT, olp TEXT); \
             CREATE TABLE links (source TEXT, dest TEXT, type TEXT); \
             INSERT INTO nodes (id, file, title, level, pos) VALUES ('\"source-id\"', '\"/tmp/s.org\"', '\"Source\"', 0, 0); \
             INSERT INTO links (source, dest, type) VALUES ('\"source-id\"', '\"https://example.com\"', '\"https\"');"
        ).unwrap();
        drop(conn);

        let index = SqliteIndex::open(&db_path).unwrap();
        let nodes = index.nodes_with_external_links().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0.id, "source-id");
        assert_eq!(nodes[0].1.len(), 1);
        assert_eq!(nodes[0].1[0].kind, "https");
    }

    #[tokio::test]
    async fn sqlite_node_by_path() {
        use super::SqliteIndex;
        use crate::index::RoamIndex;
        use std::path::Path;
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("org-roam.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id TEXT PRIMARY KEY, file TEXT, title TEXT, level INTEGER, pos INTEGER, todo TEXT, priority TEXT, olp TEXT); \
             INSERT INTO nodes (id, file, title, level, pos) VALUES ('\"some-id\"', '\"/tmp/p.org\"', '\"Title\"', 0, 0);"
        ).unwrap();
        drop(conn);

        let index = SqliteIndex::open(&db_path).unwrap();
        let node = index
            .node_by_path(Path::new("/tmp/p.org"))
            .unwrap()
            .expect("node");
        assert_eq!(node.id, "some-id");
    }
}
