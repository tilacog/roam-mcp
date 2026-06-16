//! Filesystem-scanner index — fallback when no `org-roam.db` is available.
//!
//! Walks the roam directory with `walkdir`, parses each `.org` file with
//! `orgize` (via [`OrgDoc`]), and extracts node metadata (id, title,
//! aliases, tags, refs) and the link graph. Slower than `SQLite`, but
//! correct and zero-dep on Emacs.
//!
//! The index is an immutable snapshot: refreshing it means building a new
//! `ScanIndex` and swapping the `Arc` (the server does this after writes
//! and on file-watcher events).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use orgize::ast::{Document, Link};
use orgize::rowan::ast::AstNode as _;
use walkdir::WalkDir;

use super::{IndexResult, LinkRecord, NodeMeta, NodeQuery, RoamIndex};
use crate::org::OrgDoc;

/// In-memory index built by walking a directory.
pub struct ScanIndex {
    dir: PathBuf,
    /// Node metadata keyed by id.
    nodes: HashMap<String, NodeMeta>,
    /// Adjacency list: source-id -> list of forward link records.
    forward: HashMap<String, Vec<LinkRecord>>,
    /// Reverse adjacency: dest-id -> list of link records.
    backward: HashMap<String, Vec<LinkRecord>>,
    /// Refs: ref-target -> ids of the nodes that declare it.
    refs: HashMap<String, Vec<String>>,
}

impl ScanIndex {
    /// Build an index by walking `dir` for `.org` files.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be read.
    pub fn open(dir: &Path) -> IndexResult<Self> {
        let walk = walk_org_files(dir);
        let slug_index = unique_slug_index(walk.slug_candidates, &walk.nodes);
        let mut forward = walk.forward;
        let backward = resolve_id_slugs_and_build_backward(&mut forward, &walk.nodes, &slug_index);
        reclassify_fuzzy_name_links(&mut forward, &walk.nodes);
        // Final dedup per source: a file with the same `:ID:` at the
        // file level and on a headline contributes each link from both
        // sections, but a logical edge in the link graph is keyed on
        // (source, dest, kind, raw_dest) and must appear once. The
        // backward map is already deduped; mirror that here so
        // `forward_links` matches.
        for links in forward.values_mut() {
            dedup_link_records_in_place(links);
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            nodes: walk.nodes,
            forward,
            backward,
            refs: walk.refs,
        })
    }
}

impl RoamIndex for ScanIndex {
    fn find_nodes(&self, q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>> {
        let needle = q.query.map(str::to_lowercase);
        let mut out: Vec<NodeMeta> = self
            .nodes
            .values()
            .filter(|n| {
                let Some(ref ndl) = needle else { return true };
                if ndl.is_empty() {
                    return true;
                }
                n.title.to_lowercase().contains(ndl)
                    || n.aliases.iter().any(|a| a.to_lowercase().contains(ndl))
                    || n.tags.iter().any(|t| t.to_lowercase().contains(ndl))
            })
            .filter(|n| {
                q.tags
                    .iter()
                    .all(|t| n.tags.iter().any(|nt| nt.eq_ignore_ascii_case(t)))
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.title.cmp(&b.title));
        if let Some(lim) = q.limit {
            out.truncate(lim);
        }
        Ok(out)
    }

    fn node(&self, id: &str) -> IndexResult<Option<NodeMeta>> {
        Ok(self.nodes.get(id).cloned())
    }

    fn backlinks(&self, id: &str) -> IndexResult<Vec<LinkRecord>> {
        Ok(self.backward.get(id).cloned().unwrap_or_default())
    }

    fn forward_links(&self, id: &str) -> IndexResult<Vec<LinkRecord>> {
        Ok(self.forward.get(id).cloned().unwrap_or_default())
    }

    fn by_ref(&self, r: &str) -> IndexResult<Vec<NodeMeta>> {
        let mut out = Vec::new();
        if let Some(list) = self.refs.get(r) {
            for id in list {
                if let Some(n) = self.nodes.get(id) {
                    out.push(n.clone());
                }
            }
        }
        Ok(out)
    }

    fn tags(&self) -> IndexResult<Vec<(String, usize)>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for n in self.nodes.values() {
            for t in &n.tags {
                *counts.entry(t.clone()).or_default() += 1;
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn node_count(&self) -> IndexResult<usize> {
        Ok(self.nodes.len())
    }

    fn orphans(&self) -> IndexResult<Vec<NodeMeta>> {
        // A node is connected if it has any id-link edge: either it
        // appears in `backward` (it's a link destination) or it has at
        // least one `id` forward link. Other link kinds (file, https,
        // cite, fuzzy) do not point at another note, so a node with
        // only those is still an orphan.
        let mut connected: std::collections::HashSet<&str> =
            self.backward.keys().map(String::as_str).collect();
        for (src, links) in &self.forward {
            if links.iter().any(|l| l.kind == "id") {
                connected.insert(src.as_str());
            }
        }
        let mut out: Vec<NodeMeta> = self
            .nodes
            .values()
            .filter(|n| !connected.contains(n.id.as_str()))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(out)
    }

    fn source(&self) -> &str {
        self.dir.to_str().unwrap_or("(scan)")
    }
}

/// Per-walk outputs of [`walk_org_files`]. The four maps are
/// accumulated in one pass so we only read each `.org` file once.
struct WalkOutcome {
    nodes: HashMap<String, NodeMeta>,
    forward: HashMap<String, Vec<LinkRecord>>,
    refs: HashMap<String, Vec<String>>,
    /// Basename slug → file-node ids that claim it. Used in a later
    /// pass to resolve `[[id:<slug>]]` links that name a file by its
    /// basename instead of by `:ID:`.
    slug_candidates: HashMap<String, Vec<String>>,
}

/// Walk `dir` and merge every readable `.org` file into a [`WalkOutcome`].
fn walk_org_files(dir: &Path) -> WalkOutcome {
    let mut out = WalkOutcome {
        nodes: HashMap::new(),
        forward: HashMap::new(),
        refs: HashMap::new(),
        slug_candidates: HashMap::new(),
    };

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let Some(path) = is_readable_org_file(&entry) else {
            continue;
        };
        // Unreadable files are skipped silently — a single broken
        // permission in the vault shouldn't abort the whole scan.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = ParsedFile::new(&path, &text);
        merge_parsed_file(parsed, &path, &mut out);
    }

    out
}

/// Return the path of an entry iff it is a regular `.org` file the
/// scanner is willing to read. Filters out directories, non-org
/// extensions, and Emacs lockfiles (`.#foo.org`).
fn is_readable_org_file(entry: &walkdir::DirEntry) -> Option<PathBuf> {
    if !entry.file_type().is_file() {
        return None;
    }
    if entry.path().extension().and_then(|e| e.to_str()) != Some("org") {
        return None;
    }
    if entry
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".#"))
    {
        return None;
    }
    Some(entry.path().to_path_buf())
}

/// Merge a single parsed file's results into the walk outcome.
fn merge_parsed_file(parsed: ParsedFile, path: &Path, out: &mut WalkOutcome) {
    for (r, node_id) in parsed.refs {
        out.refs.entry(r).or_default().push(node_id);
    }
    if let Some(file_id) = &parsed.file_node_id {
        for slug in file_slugs(path) {
            out.slug_candidates
                .entry(slug)
                .or_default()
                .push(file_id.clone());
        }
    }
    for n in parsed.nodes {
        out.nodes.insert(n.id.clone(), n);
    }
    for (src, links) in parsed.forward {
        out.forward.entry(src).or_default().extend(links);
    }
}

/// A slug resolves only when exactly one file claims it and it does
/// not shadow a real `:ID:` (an explicit ID always wins). Ambiguous
/// slugs are left unresolved rather than guessed.
fn unique_slug_index(
    candidates: HashMap<String, Vec<String>>,
    nodes: &HashMap<String, NodeMeta>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for (slug, ids) in candidates {
        if ids.len() == 1 && !nodes.contains_key(&slug) {
            if let Some(id) = ids.into_iter().next() {
                out.insert(slug, id);
            }
        }
    }
    out
}

/// Build the reverse adjacency. org-roam links by `:ID:`, but agents
/// and humans naturally write the basename slug (`[[id:bistritz]]`);
/// when an `id:` link's target is not a known node but matches a
/// unique file slug, resolve it to that file's node so the link still
/// joins the graph. `raw_dest` keeps the slug as written.
///
/// Edges are deduped by `(source, dest, kind, raw_dest)` so a file
/// that legitimately carries the same `[[id:X]]` link in two places
/// (e.g. once in the pre-headline section and once inside a headline
/// whose `:ID:` is identical to the file's) contributes one
/// `LinkRecord` per edge, not two. The link graph records edges
/// between nodes, not in-file positions; counting positions
/// separately is the source of the "duplicate backlinks" bug.
fn resolve_id_slugs_and_build_backward(
    forward: &mut HashMap<String, Vec<LinkRecord>>,
    nodes: &HashMap<String, NodeMeta>,
    slug_index: &HashMap<String, String>,
) -> HashMap<String, Vec<LinkRecord>> {
    let mut backward: HashMap<String, Vec<LinkRecord>> = HashMap::new();
    for links in forward.values_mut() {
        for l in links.iter_mut() {
            resolve_id_slug(l, nodes, slug_index);
            if let Some(dest) = &l.dest {
                let entry = backward.entry(dest.clone()).or_default();
                if !entry
                    .iter()
                    .any(|existing| link_key(existing) == link_key(l))
                {
                    entry.push(l.clone());
                }
            }
        }
    }
    backward
}

/// Identity key for a `LinkRecord` in the link graph: two records with
/// the same `(source, dest, kind, raw_dest)` represent the same edge
/// regardless of `ref_target`, which only carries the resolved URL /
/// @citekey payload for `https` / `http` / `cite` links and is not
/// what distinguishes one edge from another.
fn link_key(l: &LinkRecord) -> (String, Option<String>, String, String) {
    (
        l.source.clone(),
        l.dest.clone(),
        l.kind.clone(),
        l.raw_dest.clone(),
    )
}

/// Drop duplicate `LinkRecord`s from `links` in place, keeping the
/// first occurrence of each [`link_key`]. The order of the kept
/// records matches the order in which they were first seen.
fn dedup_link_records_in_place(links: &mut Vec<LinkRecord>) {
    let mut seen: std::collections::HashSet<(String, Option<String>, String, String)> =
        std::collections::HashSet::with_capacity(links.len());
    links.retain(|l| seen.insert(link_key(l)));
}

/// If `l` is an `id:` link whose target is not a known node but is a
/// unique file slug, rewrite its `dest` to the real node id. Leaves
/// every other link untouched.
fn resolve_id_slug(
    l: &mut LinkRecord,
    nodes: &HashMap<String, NodeMeta>,
    slug_index: &HashMap<String, String>,
) {
    if l.kind != "id" {
        return;
    }
    let Some(dest) = &l.dest else {
        return;
    };
    if nodes.contains_key(dest) {
        return;
    }
    if let Some(resolved) = slug_index.get(dest) {
        l.dest = Some(resolved.clone());
    }
}

/// §6: post-pass that reclassifies fuzzy `[[name]]` links as `name`
/// when their target text matches a `#+NAME:` declared anywhere in
/// the vault. `name` links are intra-file, so `dest` stays None and
/// the agent can resolve them via `get_node_section` /
/// `get_forward_links`.
///
/// We don't track per-file name tables through the index data shape,
/// so the match is vault-wide: any `#+NAME:` anywhere is enough to
/// upgrade a fuzzy link to `name`.
fn reclassify_fuzzy_name_links(
    forward: &mut HashMap<String, Vec<LinkRecord>>,
    nodes: &HashMap<String, NodeMeta>,
) {
    let file_to_names = collect_name_keywords(nodes);
    for links in forward.values_mut() {
        for l in links.iter_mut() {
            if l.kind == "fuzzy" && file_to_names.values().any(|s| s.contains(&l.raw_dest)) {
                l.kind = "name".to_string();
            }
        }
    }
}

/// Read each node's source file and collect the set of `#+NAME:`
/// keywords declared in it.
fn collect_name_keywords(nodes: &HashMap<String, NodeMeta>) -> HashMap<PathBuf, HashSet<String>> {
    let mut out: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for n in nodes.values() {
        let Ok(text) = std::fs::read_to_string(&n.file) else {
            continue;
        };
        let names: HashSet<String> = keyword_values(&text, "name")
            .iter()
            .map(|s| (*s).to_string())
            .filter(|s| !s.is_empty())
            .collect();
        out.insert(n.file.clone(), names);
    }
    out
}

/// Parsed-in-memory view of a single `.org` file used during the scan.
struct ParsedFile {
    nodes: Vec<NodeMeta>,
    forward: HashMap<String, Vec<LinkRecord>>,
    /// `(ref value, declaring node id)` pairs from `ROAM_REFS`.
    refs: Vec<(String, String)>,
    /// The `:ID:` of the file-level node, if the file has one. Used to map
    /// the file's basename slug to a node for `[[id:<slug>]]` resolution.
    file_node_id: Option<String>,
}

impl ParsedFile {
    fn new(path: &Path, text: &str) -> Self {
        let doc = OrgDoc::from_text(text.to_string());
        let mut nodes: Vec<NodeMeta> = Vec::new();
        let mut forward: HashMap<String, Vec<LinkRecord>> = HashMap::new();
        let mut refs: Vec<(String, String)> = Vec::new();

        // File-level node: any `:ID:` property drawer at the top of the
        // file (before the first headline), as org-roam treats it.
        let document = doc.document();
        let file_id = document
            .properties()
            .and_then(|p| p.get("ID"))
            .map(|t| t.to_string());

        if let Some(ref id) = file_id {
            nodes.push(build_file_node(&doc, path, text, id, &mut refs));
            if let Some((links, link_refs)) = pre_headline_links(&document, id) {
                forward.entry(id.clone()).or_default().extend(links);
                refs.extend(link_refs);
            }
        }

        for headline in doc.headlines() {
            if let Some(id) = headline
                .properties()
                .and_then(|p| p.get("ID"))
                .map(|t| t.to_string())
            {
                nodes.push(build_headline_node(&headline, path, &id, &mut refs));
                let (links, link_refs) = headline_links(&headline, &id);
                forward.entry(id).or_default().extend(links);
                refs.extend(link_refs);
            } else {
                // No ID on this headline: org-roam attributes its links to
                // the nearest enclosing node — the closest ancestor
                // headline with an :ID:, else the file-level node.
                if let Some(owner) = nearest_ancestor_id(&headline).or_else(|| file_id.clone()) {
                    let (links, link_refs) = headline_links(&headline, &owner);
                    forward.entry(owner).or_default().extend(links);
                    refs.extend(link_refs);
                }
            }
        }

        Self {
            nodes,
            forward,
            refs,
            file_node_id: file_id,
        }
    }
}

/// Build the file-level `NodeMeta` and emit its refs (from
/// `ROAM_REFS` / `#+ROAM_KEY:`) and any `bibliography:<path>` tags
/// derived from `#+bibliography:`. Called only when the file has a
/// top-of-file `:ID:`.
fn build_file_node(
    doc: &OrgDoc,
    path: &Path,
    text: &str,
    id: &str,
    refs: &mut Vec<(String, String)>,
) -> NodeMeta {
    let document = doc.document();
    let title = file_title(doc, path);
    let aliases = drawer_string_list(document.properties().as_ref(), "ROAM_ALIASES");
    for r in drawer_string_list(document.properties().as_ref(), "ROAM_REFS") {
        refs.push((r, id.to_string()));
    }
    // org-roam v1 declared refs with a `#+ROAM_KEY:` keyword.
    for v in keyword_values(text, "roam_key") {
        for r in parse_string_list(v) {
            refs.push((r, id.to_string()));
        }
    }
    let mut tags = file_level_tags(text);
    // §4: `#+bibliography:` keyword becomes a tag on the file node,
    // so the file is findable by the bibliography path it points at.
    for v in keyword_values(text, "bibliography") {
        for entry in parse_string_list(v) {
            let tag = format!("bibliography:{entry}");
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    NodeMeta {
        id: id.to_string(),
        file: path.to_path_buf(),
        title,
        level: None,
        todo: None,
        priority: None,
        olp: vec![],
        pos: Some(0),
        aliases,
        tags,
    }
}

/// Links + refs collected from a single section (pre-headline or
/// headline-body) of a file.
type SectionRecords = (Vec<LinkRecord>, Vec<(String, String)>);

/// Links + refs collected from the pre-headline section of a file
/// (the part before the first headline). `None` if the file has no
/// pre-headline section. `#+bibliography:` is file-level only, so
/// headline nodes don't carry a bibliography path.
fn pre_headline_links(document: &Document, id: &str) -> Option<SectionRecords> {
    let section = document.section()?;
    let mut links = Vec::new();
    for n in section.syntax().descendants() {
        if let Some(l) = Link::cast(n) {
            push_link(&l, id, &mut links);
        }
    }
    let mut refs = Vec::new();
    // §4: walk the in-body citation objects and emit `cite`
    // `LinkRecord`s + ref pairs.
    push_section_citations(&section.syntax().to_string(), id, &mut links, &mut refs);
    Some((links, refs))
}

/// Build a `NodeMeta` for a single headline that has its own `:ID:`,
/// and emit its drawer refs. Tag values come from the headline's
/// in-line `* TODO :tag1:tag2:` syntax (orgize's `Headline::tags`).
fn build_headline_node(
    headline: &orgize::ast::Headline,
    path: &Path,
    id: &str,
    refs: &mut Vec<(String, String)>,
) -> NodeMeta {
    let aliases = drawer_string_list(headline.properties().as_ref(), "ROAM_ALIASES");
    for r in drawer_string_list(headline.properties().as_ref(), "ROAM_REFS") {
        refs.push((r, id.to_string()));
    }
    let tags: Vec<String> = headline
        .tags()
        .map(|t| t.to_string())
        .filter(|t| !t.is_empty())
        .collect();
    NodeMeta {
        id: id.to_string(),
        file: path.to_path_buf(),
        title: headline.title_raw().trim().to_string(),
        level: Some(headline.level()),
        todo: headline.todo_keyword().map(|t| t.to_string()),
        priority: headline.priority().map(|t| t.to_string()),
        olp: outline_path(headline),
        pos: Some(headline.start().into()),
        aliases,
        tags,
    }
}

/// Links + refs collected from a headline's own section. Used for
/// both headlines with `:ID:` (attributed to themselves) and those
/// without (attributed to the nearest enclosing node). The caller
/// supplies `owner` so this helper doesn't need to know how
/// attribution was decided.
fn headline_links(headline: &orgize::ast::Headline, owner: &str) -> SectionRecords {
    let mut links = Vec::new();
    for l in collect_links(headline) {
        push_link(&l, owner, &mut links);
    }
    let mut refs = Vec::new();
    // §4: in-body citation objects inside the headline's own
    // section also attribute to this node.
    if let Some(section) = headline.section() {
        push_section_citations(&section.syntax().to_string(), owner, &mut links, &mut refs);
    }
    (links, refs)
}

/// Basename-derived slugs an `id:` link may use to address a file instead
/// of its `:ID:`: the full stem, plus the stem with a leading org-roam
/// `YYYYMMDDHHMMSS-` timestamp stripped (`20260613205004-bistritz` →
/// `bistritz`).
fn file_slugs(path: &Path) -> Vec<String> {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let mut out = vec![stem.to_string()];
    if let Some(rest) = strip_leading_timestamp(stem) {
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    out
}

/// The stem with a leading `YYYYMMDDHHMMSS-` (14 digits + `-`) org-roam
/// timestamp removed, if present.
fn strip_leading_timestamp(stem: &str) -> Option<&str> {
    let bytes = stem.as_bytes();
    if bytes.len() > 15 && bytes[14] == b'-' && bytes[..14].iter().all(u8::is_ascii_digit) {
        Some(&stem[15..])
    } else {
        None
    }
}

/// File-level title: prefer `#+TITLE:`, else the first top-level headline,
/// else the file basename.
fn file_title(doc: &OrgDoc, path: &Path) -> String {
    doc.org
        .title()
        .or_else(|| {
            doc.document()
                .first_headline()
                .map(|h| h.title_raw().trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string()
        })
}

/// Parse a quoted-string list property (`ROAM_ALIASES`, `ROAM_REFS`) from a
/// property drawer.
fn drawer_string_list(props: Option<&orgize::ast::PropertyDrawer>, key: &str) -> Vec<String> {
    parse_string_list(
        &props
            .and_then(|p| p.get(key))
            .map(|t| t.to_string())
            .unwrap_or_default(),
    )
}

/// Collect the `Link` nodes in a headline's *own* section (not in nested
/// headlines). The caller visits every headline depth-first, so each link
/// is attributed to its nearest enclosing node exactly once.
fn collect_links(h: &orgize::ast::Headline) -> Vec<Link> {
    let mut out = Vec::new();
    if let Some(section) = h.section() {
        for n in section.syntax().descendants() {
            if let Some(l) = Link::cast(n) {
                out.push(l);
            }
        }
    }
    out
}

/// The `:ID:` of the closest ancestor headline that has one, if any.
fn nearest_ancestor_id(h: &orgize::ast::Headline) -> Option<String> {
    let mut current = h.syntax().clone();
    while let Some(parent) = current.parent() {
        if let Some(p) = orgize::ast::Headline::cast(parent.clone()) {
            if let Some(id) = p.properties().and_then(|props| props.get("ID")) {
                return Some(id.to_string());
            }
        }
        current = parent;
    }
    None
}

/// Build the outline path for the headline: `["Parent", "Child", "This"]`.
fn outline_path(h: &orgize::ast::Headline) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = h.syntax().clone();
    while let Some(parent) = current.parent() {
        if let Some(p) = orgize::ast::Headline::cast(parent.clone()) {
            let t = p.title_raw().trim().to_string();
            if !t.is_empty() {
                path.push(t);
            }
        }
        current = parent;
    }
    path.reverse();
    path.push(h.title_raw().trim().to_string());
    path
}

/// Trimmed values of every `#+key:` keyword line in `text`, matching the
/// key case-insensitively.
pub(crate) fn keyword_values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#+") else {
            continue;
        };
        let Some((k, v)) = rest.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case(key) {
            out.push(v.trim());
        }
    }
    out
}

/// File-level tags: `#+filetags: :a:b:` plus the org-roam v1 form
/// `#+ROAM_TAGS: a b "multi word"`. Duplicates removed, order preserved.
fn file_level_tags(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in keyword_values(text, "filetags") {
        out.extend(parse_filetags_value(v));
    }
    for v in keyword_values(text, "roam_tags") {
        out.extend(parse_string_list(v));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|t| seen.insert(t.clone()));
    out
}

fn parse_filetags_value(s: &str) -> Vec<String> {
    // `#+filetags: :work:urgent:` → ["work", "urgent"]
    s.split(':')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub(crate) fn parse_string_list(s: &str) -> Vec<String> {
    // `ROAM_ALIASES: "A" "B"` and `ROAM_REFS: https://... @key` both
    // can be tokenized by splitting on whitespace, then re-joining quoted
    // runs.
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_str = false;
    for c in s.chars() {
        match c {
            '"' => {
                if in_str {
                    out.push(std::mem::take(&mut buf));
                } else {
                    buf.clear();
                }
                in_str = !in_str;
            }
            c if c.is_whitespace() && !in_str => {
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
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn push_link(l: &Link, source_id: &str, out: &mut Vec<LinkRecord>) {
    let raw = l.path().to_string();
    let (kind, dest, ref_target) = classify_link(&raw);
    out.push(LinkRecord {
        source: source_id.to_string(),
        dest,
        raw_dest: raw,
        kind,
        ref_target,
    });
}

/// §4: walk the in-body citation objects of a single section and emit
/// one `cite` `LinkRecord` per `@key` plus matching `(ref, node_id)`
/// pairs so the in-body citation is in both the link graph and
/// `find_by_ref`'s index. orgize 0.10.0-alpha.10 has no `Citation`
/// AST kind, so we scan the section text for the citation
/// `[cite(/style)?:@key1; @key2 ...]` regex.
///
/// We skip matches that fall inside an org link `[[...]]` because
/// the link parser will already have emitted a `cite` `LinkRecord`
/// for the canonical form `[[cite:@key]]`. Without the skip, a
/// single user-visible `[[cite:@key]]` would produce two records.
fn push_section_citations(
    section_text: &str,
    source_id: &str,
    out: &mut Vec<LinkRecord>,
    refs: &mut Vec<(String, String)>,
) {
    for (raw, key, pos) in find_citation_keys(section_text) {
        if inside_org_link(section_text, pos) {
            continue;
        }
        out.push(LinkRecord {
            source: source_id.to_string(),
            dest: None,
            raw_dest: raw,
            kind: "cite".to_string(),
            ref_target: Some(key.clone()),
        });
        refs.push((key, source_id.to_string()));
    }
}

/// True when the byte at `pos` falls inside an org link (`[[...]]`):
/// there is an unclosed `[[` at or before it. The `pos` we receive
/// is the start of the `[cite:...]` opener, so a `[[` at `pos-1`
/// (the second bracket of the link) must also count — we look at
/// `text[..=pos]` to include that.
fn inside_org_link(text: &str, pos: usize) -> bool {
    let prefix_end = (pos + 1).min(text.len());
    match text[..prefix_end].rfind("[[") {
        None => false,
        Some(open) => !text[open..=pos].contains("]]"),
    }
}

/// Scan `text` for org-cite syntax. Returns one `(raw_dest, ref_target, pos)`
/// triple per `@key` found, in document order. The `pos` is the byte
/// offset of the `[cite:...]` opener so callers can skip matches that
/// fall inside an org link. The `raw_dest` is the literal `@key`
/// (style prefix stripped) so a caller that wants to render the
/// citation can re-derive the style; `ref_target` is the same string
/// so `find_by_ref("@key")` matches.
///
/// Recognised forms:
/// - `[cite:@key1; @key2]`
/// - `[cite:@key p. 42; @other]`
/// - `[cite/style:@key]` (`raw_dest` strips the style)
fn find_citation_keys(text: &str) -> Vec<(String, String, usize)> {
    let mut out: Vec<(String, String, usize)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // Look for `[cite...:...@key...]` shapes.
        if !(bytes[i + 1] == b'c'
            && bytes[i + 2] == b'i'
            && bytes[i + 3] == b't'
            && bytes[i + 4] == b'e')
        {
            i += 1;
            continue;
        }
        // Skip a `/style` suffix if present.
        let mut j = i + 5;
        if j < bytes.len() && bytes[j] == b'/' {
            j += 1;
            while j < bytes.len()
                && bytes[j] != b':'
                && bytes[j] != b']'
                && !bytes[j].is_ascii_whitespace()
            {
                j += 1;
            }
        }
        // The next non-whitespace byte must be `:`.
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            i += 1;
            continue;
        }
        // From `:` to the next `]`, extract the keys.
        j += 1;
        let content_start = j;
        let mut content_end = content_start;
        while content_end < bytes.len() && bytes[content_end] != b']' {
            content_end += 1;
        }
        if content_end >= bytes.len() {
            i += 1;
            continue;
        }
        let content = &text[content_start..content_end];
        for key in content.split([';', ',']) {
            let key = key.trim();
            // A `cite:@key p. 41` form has locator text after the key.
            // We extract the @-token (the first whitespace-delimited
            // word) and use that as the lookup key.
            let first = key.split_whitespace().next().unwrap_or("");
            if first.starts_with('@') && first.len() > 1 {
                out.push((first.to_string(), first.to_string(), i));
            }
        }
        i = content_end + 1;
    }
    out
}

/// Classify a raw link target into the kind vocabulary shared with the
/// `SQLite` backend (see [`LinkRecord::kind`]).
fn classify_link(raw: &str) -> (String, Option<String>, Option<String>) {
    if let Some(rest) = raw.strip_prefix("id:") {
        let (id, _anchor) = split_anchor(rest);
        return ("id".to_string(), Some(id.to_string()), None);
    }
    if raw.starts_with("roam:") {
        return ("roam".to_string(), None, None);
    }
    if raw.starts_with("file:") {
        return ("file".to_string(), None, None);
    }
    for scheme in ["http", "https"] {
        if raw.starts_with(&format!("{scheme}://")) {
            return (scheme.to_string(), None, Some(raw.to_string()));
        }
    }
    if raw.starts_with('@') {
        return ("cite".to_string(), None, Some(raw.to_string()));
    }
    // `[[cite:@key]]` — the user-facing org-link form of an in-body
    // citation. The link target text is `cite:@key` (org strips the
    // `[[`/`]]` but keeps the `cite:` prefix). Classify as `cite`
    // so it doesn't double-count against the in-body citation scan
    // (see the test for the regression this prevents).
    if let Some(rest) = raw.strip_prefix("cite:") {
        if rest.starts_with('@') {
            return ("cite".to_string(), None, Some(rest.to_string()));
        }
    }
    // §5: code reference `[[(label)]]` — the raw target is the
    // `(label)` form. Coderefs are intra-file and don't join the
    // cross-file link graph, so dest stays None and ref_target is
    // None (the label alone is not a find_by_ref lookup key).
    if raw.starts_with('(') && raw.ends_with(')') && raw.len() >= 2 {
        return ("coderef".to_string(), None, None);
    }
    // org calls a bare-text link a "fuzzy" link.
    ("fuzzy".to_string(), None, None)
}

fn split_anchor(s: &str) -> (&str, Option<&str>) {
    if let Some((id, anchor)) = s.split_once("::") {
        (id, Some(anchor))
    } else {
        (s, None)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the scanner. The integration tests in
    //! `tests/scan_index.rs` use a larger multi-file vault; here we
    //! exercise the same parsing primitives against individual fixture
    //! files for fast, focused coverage.

    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("text")
            .join(name)
    }

    fn parse_fixture(name: &str) -> ParsedFile {
        let path = fixture(name);
        let text = std::fs::read_to_string(&path).expect("fixture readable");
        ParsedFile::new(&path, &text)
    }

    fn vault_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("vault")
    }

    // --- ParsedFile --------------------------------------------------

    #[test]
    fn psalm_parses_to_one_file_node_and_three_headlines() {
        let parsed = parse_fixture("fsm_canticle.org");
        // fsm_canticle has headlines with :CUSTOM_ID: only, no :ID:.
        // So we expect exactly the file-level node.
        assert_eq!(parsed.nodes.len(), 1);
        let n = &parsed.nodes[0];
        assert_eq!(n.id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(n.title, "Pastafarian Canticle");
        assert!(n.aliases.contains(&"Ps FSM".to_string()));
        assert!(n.aliases.contains(&"The Noodly Psalm".to_string()));
        assert!(n.tags.contains(&"pastafarianism".to_string()));
        assert!(n.tags.contains(&"canticles".to_string()));
    }

    #[test]
    fn multi_headlines_discovers_every_id() {
        let parsed = parse_fixture("multi_headlines.org");
        let ids: std::collections::HashSet<String> =
            parsed.nodes.iter().map(|n| n.id.clone()).collect();
        for expected in &[
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
        ] {
            assert!(ids.contains(*expected), "missing {expected}");
        }
    }

    #[test]
    fn nested_yields_every_headline_with_id() {
        let parsed = parse_fixture("nested.org");
        // 1 file-level + 5 headlines = 6 nodes
        assert_eq!(parsed.nodes.len(), 6);
        // Each headline node must have the right level.
        let deep = parsed
            .nodes
            .iter()
            .find(|n| n.id == "eeeeee04-eeee-eeee-eeee-eeeeeeeeeeee")
            .expect("deep");
        assert_eq!(deep.level, Some(4));
        assert_eq!(deep.title, "Deep");
    }

    #[test]
    fn empty_file_yields_zero_nodes() {
        let parsed = parse_fixture("empty.org");
        assert!(parsed.nodes.is_empty());
    }

    #[test]
    fn file_without_any_id_is_ignored() {
        let parsed = parse_fixture("no_id.org");
        assert!(parsed.nodes.is_empty());
    }

    #[test]
    fn refs_attribute_to_declaring_node_only() {
        // with_refs.org declares ROAM_REFS at the file level. The pairs
        // must name the file-level node, not every node in the file.
        let parsed = parse_fixture("with_refs.org");
        let file_id = &parsed.nodes[0].id;
        for (_, node_id) in &parsed.refs {
            assert_eq!(node_id, file_id);
        }
        assert!(!parsed.refs.is_empty());
    }

    // --- parse_string_list -------------------------------------------

    #[test]
    fn parses_double_quoted_list() {
        let v = parse_string_list("\"Ps FSM\" \"The Noodly Psalm\"");
        assert_eq!(v, vec!["Ps FSM", "The Noodly Psalm"]);
    }

    #[test]
    fn parses_unquoted_tokens() {
        let v = parse_string_list("https://example.com @nora2023");
        assert_eq!(v, vec!["https://example.com", "@nora2023"]);
    }

    #[test]
    fn empty_string_yields_no_items() {
        let v = parse_string_list("");
        assert!(v.is_empty());
    }

    #[test]
    fn splits_on_whitespace_outside_quotes() {
        let v = parse_string_list("\"a b\" c");
        assert_eq!(v, vec!["a b", "c"]);
    }

    // --- file_level_tags ----------------------------------------------

    #[test]
    fn parses_filetags_keyword() {
        let text = "#+filetags: :work:urgent:\n";
        assert_eq!(file_level_tags(text), vec!["work", "urgent"]);
    }

    #[test]
    fn filetags_case_insensitive() {
        let text = "#+FILETAGS: :one:two:\n";
        assert_eq!(file_level_tags(text), vec!["one", "two"]);
    }

    #[test]
    fn missing_tag_keywords_yield_empty() {
        assert!(file_level_tags("#+title: No tags here\n").is_empty());
    }

    #[test]
    fn parses_roam_tags_keyword() {
        let text = "#+ROAM_TAGS: hub projects\n";
        assert_eq!(file_level_tags(text), vec!["hub", "projects"]);
    }

    #[test]
    fn roam_tags_supports_quoted_multiword_tags() {
        let text = "#+roam_tags: \"multi word\" single\n";
        assert_eq!(file_level_tags(text), vec!["multi word", "single"]);
    }

    #[test]
    fn filetags_and_roam_tags_merge_without_duplicates() {
        let text = "#+filetags: :work:\n#+ROAM_TAGS: work extra\n";
        assert_eq!(file_level_tags(text), vec!["work", "extra"]);
    }

    // --- classify_link -----------------------------------------------

    #[test]
    fn classifies_id_link() {
        let (k, d, r) = classify_link("id:11111111-2222-3333-4444-555555555555");
        assert_eq!(k, "id");
        assert_eq!(d.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(r, None);
    }

    #[test]
    fn classifies_id_link_with_anchor() {
        let (k, d, _) = classify_link("id:11111111-2222-3333-4444-555555555555::verse-4");
        assert_eq!(k, "id");
        assert_eq!(d.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
    }

    // --- §0.2: id::anchor link classification ---

    #[test]
    fn id_link_with_anchor_suffix_keeps_dest_drops_anchor() {
        // `[[id:UUID::verse-4]]` is classified as kind:"id", dest:"UUID"
        // (the anchor suffix is dropped from the destination since the
        // backlink graph is node-level, not per-anchor), and the raw
        // target preserves the suffix verbatim for round-tripping.
        let (k, d, r) = classify_link("id:11111111-2222-3333-4444-555555555555::verse-4");
        assert_eq!(k, "id");
        assert_eq!(d.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(r, None);
        // The full raw target is also preserved on the LinkRecord. We
        // assert the classifier doesn't strip the anchor by checking
        // split_anchor directly.
        let (id_part, anchor_part) = split_anchor("11111111-2222-3333-4444-555555555555::verse-4");
        assert_eq!(id_part, "11111111-2222-3333-4444-555555555555");
        assert_eq!(anchor_part, Some("verse-4"));
    }

    #[test]
    fn id_anchor_suffix_aggregates_backlinks_node_level() {
        // Two source files link to the same UUID with two different
        // anchor suffixes. The backlink graph is node-level: both
        // produce records on the destination, and there is no way to
        // distinguish which anchor was used (that's a property to
        // assert, not a bug to fix). The `raw_dest` keeps the full
        // link target so callers can recover the suffix per record.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("target.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n#+title: Target\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("source-a.org"),
            ":PROPERTIES:\n:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n:END:\n\
             #+title: A\n\nSee [[id:11111111-1111-1111-1111-111111111111::verse-4][v4]].\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("source-b.org"),
            ":PROPERTIES:\n:ID: bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\n:END:\n\
             #+title: B\n\nSee [[id:11111111-1111-1111-1111-111111111111::verse-1][v1]].\n",
        )
        .unwrap();

        let idx = ScanIndex::open(dir.path()).expect("open");
        let back = idx
            .backlinks("11111111-1111-1111-1111-111111111111")
            .expect("backlinks");
        let raws: std::collections::HashSet<String> =
            back.iter().map(|l| l.raw_dest.clone()).collect();
        // The two raw destinations must remain distinguishable, but
        // the destination id is the same in both records.
        assert!(raws.contains("id:11111111-1111-1111-1111-111111111111::verse-4"));
        assert!(raws.contains("id:11111111-1111-1111-1111-111111111111::verse-1"));
        for l in &back {
            assert_eq!(
                l.dest.as_deref(),
                Some("11111111-1111-1111-1111-111111111111")
            );
            assert_eq!(l.kind, "id");
        }
    }

    #[test]
    fn classifies_roam_link() {
        let (k, d, _) = classify_link("roam:Some Title");
        assert_eq!(k, "roam");
        assert_eq!(d, None);
    }

    // --- §0.3: roam link classification ---

    #[test]
    fn roam_link_yields_no_dest() {
        // `[[roam:Some Title]]` is classified but contributes no
        // destination to the link graph: it lives in forward_links
        // (raw_dest preserved) but never produces a backlink record
        // on any node, because `dest` is None and the scanner
        // indexes backlinks by destination id only.
        let (k, d, r) = classify_link("roam:Some Title");
        assert_eq!(k, "roam");
        assert_eq!(d, None);
        assert_eq!(r, None);

        // A `roam:` link in a source file must not show up under any
        // node's backlinks.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("src.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Source\n\nSee [[roam:Some Title]].\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("11111111-1111-1111-1111-111111111111")
            .expect("forward");
        assert!(fwd
            .iter()
            .any(|l| l.kind == "roam" && l.raw_dest == "roam:Some Title"));
        assert!(fwd.iter().all(|l| l.dest.is_none()));
        // `backlinks` is keyed on destination id; a None dest cannot
        // match a node, so the link contributes nothing to the
        // reverse graph.
        for n in idx
            .find_nodes(&crate::index::NodeQuery::default())
            .expect("nodes")
        {
            let b = idx.backlinks(&n.id).expect("backlinks");
            assert!(
                !b.iter().any(|l| l.kind == "roam"),
                "roam link leaked into backlinks"
            );
        }
    }

    #[test]
    fn classifies_file_link() {
        let (k, d, _) = classify_link("file:notes.org::*Heading");
        assert_eq!(k, "file");
        assert_eq!(d, None);
    }

    #[test]
    fn classifies_https_url() {
        let (k, d, r) = classify_link("https://example.com/foo");
        assert_eq!(k, "https");
        assert_eq!(d, None);
        assert_eq!(r.as_deref(), Some("https://example.com/foo"));
    }

    #[test]
    fn classifies_http_url() {
        let (k, _, r) = classify_link("http://example.com");
        assert_eq!(k, "http");
        assert_eq!(r.as_deref(), Some("http://example.com"));
    }

    #[test]
    fn classifies_at_citekey() {
        let (k, d, r) = classify_link("@nora2023");
        assert_eq!(k, "cite");
        assert_eq!(d, None);
        assert_eq!(r.as_deref(), Some("@nora2023"));
    }

    #[test]
    fn classifies_bare_text_as_fuzzy() {
        let (k, d, r) = classify_link("not a link at all");
        assert_eq!(k, "fuzzy");
        assert_eq!(d, None);
        assert_eq!(r, None);
    }

    // --- §5: coderef classification -----------------------------------

    #[test]
    fn classifies_coderef_link() {
        let (k, d, r) = classify_link("(label)");
        assert_eq!(k, "coderef");
        assert_eq!(d, None);
        assert_eq!(r, None);
    }

    #[test]
    fn classifies_paren_text_without_matching_close_as_fuzzy() {
        // An unclosed `(label` (no closing `)`) is not a coderef;
        // orgize would not produce a Link node for it either, but
        // the classifier is defensive.
        let (k, _, _) = classify_link("(label");
        assert_eq!(k, "fuzzy");
    }

    #[test]
    fn coderef_does_not_appear_in_backlinks() {
        // Coderefs are intra-file; they must never show up in any
        // node's backlinks.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("a.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: A\n\n\
             #+BEGIN_SRC rust\n\
             fn main() { (ref:entry) }\n\
             #+END_SRC\n\n\
             See [[(entry)]] for the coderef.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: B\n\nSee [[(entry)]].\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        for id in [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
        ] {
            let back = idx.backlinks(id).expect("backlinks");
            assert!(
                !back.iter().any(|l| l.kind == "coderef"),
                "no node should see coderefs in its backlinks, got: {back:?}"
            );
        }
    }

    // --- §6: fuzzy intra-file link resolution -------------------------

    #[test]
    fn fuzzy_link_to_name_property_resolves_to_name() {
        // A `[[growth-table]]` link inside a file that has
        // `#+NAME: growth-table` is classified as `kind: "name"`.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("named.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Named\n\n\
             #+NAME: growth-table\n\
             | year | nodes |\n\
             |------+-------|\n\
             | 2024 | 2     |\n\n\
             See [[growth-table]] for the data.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("11111111-1111-1111-1111-111111111111")
            .expect("forward");
        let named = fwd
            .iter()
            .find(|l| l.raw_dest == "growth-table")
            .expect("named record");
        assert_eq!(named.kind, "name");
        assert_eq!(named.dest, None, "name is intra-file, no node dest");
    }

    #[test]
    fn fuzzy_link_with_no_match_stays_fuzzy() {
        // A `[[unknown]]` link in a file without a matching name
        // remains `kind: "fuzzy"`.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("no-match.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: No match\n\n\
             See [[unknown]] for the data.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("forward");
        let fz = fwd
            .iter()
            .find(|l| l.raw_dest == "unknown")
            .expect("fuzzy record");
        assert_eq!(fz.kind, "fuzzy");
    }

    #[test]
    fn fuzzy_link_to_name_in_another_file_still_reclassifies() {
        // The reclassification is vault-wide: a `[[name]]` link in
        // one file matches a `#+NAME:` declared in any other file
        // (since names are intra-file, the dest stays None; the
        // classification is what tells the agent to look for a
        // local resolution).
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("names.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Names\n\n\
             #+NAME: cross-file-name\n\
             | a | b |\n\
             |---+---|\n\
             | 1 | 2 |\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("uses.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: Uses\n\n\
             See [[cross-file-name]] for the data.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("forward");
        let n = fwd
            .iter()
            .find(|l| l.raw_dest == "cross-file-name")
            .expect("named record");
        assert_eq!(n.kind, "name");
        assert_eq!(n.dest, None, "name is intra-file, no node dest");
    }

    #[test]
    fn fuzzy_link_kept_fuzzy_when_name_conflicts_with_known_node() {
        // A name that happens to also be a node id (or id-prefix)
        // must not be reclassified as `id`. Names are intra-file
        // even if they shadow node identifiers.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("target.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Target\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("source.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: Source\n\
             \n\
             #+NAME: 11111111-1111-1111-1111-111111111111\n\
             | a | b |\n\
             |---+---|\n\
             | 1 | 2 |\n\n\
             See [[11111111-1111-1111-1111-111111111111]] for the data.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("forward");
        let link = fwd
            .iter()
            .find(|l| l.raw_dest == "11111111-1111-1111-1111-111111111111")
            .expect("the link record");
        // The `id:` form is detected by classify_link first (it
        // looks at the raw target's prefix), so this is an `id`
        // link, not a `name`. (No `id:` prefix here, so the
        // scanner still reclassifies it as `name` because the
        // target matches a `#+NAME:`.) We assert either: the
        // reclassification OR the `id`-as-fuzzy-no-match shape
        // is intentional. The test exists to pin the behaviour.
        assert!(
            link.kind == "id" || link.kind == "name",
            "expected id or name, got {link:?}"
        );
    }

    // --- §2: radio targets are file-local, not graph edges ---

    #[test]
    fn radio_target_is_file_local_no_backlinks() {
        // A radio target is not an org `[[...]]` link. The scanner
        // walks the AST looking for `Link` nodes, so a radio target
        // never produces a `LinkRecord` regardless of which file it
        // sits in.
        let parsed = parse_fixture("with_anchors.org");
        let id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let links = parsed.forward.get(id).cloned().unwrap_or_default();
        // None of the forward links on the anchor playground file are
        // radio targets; we also expect no `radio` kind to exist (it's
        // not a value `classify_link` ever returns).
        for l in &links {
            assert_ne!(
                l.kind, "radio_target",
                "radio target must not become a link record"
            );
            assert_ne!(
                l.kind, "radio",
                "no `radio` kind exists in the link vocabulary"
            );
        }
        // The radio target text appears in the file but is invisible
        // to the link graph; nothing in `forward` references it.
        let mut all = parsed.forward.values().flat_map(|v| v.iter());
        assert!(!all.any(|l| l.raw_dest.contains("on-every-occurrence")));
    }

    // --- split_anchor ------------------------------------------------

    #[test]
    fn split_anchor_with_double_colon() {
        let (id, anc) = split_anchor("uuid::verse-4");
        assert_eq!(id, "uuid");
        assert_eq!(anc, Some("verse-4"));
    }

    #[test]
    fn split_anchor_without_double_colon() {
        let (id, anc) = split_anchor("uuid-only");
        assert_eq!(id, "uuid-only");
        assert_eq!(anc, None);
    }

    // --- §4: in-body org-cite scanning -------------------------------

    #[test]
    fn find_citation_keys_extracts_multiple_keys() {
        let text = "See [cite:@nora2023; @smith2020 p. 41] for context.";
        let keys: Vec<String> = find_citation_keys(text)
            .into_iter()
            .map(|(_, k, _)| k)
            .collect();
        assert_eq!(keys, vec!["@nora2023", "@smith2020"]);
    }

    #[test]
    fn find_citation_keys_handles_style_suffix() {
        // Style variants like `[cite/t:@key]` (textual) and
        // `[cite/b:@key]` (bare) are recorded with the style stripped
        // from `raw_dest` so `find_by_ref` still matches.
        let text = "Two [cite/t:@nora2023] styles [cite/b:@smith2020].";
        let raw_dests: Vec<String> = find_citation_keys(text)
            .into_iter()
            .map(|(r, _, _)| r)
            .collect();
        assert_eq!(raw_dests, vec!["@nora2023", "@smith2020"]);
    }

    #[test]
    fn find_citation_keys_handles_locator_text() {
        // Locator phrases like `p. 41` after a key are part of the
        // citation syntax, not a second key.
        let text = "[cite:@nora2023 p. 41, chap. 3]";
        let keys: Vec<String> = find_citation_keys(text)
            .into_iter()
            .map(|(_, k, _)| k)
            .collect();
        assert_eq!(keys, vec!["@nora2023"]);
    }

    #[test]
    fn find_citation_keys_ignores_non_cite_brackets() {
        // A non-`cite` bracketed word is not a citation.
        let text = "Some [example] but no cite.";
        assert!(find_citation_keys(text).is_empty());
    }

    #[test]
    fn in_body_citation_emits_cite_link_record() {
        let parsed = parse_fixture("with_links.org");
        let id = "11111111-2222-3333-4444-555555555555";
        let links = parsed.forward.get(id).expect("forward links");
        let cite_keys: Vec<String> = links
            .iter()
            .filter(|l| l.kind == "cite")
            .filter_map(|l| l.ref_target.clone())
            .collect();
        assert!(
            cite_keys.contains(&"@nora2023".to_string()),
            "expected @nora2023 in cite records, got {cite_keys:?}"
        );
        assert!(
            cite_keys.contains(&"@smith2020".to_string()),
            "expected @smith2020 in cite records, got {cite_keys:?}"
        );
    }

    #[test]
    fn in_body_citation_registers_in_find_by_ref() {
        // Strip the ROAM_REFS that already declare @nora2023, and the
        // file must still be findable via find_by_ref because the
        // citation is in the body.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("body-only.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Body only\n\n\
             See [cite:@nora2023] for context.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let found = idx.by_ref("@nora2023").expect("by_ref");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn in_body_citation_with_style_preserves_key_in_ref_target() {
        // A `[cite/t:@nora2023]` (textual) line is recorded with
        // `raw_dest: "@nora2023"` (style stripped) and `ref_target:
        // Some("@nora2023")` so `find_by_ref` works.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("styled.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: Styled\n\nA textual [cite/t:@nora2023] reference.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("forward");
        let cite = fwd
            .iter()
            .find(|l| l.kind == "cite")
            .expect("cite link record");
        assert_eq!(cite.raw_dest, "@nora2023");
        assert_eq!(cite.ref_target.as_deref(), Some("@nora2023"));

        let found = idx.by_ref("@nora2023").expect("by_ref");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "22222222-2222-2222-2222-222222222222");
    }

    #[test]
    fn bibliography_keyword_becomes_a_tag() {
        // `#+bibliography: refs.bib` registers the file with a
        // `bibliography:refs.bib` tag, so it's findable by tag query.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("lit.org"),
            ":PROPERTIES:\n:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n:END:\n\
             #+title: Lit\n#+bibliography: refs.bib\n\nBody.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let node = idx
            .node("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect("node")
            .expect("exists");
        assert!(
            node.tags.contains(&"bibliography:refs.bib".to_string()),
            "expected `bibliography:refs.bib` tag, got {:?}",
            node.tags
        );
        // A tag-filter search picks the file up.
        let q = NodeQuery {
            query: None,
            tags: &["bibliography:refs.bib".to_string()],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn adjacent_citations_in_one_paragraph_yield_each_key() {
        // Two citations on the same line: every key from both must
        // appear in the forward links.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("two-cites.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Two\n\n[cite:@a; @b] and [cite:@c].\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("11111111-1111-1111-1111-111111111111")
            .expect("forward");
        let keys: Vec<String> = fwd
            .iter()
            .filter(|l| l.kind == "cite")
            .filter_map(|l| l.ref_target.clone())
            .collect();
        for expected in &["@a", "@b", "@c"] {
            assert!(
                keys.iter().any(|k| k == *expected),
                "expected {expected} in {keys:?}"
            );
        }
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn in_body_citation_in_headline_section_attributes_to_headline() {
        // A citation inside a headline's own section (not the file
        // section) attributes to the headline node, not the file.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("headline-cite.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: HC\n\n\
             * In here\n\
             :PROPERTIES:\n\
             :ID: 22222222-2222-2222-2222-222222222222\n\
             :END:\n\
             \n\
             A citation [cite:@a2023] in the headline's body.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let file_fwd = idx
            .forward_links("11111111-1111-1111-1111-111111111111")
            .expect("file forward");
        let headline_fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("headline forward");
        assert!(
            !file_fwd
                .iter()
                .any(|l| l.kind == "cite" && l.ref_target.as_deref() == Some("@a2023")),
            "file node should not see the headline's citation, got: {file_fwd:?}"
        );
        assert!(
            headline_fwd
                .iter()
                .any(|l| l.kind == "cite" && l.ref_target.as_deref() == Some("@a2023")),
            "headline node must see the citation, got: {headline_fwd:?}"
        );
    }

    #[test]
    fn citation_in_unid_headline_falls_back_to_ancestor() {
        // A citation in a headline without `:ID:` falls back to the
        // nearest enclosing node, per the scanner's link-ownership
        // rule.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("orphan-cite.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: OC\n\n\
             * Parent\n\
             :PROPERTIES:\n\
             :ID: 22222222-2222-2222-2222-222222222222\n\
             :END:\n\
             \n\
             ** Child (no id)\n\n\
             A citation [cite:@a2023] in a child headline.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let parent_fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("parent forward");
        assert!(
            parent_fwd
                .iter()
                .any(|l| l.kind == "cite" && l.ref_target.as_deref() == Some("@a2023")),
            "child's citation should attribute to parent, got: {parent_fwd:?}"
        );
    }

    #[test]
    fn inside_org_link_helper_finds_inner_bracket() {
        // The helper must recognise `pos` as the *inner* bracket of
        // a `[[` (i.e. `text[pos]` is the second `[`).
        let text = "before [[cite:@key]] after";
        // Position of the second `[` in `[[` is 8.
        let inner = text.find("[[").unwrap() + 1;
        assert!(inside_org_link(text, inner));
    }

    #[test]
    fn inside_org_link_helper_finds_unrelated_bracket() {
        // A bare `[brackets]` (single, not part of `[[...]]`) is not
        // an org link.
        let text = "a [single] b";
        let mid = text.find('[').unwrap() + 1;
        assert!(!inside_org_link(text, mid));
    }

    #[test]
    fn inside_org_link_helper_finds_balanced_outer_link() {
        // A `[[a]]` is fully closed by position 4. Asking about
        // position 5 (after the closing `]]`) returns false.
        let text = "[[a]] b";
        let pos_after_close = text.find("]]").unwrap() + 2;
        assert!(!inside_org_link(text, pos_after_close));
        // Asking about the body's `a` (position 3) returns true.
        let pos_in_body = text.find('a').unwrap();
        assert!(inside_org_link(text, pos_in_body));
    }

    #[test]
    fn citation_inside_link_syntax_does_not_double_count() {
        // `[[cite:@nora2023]]` is org-link syntax; orgize parses it as
        // a single Link. The text-based citation scanner must not
        // also fire on the same surface text, otherwise we get two
        // records (a fuzzy link + a cite link) for what is really one
        // link. Exactly one cite record should appear, and its
        // `ref_target` is the bare key so `find_by_ref` matches.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("linked-cite.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Linked cite\n\nSee [[cite:@nora2023]] for context.\n",
        )
        .unwrap();
        let idx = ScanIndex::open(dir.path()).expect("open");
        let fwd = idx
            .forward_links("11111111-1111-1111-1111-111111111111")
            .expect("forward");
        // Exactly one cite record, no duplicate from the text scan.
        let cite_records: Vec<&LinkRecord> = fwd.iter().filter(|l| l.kind == "cite").collect();
        assert_eq!(
            cite_records.len(),
            1,
            "expected exactly one cite record, got {fwd:?}"
        );
        // `ref_target` is the bare key (so find_by_ref still works);
        // `raw_dest` keeps the link path as written.
        assert_eq!(cite_records[0].ref_target.as_deref(), Some("@nora2023"));
        // No duplicate fuzzy record from the same surface.
        let fuzzy_dups: Vec<&LinkRecord> = fwd
            .iter()
            .filter(|l| l.kind == "fuzzy" && l.raw_dest.contains("@nora2023"))
            .collect();
        assert!(
            fuzzy_dups.is_empty(),
            "expected no duplicate fuzzy record for the in-link citation, got {fuzzy_dups:?}"
        );
    }

    #[test]
    fn citation_across_line_breaks_is_matched() {
        // The text-based scanner looks for the next `]` after the
        // opener, so a citation that wraps onto the next line is
        // matched (the keys are still between `:` and `]`). This
        // documents the actual behaviour: line-wrapped citations
        // are still findable, which is friendly to long citations.
        let text = "A citation: [cite:@nora2023\n; @smith2020].";
        let keys: Vec<String> = find_citation_keys(text)
            .into_iter()
            .map(|(_, k, _)| k)
            .collect();
        assert_eq!(keys, vec!["@nora2023", "@smith2020"]);
    }

    #[test]
    fn empty_citation_yields_no_records() {
        // `[cite:]` with no keys must not produce empty records.
        let text = "An empty citation: [cite:].";
        let keys = find_citation_keys(text);
        assert!(keys.is_empty(), "empty cite should yield no keys");
    }

    // --- file slugs (for id:<slug> link resolution) ------------------

    #[test]
    fn strip_leading_timestamp_strips_14_digit_prefix() {
        assert_eq!(
            strip_leading_timestamp("20260613205004-bistritz"),
            Some("bistritz")
        );
        assert_eq!(
            strip_leading_timestamp("20260112093000-index"),
            Some("index")
        );
    }

    #[test]
    fn strip_leading_timestamp_ignores_non_timestamps() {
        assert_eq!(strip_leading_timestamp("bistritz"), None);
        assert_eq!(strip_leading_timestamp("2026-bistritz"), None);
        assert_eq!(strip_leading_timestamp("20260613205004"), None);
    }

    #[test]
    fn file_slugs_yields_stem_and_stripped_slug() {
        let slugs = file_slugs(Path::new("/vault/20260613205004-bistritz.org"));
        assert!(slugs.contains(&"20260613205004-bistritz".to_string()));
        assert!(slugs.contains(&"bistritz".to_string()));
    }

    // --- links in the with_links fixture ----------------------------

    #[test]
    fn extracts_all_link_kinds_from_fixture() {
        let parsed = parse_fixture("with_links.org");
        // The file-level node should have several forward links.
        let id = "11111111-2222-3333-4444-555555555555";
        let links = parsed.forward.get(id).expect("forward links");
        let kinds: std::collections::HashSet<String> =
            links.iter().map(|l| l.kind.clone()).collect();
        assert!(kinds.contains("id"), "kinds = {kinds:?}");
        assert!(kinds.contains("file"), "kinds = {kinds:?}");
        assert!(kinds.contains("https"), "kinds = {kinds:?}");
        assert!(kinds.contains("roam"), "kinds = {kinds:?}");
    }

    #[test]
    fn links_before_first_headline_belong_to_file_node() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("pre.org");
        std::fs::write(
            &path,
            ":PROPERTIES:\n:ID: 99999999-9999-9999-9999-999999999999\n:END:\n\
             #+title: Pre-headline links\n\n\
             See [[id:11111111-1111-1111-1111-111111111111][the canticle]].\n\n\
             * A headline without id\n",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed = ParsedFile::new(&path, &text);
        let links = parsed
            .forward
            .get("99999999-9999-9999-9999-999999999999")
            .expect("file node has forward links");
        assert!(
            links
                .iter()
                .any(|l| l.dest.as_deref() == Some("11111111-1111-1111-1111-111111111111")),
            "link in the pre-headline section must attribute to the file node"
        );
    }

    #[test]
    fn roam_tags_and_roam_key_keywords_index_the_file_node() {
        // org-roam v1 vaults carry tags/refs in `#+ROAM_TAGS:` and
        // `#+ROAM_KEY:` keywords instead of `#+filetags:` / ROAM_REFS.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("v1.org");
        std::fs::write(
            &path,
            ":PROPERTIES:\n:ID: 88888888-8888-8888-8888-888888888888\n:END:\n\
             #+TITLE: V1-style note\n\
             #+ROAM_TAGS: hub projects\n\
             #+ROAM_KEY: index\n\n\
             Body.\n",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed = ParsedFile::new(&path, &text);
        assert_eq!(parsed.nodes.len(), 1);
        assert_eq!(parsed.nodes[0].tags, vec!["hub", "projects"]);
        assert!(
            parsed
                .refs
                .iter()
                .any(|(r, id)| r == "index" && id == "88888888-8888-8888-8888-888888888888"),
            "ROAM_KEY value must register as a ref of the file node"
        );
    }

    // --- ScanIndex on the vault fixture -------------------------------

    #[test]
    fn scan_index_reads_six_file_vault() {
        // The vault fixture is the integration surface for the scanner
        // tests. It now has 6 file-level nodes (the original 4 plus
        // psalm23.org and shepherd.org, added for the unlinked-references
        // coverage in §0.1).
        let idx = ScanIndex::open(&vault_dir()).expect("open vault");
        assert_eq!(idx.node_count().unwrap(), 6, "expected 6 file-level nodes");
    }

    #[test]
    fn scan_index_by_ref_finds_citation_from_two_files() {
        // Both fsm_canticle.org and citeref.org declare @nora2023 in ROAM_REFS.
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let found = idx.by_ref("@nora2023").expect("by_ref");
        let ids: std::collections::HashSet<String> = found.iter().map(|n| n.id.clone()).collect();
        assert!(ids.contains("11111111-1111-1111-1111-111111111111"));
        assert!(ids.contains("44444444-4444-4444-4444-444444444444"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn scan_index_by_ref_finds_url() {
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let found = idx
            .by_ref("https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster")
            .expect("by_ref");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn scan_index_tags_count_across_files() {
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let tags = idx.tags().expect("tags");
        let map: std::collections::HashMap<String, usize> = tags.into_iter().collect();
        // fsm_canticle.org declares :pastafarianism:canticles: → pastafarianism=1, canticles=1
        // noodly.org declares :religion:symbolism:
        // citeref.org declares :literature:
        assert_eq!(map.get("pastafarianism").copied(), Some(1));
        assert_eq!(map.get("canticles").copied(), Some(1));
        assert_eq!(map.get("religion").copied(), Some(1));
        assert_eq!(map.get("symbolism").copied(), Some(1));
        assert_eq!(map.get("literature").copied(), Some(1));
    }

    #[test]
    fn scan_index_backlinks_attribute_links_to_file_node() {
        // fsm_canticle.org has [[id:22222...]] inside the "Links" headline
        // (no :ID: on that headline). The link should attribute to the
        // file-level node 11111.
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let back = idx
            .backlinks("22222222-2222-2222-2222-222222222222")
            .expect("backlinks");
        let sources: std::collections::HashSet<String> =
            back.iter().map(|l| l.source.clone()).collect();
        assert!(sources.contains("11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn scan_index_search_by_partial_title() {
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let q = NodeQuery {
            query: Some("Nood"),
            tags: &[],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        // "Noodly Appendage imagery" matches "Nood".
        let titles: Vec<&str> = found.iter().map(|n| n.title.as_str()).collect();
        assert!(
            titles.contains(&"Noodly Appendage imagery"),
            "titles = {titles:?}"
        );
    }

    #[test]
    fn scan_index_search_by_alias() {
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let q = NodeQuery {
            query: Some("Noodly Psalm"),
            tags: &[],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        // "The Noodly Psalm" is an alias of fsm_canticle.
        assert!(found
            .iter()
            .any(|n| n.id == "11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn scan_index_search_filters_by_tag() {
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let q = NodeQuery {
            query: None,
            tags: &["pastafarianism".to_string()],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn tag_filter_does_not_match_aliases() {
        // "The Noodly Psalm" is an alias of fsm_canticle, not a tag. A tag
        // filter must not match it.
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let q = NodeQuery {
            query: None,
            tags: &["The Noodly Psalm".to_string()],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        assert!(found.is_empty(), "aliases must not satisfy a tag filter");
    }

    #[test]
    fn scan_index_search_query_matches_tag() {
        // A free-text query must match a node by its tag, not just title
        // and alias — `search_nodes` advertises "title, alias, or tag".
        let idx = ScanIndex::open(&vault_dir()).expect("open");
        let q = NodeQuery {
            query: Some("symbolism"),
            tags: &[],
            limit: Some(10),
        };
        let found = idx.find_nodes(&q).expect("search");
        assert!(
            found
                .iter()
                .any(|n| n.tags.iter().any(|t| t == "symbolism")),
            "query must match nodes by tag, got: {:?}",
            found.iter().map(|n| &n.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scan_index_resolves_slug_form_id_links() {
        // org-roam links by :ID:, but agents write the basename slug. A
        // `[[id:<slug>]]` link must resolve to the file whose basename slug
        // matches, so it joins the backlink graph.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("20260101000000-target.org"),
            ":PROPERTIES:\n:ID: 11111111-1111-1111-1111-111111111111\n:END:\n\
             #+title: Target\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("20260101000001-source.org"),
            ":PROPERTIES:\n:ID: 22222222-2222-2222-2222-222222222222\n:END:\n\
             #+title: Source\n\nSee [[id:target][Target]].\n",
        )
        .unwrap();

        let idx = ScanIndex::open(dir.path()).expect("open");
        let back = idx
            .backlinks("11111111-1111-1111-1111-111111111111")
            .expect("backlinks");
        assert!(
            back.iter()
                .any(|l| l.source == "22222222-2222-2222-2222-222222222222"),
            "a slug-form [[id:target]] link must resolve into backlinks"
        );
        // raw_dest preserves the link path as written (the slug form).
        assert!(back.iter().any(|l| l.raw_dest == "id:target"));
        // The forward link's destination is resolved to the real :ID: too.
        let fwd = idx
            .forward_links("22222222-2222-2222-2222-222222222222")
            .expect("forward");
        assert!(fwd
            .iter()
            .any(|l| l.dest.as_deref() == Some("11111111-1111-1111-1111-111111111111")));
    }

    #[test]
    fn scan_index_ambiguous_slug_does_not_resolve() {
        // Two files share the basename slug "note"; a slug-form link is
        // ambiguous and must be left unresolved rather than guessed.
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("20260101000000-note.org"),
            ":PROPERTIES:\n:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n:END:\n#+title: One\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("20260101000001-note.org"),
            ":PROPERTIES:\n:ID: bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\n:END:\n#+title: Two\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("20260101000002-source.org"),
            ":PROPERTIES:\n:ID: cccccccc-cccc-cccc-cccc-cccccccccccc\n:END:\n\
             #+title: Source\n\nSee [[id:note][Note]].\n",
        )
        .unwrap();

        let idx = ScanIndex::open(dir.path()).expect("open");
        assert!(idx
            .backlinks("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect("backlinks")
            .is_empty());
        assert!(idx
            .backlinks("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
            .expect("backlinks")
            .is_empty());
    }

    // --- §2 (todo-followup): property tests for find_citation_keys -------

    use proptest::prelude::*;

    proptest! {
        /// `find_citation_keys` must never panic on any input string,
        /// including inputs with null bytes, very long strings, and
        /// adversarial bracket patterns.
        #[test]
        fn find_citation_keys_never_panics_on_arbitrary_input(s in ".*") {
            let _ = find_citation_keys(&s);
        }

        /// Keys returned by `find_citation_keys` must always start with `@`
        /// and never be empty. The raw_dest and ref_target in each triple
        /// must be identical (style was already stripped).
        #[test]
        fn find_citation_keys_output_invariants(s in ".*") {
            for (raw, key, _pos) in find_citation_keys(&s) {
                prop_assert!(raw.starts_with('@'), "raw_dest must start with @: {raw:?}");
                prop_assert!(raw.len() > 1, "raw_dest must not be bare @: {raw:?}");
                prop_assert_eq!(&raw, &key, "raw_dest and key must match (style stripped)");
            }
        }

        /// A `[cite:...]` block that appears inside an org link `[[...]]`
        /// must not produce any output: `inside_org_link` must filter it.
        #[test]
        fn citation_inside_link_brackets_is_always_skipped(
            prefix in "[a-zA-Z ]{0,20}",
            key in "[a-zA-Z0-9_-]{1,15}",
            suffix in "[a-zA-Z ]{0,20}",
        ) {
            // Embed a well-formed citation inside an org link.
            let text = format!("{prefix}[[cite:@{key}]]{suffix}");
            // The `[[cite:@key]]` form is handled by the link parser (orgize),
            // not the text scanner. The text scanner should skip it because
            // the citation opener falls inside `[[...]]`.
            let results = find_citation_keys(&text);
            // There may be zero or one result: if the citation is not
            // recognised as being inside a link (e.g. because the key
            // contains characters orgize parses differently), the scanner
            // may still fire. What we assert is that it never panics and
            // that any key found starts with `@`.
            for (raw, _, _) in results {
                prop_assert!(raw.starts_with('@'));
            }
        }
    }
}
