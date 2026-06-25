//! Validation for org-roam node sources.
//!
//! A node is "valid" if it satisfies two layers of checks:
//!
//! 1. **org-roam invariants** — the org-roam spec the rest of the server
//!    relies on: a `:PROPERTIES:` drawer containing a valid `:ID:` UUID,
//!    and (optionally) well-formed `:ROAM_ALIASES:` and `:ROAM_REFS:`.
//!    These are the things that, if broken, would silently corrupt the
//!    index or break backlinks.
//!
//! 2. **Structural well-formedness** — the org file parses as a tree
//!    orgize can read end-to-end, with sensible headline nesting. This
//!    is intentionally narrow: we don't try to mirror Emacs `org-element`
//!    exactly, we just catch the things that would confuse the indexer
//!    or the read-side tools (a level jump from 1 to 3, a property
//!    drawer that never closes, etc.).
//!
//! The result is a flat list of [`ValidationIssue`]s with line/column
//! info. Callers ([`crate::tools::validation_tools::validate_node`] and
//! the `find_invalid_nodes` bulk scanner) surface that list to the MCP
//! client and refuse to write on failure.

use std::path::{Path, PathBuf};

use orgize::ast::{Headline, Link, Section};
use orgize::rowan::ast::AstNode as _;
use orgize::SyntaxKind;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::org::OrgDoc;

/// One issue found in a node's source.
///
/// The shape is intentionally flat: an MCP client (or a human reading
/// the JSON) can sort/filter by `kind_group` and `variant` without
/// walking nested objects. `line`/`column` are 1-based; `None` means
/// "no specific location applies" (rare, used for whole-file invariants
/// like "no `:PROPERTIES:` drawer at all").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// "orgize" for structural issues, "`org_roam`" for org-roam invariants.
    pub kind_group: IssueGroup,

    /// A stable, greppable identifier for the specific issue, e.g.
    /// `"headline_level_mismatch"`, `"missing_id_drawer"`.
    pub variant: String,

    /// Human-readable description of the problem.
    pub message: String,

    /// 1-based line number, if the issue points at a specific location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,

    /// 1-based column number (in characters, not bytes), if the issue
    /// points at a specific location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// Which layer produced the issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueGroup {
    /// Structural org-mode well-formedness (orgize AST-based checks).
    Orgize,
    /// org-roam-specific invariants the index/backlink layer relies on.
    OrgRoam,
}

/// The output of validating a single node's source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// All issues found, in the order they were discovered. Empty means
    /// the source passed every check.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    /// `true` when no issues were found (the source is valid).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }

    /// Push a single issue.
    fn push(&mut self, issue: ValidationIssue) {
        self.issues.push(issue);
    }
}

/// Constructors for the common case where the variant is a static
/// string — saves `.to_string()` boilerplate at every call site.
impl ValidationIssue {
    fn new(
        kind_group: IssueGroup,
        variant: &'static str,
        message: String,
        line: Option<u32>,
        column: Option<u32>,
    ) -> Self {
        Self {
            kind_group,
            variant: variant.to_string(),
            message,
            line,
            column,
        }
    }
}

impl IntoIterator for ValidationReport {
    type Item = ValidationIssue;
    type IntoIter = std::vec::IntoIter<ValidationIssue>;
    fn into_iter(self) -> Self::IntoIter {
        self.issues.into_iter()
    }
}

/// Validate a single node's org text end-to-end.
///
/// Runs the org-roam invariant pass first (cheaper, no AST walk), then
/// the structural pass. Returns a [`ValidationReport`] — empty when the
/// source is valid.
#[must_use]
pub fn validate_node_source(source: &str) -> ValidationReport {
    let mut report = ValidationReport::default();
    check_org_roam_invariants(source, &mut report);
    // Only walk the AST if the org-roam layer accepted the source as a
    // file we'd care about structurally. This avoids noisy structural
    // errors for inputs that are clearly not a node at all (e.g. a file
    // with no `:ID:` is already reported as `missing_id_drawer`).
    if report.is_ok() {
        check_structural(source, &mut report);
    }
    report
}

/// Run context-aware checks (e.g. external link validation).
///
/// # Errors
///
/// This does not return errors directly; it pushes [`ValidationIssue`]s
/// into `report`.
pub fn validate_node_with_context(
    source: &str,
    roam_dir: &Path,
    file_path: &Path,
    index: Option<&dyn crate::index::RoamIndex>,
    report: &mut ValidationReport,
) {
    check_external_links(source, roam_dir, file_path, index, report);
}

/// Flag any external links that point to non-existent files or that
/// should be `id:` links instead.
fn check_external_links(
    source: &str,
    roam_dir: &Path,
    file_path: &Path,
    index: Option<&dyn crate::index::RoamIndex>,
    report: &mut ValidationReport,
) {
    let doc = OrgDoc::from_text(source.to_string());
    for n in doc.document().syntax().descendants() {
        let Some(link) = Link::cast(n) else {
            continue;
        };
        let raw_path = link.path().to_string();
        let (line_no, col_no) = line_col_for(source, link.syntax().text_range().start().into());

        let is_internal = raw_path.starts_with("id:")
            || raw_path.starts_with("roam:")
            || raw_path.starts_with("http:")
            || raw_path.starts_with("https:")
            || raw_path.starts_with("cite:")
            || raw_path.starts_with("mailto:")
            || raw_path.starts_with("doi:");

        if let Some(rest) = raw_path.strip_prefix("file:") {
            validate_file_link(rest, roam_dir, file_path, index, line_no, col_no, report);
        } else if raw_path.starts_with('/')
            || raw_path.starts_with("./")
            || raw_path.starts_with("../")
        {
            validate_file_link(
                &raw_path, roam_dir, file_path, index, line_no, col_no, report,
            );
        } else if !is_internal
            && (Path::new(&raw_path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("org"))
                || raw_path.contains('/'))
        {
            // "Regular org links that point to files"
            validate_file_link(
                &raw_path, roam_dir, file_path, index, line_no, col_no, report,
            );
        }
    }
}

fn validate_file_link(
    path_str: &str,
    roam_dir: &Path,
    file_path: &Path,
    index: Option<&dyn crate::index::RoamIndex>,
    line_no: u32,
    col_no: u32,
    report: &mut ValidationReport,
) {
    // Strip anchors if present (e.g. `file:foo.org::*Heading`)
    let (clean_path, _anchor) = if let Some((p, a)) = path_str.split_once("::") {
        (p, Some(a))
    } else {
        (path_str, None)
    };

    if clean_path.is_empty() {
        return;
    }

    let p = PathBuf::from(clean_path);
    let abs_path = file_path
        .parent()
        .map_or_else(|| p.clone(), |parent| parent.join(&p));

    // Existence check
    if !abs_path.exists() {
        report.push(ValidationIssue::new(
            IssueGroup::OrgRoam,
            "broken_file_link",
            format!("file link points to non-existent path: {clean_path}"),
            Some(line_no),
            Some(col_no),
        ));
        return;
    }

    // Vault boundary check: if it's inside the vault, it should probably be an ID link.
    if let Ok(canonical) = abs_path.canonicalize() {
        if let Ok(canonical_roam) = roam_dir.canonicalize() {
            if canonical.starts_with(&canonical_roam)
                && canonical.extension().and_then(|e| e.to_str()) == Some("org")
            {
                // It's an org file in the vault. Does it have an ID?
                let has_id = if let Some(idx) = index {
                    idx.node_by_path(&canonical).is_ok_and(|opt| opt.is_some())
                } else {
                    // Fallback to checking the file directly if no index is provided
                    OrgDoc::from_file(&canonical)
                        .ok()
                        .and_then(|d| {
                            d.document()
                                .properties()
                                .and_then(|props| props.get("ID"))
                                .map(|_| ())
                        })
                        .is_some()
                };

                if has_id {
                    report.push(ValidationIssue::new(
                        IssueGroup::OrgRoam,
                        "prefer_id_link",
                        format!(
                            "link points to a node in the vault via file path: {clean_path}; prefer using an id: link"
                        ),
                        Some(line_no),
                        Some(col_no),
                    ));
                }
            }
        }
    }
}

// ── org-roam invariants ────────────────────────────────────────────────────

/// Run the org-roam invariant checks against `source`. Pushes any
/// issues into `report`. Does not touch the AST.
fn check_org_roam_invariants(source: &str, report: &mut ValidationReport) {
    let drawer = find_properties_drawer(source);
    let Some(drawer) = drawer else {
        // No :PROPERTIES: drawer at all. The most fundamental violation:
        // org-roam cannot index this file as a node.
        report.push(ValidationIssue::new(
            IssueGroup::OrgRoam,
            "missing_properties_drawer",
            "file has no :PROPERTIES: drawer; org-roam requires one".to_string(),
            Some(1),
            Some(1),
        ));
        return;
    };

    let id_value = drawer.get("ID").map(str::trim);
    match id_value {
        None => report.push(ValidationIssue::new(
            IssueGroup::OrgRoam,
            "missing_id_drawer",
            ":PROPERTIES: drawer has no :ID: entry".to_string(),
            Some(drawer.id_line),
            Some(1),
        )),
        Some("") => report.push(ValidationIssue::new(
            IssueGroup::OrgRoam,
            "empty_id_drawer",
            ":ID: is present but empty".to_string(),
            Some(drawer.id_line),
            Some(1),
        )),
        Some(id) => match Uuid::parse_str(id) {
            Ok(_) => {}
            Err(_) => report.push(ValidationIssue::new(
                IssueGroup::OrgRoam,
                "malformed_id_drawer",
                format!(":ID: {id:?} is not a valid UUID"),
                Some(drawer.id_line),
                Some(1),
            )),
        },
    }

    // :ROAM_ALIASES: is a space-separated list of double-quoted strings.
    // org-roam tolerates missing/empty (then no aliases), but a malformed
    // value (unterminated quote, junk) is worth flagging.
    if let Some(raw) = drawer.get("ROAM_ALIASES") {
        if let Err(msg) = check_quoted_list(raw) {
            report.push(ValidationIssue::new(
                IssueGroup::OrgRoam,
                "malformed_roam_aliases",
                format!(":ROAM_ALIASES: {msg}"),
                Some(drawer.aliases_line.unwrap_or(drawer.id_line)),
                Some(1),
            ));
        }
    }

    // :ROAM_REFS: is a space-separated list of either URLs or @citekeys.
    // Both are well-defined; a value that matches neither is a typo.
    if let Some(raw) = drawer.get("ROAM_REFS") {
        for token in raw.split_whitespace() {
            if is_url(token) || is_citekey(token) {
                continue;
            }
            report.push(ValidationIssue::new(
                IssueGroup::OrgRoam,
                "malformed_roam_ref",
                format!(":ROAM_REFS: entry {token:?} is neither a URL nor a @citekey"),
                Some(drawer.refs_line.unwrap_or(drawer.id_line)),
                Some(1),
            ));
        }
    }
}

/// A single `:PROPERTIES:` drawer, scanned out of the source text.
///
/// We don't use the orgize AST for this — `:PROPERTIES:` lives at the
/// top of the file, and a small hand-rolled scanner is enough to
/// extract the values we care about (`:ID:`, `:ROAM_ALIASES:`,
/// `:ROAM_REFS:`) and their 1-based line numbers. Anything more
/// elaborate would be re-implementing orgize.
struct ParsedDrawer {
    id_line: u32,
    aliases_line: Option<u32>,
    refs_line: Option<u32>,
    id: Option<String>,
    aliases: Option<String>,
    refs_: Option<String>,
}

impl ParsedDrawer {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "ID" => self.id.as_deref(),
            "ROAM_ALIASES" => self.aliases.as_deref(),
            "ROAM_REFS" => self.refs_.as_deref(),
            _ => None,
        }
    }
}

fn find_properties_drawer(source: &str) -> Option<ParsedDrawer> {
    // org-roam convention: the :PROPERTIES: drawer is the first non-blank,
    // non-comment content in the file. We accept it anywhere at the top
    // level (before any headline) — that's how the indexer and the
    // `create_node` tool both emit it.
    let mut in_drawer = false;
    let mut id_line: Option<u32> = None;
    let mut aliases_line: Option<u32> = None;
    let mut refs_line: Option<u32> = None;
    let mut id: Option<String> = None;
    let mut aliases: Option<String> = None;
    let mut refs_: Option<String> = None;

    for (idx, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim_end();
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if !in_drawer {
            if line.trim() == ":PROPERTIES:" {
                in_drawer = true;
            }
            // Skip blank lines and comments before finding the drawer.
            continue;
        }
        if line.trim() == ":END:" {
            return Some(ParsedDrawer {
                id_line: id_line.unwrap_or(1),
                aliases_line,
                refs_line,
                id,
                aliases,
                refs_,
            });
        }
        // Drawer entries are `:KEY: value` — the colon at the start and
        // the first space (or EOL) separate key from value.
        let Some(rest) = line.strip_prefix(':') else {
            // Malformed line inside the drawer — skip silently. The
            // structural pass will catch headline-level issues.
            continue;
        };
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "ID" => {
                id_line.get_or_insert(line_no);
                id = Some(value.to_string());
            }
            "ROAM_ALIASES" => {
                aliases_line.get_or_insert(line_no);
                aliases = Some(value.to_string());
            }
            "ROAM_REFS" => {
                refs_line.get_or_insert(line_no);
                refs_ = Some(value.to_string());
            }
            _ => {}
        }
    }
    None
}

fn check_quoted_list(raw: &str) -> Result<(), &'static str> {
    // Empty / whitespace-only is fine — means "no aliases".
    if raw.trim().is_empty() {
        return Ok(());
    }
    let mut in_quote = false;
    let mut current = String::new();
    for c in raw.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
            }
            ' ' | '\t' if !in_quote => {
                if !current.is_empty() {
                    current.clear();
                }
            }
            _ => current.push(c),
        }
    }
    if in_quote {
        return Err("unterminated quoted string");
    }
    Ok(())
}

fn is_url(token: &str) -> bool {
    // org-roam accepts `https?:` and `file:` refs. We don't try to fully
    // parse URLs (no URL crate in deps); a scheme prefix is enough.
    let lower = token.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file:")
        || lower.starts_with("doi:")
}

fn is_citekey(token: &str) -> bool {
    // org-roam citekeys are `@key` — keys can be almost anything but
    // must contain at least one non-`@`, non-whitespace character.
    token.starts_with('@') && token.chars().any(|c| c != '@')
}

// ── structural checks ──────────────────────────────────────────────────────

/// Structural well-formedness: walk the AST once, flag anything that
/// would confuse the indexer or the read-side tools.
fn check_structural(source: &str, report: &mut ValidationReport) {
    let doc = OrgDoc::from_text(source.to_string());
    check_headline_levels(&doc, report);
    check_property_drawers_close(&doc, report);
}

/// Flag any headline whose level is more than one greater than the
/// previous headline's level (e.g. `* foo` followed by `*** bar`).
fn check_headline_levels(doc: &OrgDoc, report: &mut ValidationReport) {
    let mut prev_level: Option<usize> = None;
    for h in doc.headlines() {
        let level = h.level();
        if let Some(prev) = prev_level {
            if level > prev + 1 {
                let (line, column) = line_col_for(&doc.text, byte_offset(&h));
                report.push(ValidationIssue::new(
                    IssueGroup::Orgize,
                    "headline_level_jump",
                    format!(
                        "headline level jumps from {prev} to {level} (must increase by at most 1)"
                    ),
                    Some(line),
                    Some(column),
                ));
            }
        }
        prev_level = Some(level);
    }
}

/// Flag any `:DRAWER:` whose `:END:` never appears within the same
/// headline / section. orgize itself doesn't error on unclosed drawers
/// (the AST is just a tree), but the rest of the server assumes
/// well-formed drawers and will silently miscount.
fn check_property_drawers_close(doc: &OrgDoc, report: &mut ValidationReport) {
    // We do a small recursive walk over SECTION nodes. A SECTION is the
    // child-bearing parent of a headline's body in orgize's AST.
    for section in doc.document().syntax().descendants() {
        if section.kind() != SyntaxKind::SECTION {
            continue;
        }
        if let Some(s) = Section::cast(section) {
            verify_section_drawers(&s, &doc.text, report);
        }
    }
}

fn verify_section_drawers(section: &Section, text: &str, report: &mut ValidationReport) {
    // Iterate the section's tokens; track depth of `:<NAME>:` opens vs
    // `:END:` closes. orgize's `Drawer` AST node would be cleaner, but
    // the alpha we're on doesn't always emit one for unclosed drawers,
    // so we fall back to a token scan.
    let start: usize = section.syntax().text_range().start().into();
    let end = section.syntax().text_range().end();
    let end: usize = end.into();
    let end = end.min(text.len());
    let slice = &text[start..end];

    let mut opens: Vec<(String, u32)> = Vec::new();
    for (idx, raw_line) in slice.lines().enumerate() {
        let line = raw_line.trim();
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if line == ":END:" {
            if let Some((name, open_line)) = opens.pop() {
                let _ = (name, open_line);
            }
        } else if let Some(rest) = line.strip_prefix(':') {
            if let Some((name, _value)) = rest.split_once(':') {
                let name = name.trim();
                if !name.is_empty() && !name.contains(' ') && name != "END" {
                    opens.push((name.to_string(), line_no));
                }
            }
        }
    }
    for (name, open_line) in opens {
        let (line, column) = line_col_for(text, start); // approximate
        report.push(ValidationIssue::new(
            IssueGroup::Orgize,
            "unclosed_drawer",
            format!("drawer :{name}: opened on line {open_line} has no :END:"),
            Some(line),
            Some(column),
        ));
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Convert a 0-based byte offset into `(line, column)` (both 1-based).
/// `column` is in characters, not bytes, for nicer display in errors.
fn line_col_for(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let prefix = &text[..offset];
    let line =
        u32::try_from(prefix.bytes().filter(|&b| b == b'\n').count() + 1).unwrap_or(u32::MAX);
    let last_nl = prefix.rfind('\n').map_or(0, |n| n + 1);
    let column = u32::try_from(prefix[last_nl..].chars().count()).unwrap_or(u32::MAX) + 1;
    (line, column.max(1))
}

/// Byte offset of a headline's start. `Headline::start` returns a
/// `TextSize`; we want a `usize` for slicing into `text`.
fn byte_offset(h: &Headline) -> usize {
    h.syntax().text_range().start().into()
}

// ── file-level driver (used by the bulk scanner) ───────────────────────────

/// One flat entry for the bulk scanner result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidNodeEntry {
    /// The org-roam `:ID:` of the node whose file had this issue, when
    /// we could extract it. `None` for files that fail before we even
    /// know the ID (e.g. no `:PROPERTIES:` drawer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,

    /// Absolute path of the offending file.
    pub file_path: String,

    /// The issue itself.
    #[serde(flatten)]
    pub issue: ValidationIssue,
}

/// Result of a bulk scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkValidationReport {
    /// Number of `.org` files inspected.
    pub scanned: u32,
    /// Number of files that produced at least one issue.
    pub with_issues: u32,
    /// Flat list of every issue, one entry per (file, issue).
    pub issues: Vec<InvalidNodeEntry>,
    /// `true` when the cap was hit and some issues were dropped.
    pub truncated: bool,
}

/// Soft cap on the number of issues returned in a single bulk scan. We
/// pick a number that fits comfortably in a single MCP tool result and
/// is still large enough to be useful on a vault of any size.
pub const BULK_ISSUE_CAP: usize = 10_000;

/// Walk `root` looking for `.org` files, validate each one, and return
/// a flat per-issue list. Soft-caps the result at [`BULK_ISSUE_CAP`].
///
/// # Errors
///
/// Returns an error only if the directory itself cannot be read. Errors
/// reading individual files are reported as `unreadable_file` issues
/// rather than aborting the scan.
pub fn scan_directory_for_invalid(root: &Path) -> std::io::Result<BulkValidationReport> {
    let mut report = BulkValidationReport::default();
    walk_org_files(root, &mut |path| {
        report.scanned += 1;
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                let truncated_before = report.issues.len() >= BULK_ISSUE_CAP;
                if !truncated_before {
                    report.issues.push(InvalidNodeEntry {
                        node_id: None,
                        file_path: path.display().to_string(),
                        issue: ValidationIssue::new(
                            IssueGroup::Orgize,
                            "unreadable_file",
                            format!("could not read file: {e}"),
                            None,
                            None,
                        ),
                    });
                }
                report.truncated = truncated_before || report.issues.len() >= BULK_ISSUE_CAP;
                return;
            }
        };
        let node_id = find_properties_drawer(&source).and_then(|d| d.id.clone());
        let mut node_report = validate_node_source(&source);
        validate_node_with_context(&source, root, path, None, &mut node_report);

        for issue in node_report {
            if report.issues.len() >= BULK_ISSUE_CAP {
                report.truncated = true;
                break;
            }
            report.issues.push(InvalidNodeEntry {
                node_id: node_id.clone(),
                file_path: path.display().to_string(),
                issue,
            });
        }
    })?;
    report.with_issues = count_files_with_issues(&report.issues);
    Ok(report)
}

fn walk_org_files(root: &Path, visit: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_org_files(&path, visit)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("org") {
            visit(&path);
        }
    }
    Ok(())
}

fn count_files_with_issues(issues: &[InvalidNodeEntry]) -> u32 {
    let mut seen = std::collections::HashSet::new();
    for entry in issues {
        seen.insert(entry.file_path.as_str());
    }
    u32::try_from(seen.len()).unwrap_or(u32::MAX)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_node_passes() {
        let src = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:ROAM_ALIASES: \"Foo\" \"Bar\"
:ROAM_REFS: https://example.com @smith2020
:END:
#+title: Valid
#+filetags: :test:

* Heading
body
";
        let r = validate_node_source(src);
        assert!(r.is_ok(), "expected no issues, got: {:#?}", r.issues);
    }

    #[test]
    fn missing_properties_drawer_is_flagged() {
        let src = "#+title: No drawer\n* heading\n";
        let r = validate_node_source(src);
        assert_eq!(r.issues.len(), 1);
        let i = &r.issues[0];
        assert_eq!(i.kind_group, IssueGroup::OrgRoam);
        assert_eq!(i.variant, "missing_properties_drawer");
    }

    #[test]
    fn missing_id_is_flagged() {
        let src = "\
:PROPERTIES:
:ROAM_ALIASES: \"Foo\"
:END:
#+title: No id
";
        let r = validate_node_source(src);
        assert!(
            r.issues.iter().any(|i| i.variant == "missing_id_drawer"),
            "expected missing_id_drawer, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn malformed_id_is_flagged() {
        let src = "\
:PROPERTIES:
:ID:       not-a-uuid
:END:
#+title: Bad id
";
        let r = validate_node_source(src);
        assert!(
            r.issues.iter().any(|i| i.variant == "malformed_id_drawer"),
            "expected malformed_id_drawer, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn malformed_roam_ref_is_flagged() {
        let src = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:ROAM_REFS: https://example.com not-a-ref
:END:
#+title: Bad ref
";
        let r = validate_node_source(src);
        assert!(
            r.issues.iter().any(|i| i.variant == "malformed_roam_ref"),
            "expected malformed_roam_ref, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn unterminated_alias_quote_is_flagged() {
        let src = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:ROAM_ALIASES: \"Foo
:END:
#+title: Bad alias
";
        let r = validate_node_source(src);
        assert!(
            r.issues
                .iter()
                .any(|i| i.variant == "malformed_roam_aliases"),
            "expected malformed_roam_aliases, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn headline_level_jump_is_flagged() {
        let src = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:END:
#+title: Jump
* one
*** three
";
        let r = validate_node_source(src);
        assert!(
            r.issues.iter().any(|i| i.variant == "headline_level_jump"),
            "expected headline_level_jump, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn unclosed_drawer_is_flagged() {
        let src = "\
:PROPERTIES:
:ID:       11111111-1111-1111-1111-111111111111
:END:
#+title: Unclosed
* heading
  :PROPERTIES:
  :FOO: bar
no end here
";
        let r = validate_node_source(src);
        assert!(
            r.issues.iter().any(|i| i.variant == "unclosed_drawer"),
            "expected unclosed_drawer, got: {:#?}",
            r.issues
        );
    }

    #[test]
    fn line_col_for_works_at_offsets() {
        let t = "abc\ndef\nghi";
        // offset 0 -> 'a' of line 1
        assert_eq!(line_col_for(t, 0), (1, 1));
        // offset 4 -> 'd' of line 2
        assert_eq!(line_col_for(t, 4), (2, 1));
        // offset 8 -> 'g' of line 3
        assert_eq!(line_col_for(t, 8), (3, 1));
    }

    #[test]
    fn cited_and_url_refs_accepted() {
        assert!(is_url("https://example.com"));
        assert!(is_url("file:/tmp/x"));
        assert!(is_url("doi:10.1/abc"));
        assert!(!is_url("hello"));
        assert!(is_citekey("@smith2020"));
        assert!(!is_citekey("smith2020"));
        assert!(!is_citekey("@"));
    }
}
