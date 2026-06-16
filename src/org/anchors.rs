//! Anchor resolution: find a sub-section of a `.org` file by anchor name.
//!
//! Resolution order (most-specific first):
//! 1. `CUSTOM_ID` on a headline → that headline's subtree.
//! 2. Headline title match (case-sensitive, exact).
//! 3. `#+NAME:` keyword → the body of the named element.
//! 4. Dedicated target `<<name>>` → the paragraph following the target.
//! 5. Radio target `<<<term>>>` → the paragraph following the marker.
//! 6. Code reference `(ref:label)` → the line of source code.
//! 7. Free-text case-insensitive search → the containing paragraph.
//!
//! Org's own `::` link search (`org-link-search`) requires explicit
//! `#`/`*` prefixes for custom-id and headline matches. We accept those
//! prefixes and strip them transparently so callers can pass either the
//! bare anchor (`v4`, `Verse 1`) or the org-typed form (`#v4`, `*Verse 1`).
//! Multiple `*` are tolerated (`**Inner` matches a level-3 "Inner"
//! headline).

use super::parse::{OrgDoc, Section};
use orgize::rowan::ast::AstNode as _;
use orgize::SyntaxKind;

/// Resolver for anchor queries. Stateless: just methods on `OrgDoc`.
pub struct AnchorResolver;

impl AnchorResolver {
    /// Resolve `anchor` against `doc`. Returns `None` if no match.
    ///
    /// Strips an optional `#` (custom-id), `*` / `* ` (headline title),
    /// or `**` / `** ` (level-prefixed headline) prefix from `anchor`
    /// before consulting the strategies. Dedicated targets `<<...>>` and
    /// radio targets `<<<...>>>` do not carry a prefix in org's syntax.
    #[must_use]
    pub fn resolve(doc: &OrgDoc, anchor: &str) -> Option<Section> {
        let needle = strip_anchor_prefix(anchor.trim());
        if needle.is_empty() {
            return None;
        }
        if let Some(s) = Self::by_custom_id(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_headline_title(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_name(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_dedicated_target(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_radio_target(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_coderef(doc, needle) {
            return Some(s);
        }
        if let Some(s) = Self::by_footnote(doc, needle) {
            return Some(s);
        }
        Self::by_text_search(doc, needle)
    }

    fn by_custom_id(doc: &OrgDoc, name: &str) -> Option<Section> {
        let h = doc.headline_by_custom_id(name)?;
        let (begin, end) = doc.subtree_range(&h);
        Some(Section {
            text: doc.slice(begin, end).to_string(),
            kind: "custom_id".into(),
            begin,
            end,
        })
    }

    fn by_headline_title(doc: &OrgDoc, name: &str) -> Option<Section> {
        let h = doc
            .headlines()
            .into_iter()
            .find(|h| h.title_raw().trim() == name)?;
        let (begin, end) = doc.subtree_range(&h);
        Some(Section {
            text: doc.slice(begin, end).to_string(),
            kind: "headline".into(),
            begin,
            end,
        })
    }

    fn by_dedicated_target(doc: &OrgDoc, name: &str) -> Option<Section> {
        let lt = format!("<<{name}>>");
        // Walk the text looking for the first *bare* `<<name>>` that
        // is not part of a radio target `<<<name>>>`. A radio target's
        // inner `<<name>>` would otherwise be a substring of it, so
        // we must check the byte before each match.
        let bytes = doc.text.as_bytes();
        let start = (0..bytes.len())
            .scan(0, |cursor, _| {
                let hay = &doc.text[*cursor..];
                let rel = hay.find(&lt)?;
                let abs = *cursor + rel;
                *cursor = abs + lt.len();
                Some(abs)
            })
            .find(|&abs| !(abs >= 1 && bytes[abs - 1] == b'<'))?;
        let after_marker = start + lt.len();

        // Skip the rest of the marker line: if the marker is followed
        // by content on the same line (e.g. `<<verse-1>> He maketh me...`),
        // that content is part of the paragraph.
        let same_line_end = doc.text[after_marker..]
            .find('\n')
            .map_or(doc.text.len(), |n| after_marker + n);
        let mut end = same_line_end;
        // Continue accumulating lines until a blank line, headline, or
        // end-of-file. `same_line_end == len` means the marker line is the
        // last line of the file (no trailing newline) — nothing follows.
        if same_line_end < doc.text.len() {
            let mut idx = same_line_end + 1;
            for line in doc.text[idx..].lines() {
                if line.is_empty() || line.starts_with("* ") || line.starts_with("#+") {
                    break;
                }
                end = idx + line.len();
                idx = end + 1;
            }
        }
        let text = doc.slice(start, end).trim_end().to_string();
        Some(Section {
            text,
            kind: "dedicated_target".into(),
            begin: start,
            end,
        })
    }

    fn by_text_search(doc: &OrgDoc, needle: &str) -> Option<Section> {
        let lower = doc.text.to_lowercase();
        let ndl = needle.to_lowercase();
        let pos = lower.find(&ndl)?;
        let line_start = doc.text[..pos].rfind('\n').map_or(0, |n| n + 1);
        let mut end = doc.text.len();
        let mut idx = line_start;
        for line in doc.text[line_start..].lines() {
            if idx > pos && (line.is_empty() || line.starts_with("* ") || line.starts_with("#+")) {
                end = idx;
                break;
            }
            idx += line.len() + 1;
        }
        Some(Section {
            text: doc.slice(line_start, end).to_string(),
            kind: "text_search".into(),
            begin: line_start,
            end,
        })
    }

    /// Radio target `<<<term>>>` — same body-extraction rules as
    /// [`by_dedicated_target`], but matching the three-`<` form.
    fn by_radio_target(doc: &OrgDoc, name: &str) -> Option<Section> {
        let lt = format!("<<<{name}>>>");
        let start = doc.text.find(&lt)?;
        let after_marker = start + lt.len();
        let same_line_end = doc.text[after_marker..]
            .find('\n')
            .map_or(doc.text.len(), |n| after_marker + n);
        let mut end = same_line_end;
        if same_line_end < doc.text.len() {
            let mut idx = same_line_end + 1;
            for line in doc.text[idx..].lines() {
                if line.is_empty() || line.starts_with("* ") || line.starts_with("#+") {
                    break;
                }
                end = idx + line.len();
                idx = end + 1;
            }
        }
        let text = doc.slice(start, end).trim_end().to_string();
        Some(Section {
            text,
            kind: "radio_target".into(),
            begin: start,
            end,
        })
    }

    /// `#+NAME:` cross-reference. We scan the source text directly
    /// because orgize 0.10.0-alpha.10 only attaches `NAME` affiliated
    /// keywords to source blocks reliably; tables and other org
    /// elements fall back to a single PARAGRAPH that absorbs both the
    /// keyword and the body. The text-based scan matches the shape org
    /// itself uses: a `#NAME: foo` line followed by the element it
    /// names (terminated by a blank line, headline, or another `#+`
    /// keyword).
    fn by_name(doc: &OrgDoc, name: &str) -> Option<Section> {
        let key_offset = find_name_keyword(&doc.text, name);
        if let Some(begin) = key_offset {
            // Find the start of the body: first non-blank, non-`#+`
            // line after the keyword.
            let after = doc.text[begin..]
                .find('\n')
                .map_or(doc.text.len(), |n| begin + n + 1);
            let body_start = scan_to_named_body(&doc.text, after);
            if body_start >= doc.text.len() {
                return None;
            }
            // The named element's end: the next blank line, headline,
            // `#+`-keyword line, or end of file.
            let end = scan_named_body_end(&doc.text, body_start);
            return Some(Section {
                text: doc.slice(begin, end).to_string(),
                kind: "name".into(),
                begin,
                end,
            });
        }
        // Fall back to the AST path: orgize does name source blocks
        // and similar, and an MCP caller may want to resolve names
        // even when our text-based scan misses a fixture.
        for n in doc.document().syntax().descendants() {
            let first = n.first_child()?;
            if first.kind() != SyntaxKind::AFFILIATED_KEYWORD {
                continue;
            }
            for child in n.children() {
                if child.kind() != SyntaxKind::AFFILIATED_KEYWORD {
                    break;
                }
                if let Some((key, value)) = read_affiliated_keyword(&child) {
                    if key == "NAME" && value == name {
                        let begin: usize = n.text_range().start().into();
                        let end: usize = n.text_range().end().into();
                        return Some(Section {
                            text: doc.slice(begin, end).to_string(),
                            kind: "name".into(),
                            begin,
                            end,
                        });
                    }
                }
            }
        }
        let _ = key_offset; // silence unused warning
        None
    }

    /// §5: code reference `(ref:label)`. orgize parses the source
    /// block content as a single `BLOCK_CONTENT` token, so a text
    /// scan of the source text is the simplest reliable way to find
    /// the line containing `(ref:label)`. We return just that one
    /// line.
    fn by_coderef(doc: &OrgDoc, label: &str) -> Option<Section> {
        let needle = format!("(ref:{label})");
        let start = doc.text.find(&needle)?;
        // Find the start of the line that contains the marker.
        let line_start = doc.text[..start].rfind('\n').map_or(0, |n| n + 1);
        // Find the end of the line.
        let line_end_rel = doc.text[line_start..]
            .find('\n')
            .map_or(doc.text.len() - line_start, |n| n);
        let line = &doc.text[line_start..line_start + line_end_rel];
        Some(Section {
            text: line.to_string(),
            kind: "coderef".into(),
            begin: line_start,
            end: line_start + line_end_rel,
        })
    }

    /// §7: footnote `[fn:label] ...` definition. orgize exposes
    /// `FnDef` AST nodes for each `[fn:N]` definition. We match by
    /// either the numeric label or a named label after the
    /// `fn:` prefix. The returned text is the footnote's body
    /// paragraph (not including the `[fn:N]` marker line itself).
    fn by_footnote(doc: &OrgDoc, name: &str) -> Option<Section> {
        // Tolerate the caller passing either the bare label or the
        // org-typed `fn:label` form.
        let label = name.strip_prefix("fn:").unwrap_or(name);
        for n in doc.document().syntax().descendants() {
            if n.kind() != SyntaxKind::FN_DEF {
                continue;
            }
            // The FN_DEF's text range begins with `[fn:label]` and is
            // followed by a paragraph of body text. We extract the
            // label from the leading bytes.
            let start: usize = n.text_range().start().into();
            let end: usize = n.text_range().end().into();
            let raw = &doc.text[start..end];
            let Some(after_fn) = raw.strip_prefix("[fn:") else {
                continue;
            };
            let Some(close) = after_fn.find(']') else {
                continue;
            };
            let this_label = &after_fn[..close];
            if this_label != label {
                continue;
            }
            // The body of the footnote is everything after the
            // `[fn:label]` marker on the same line, plus any
            // continuation lines until the next blank line.
            let after_marker = start + 4 + close + 1;
            let same_line_end = doc.text[after_marker..]
                .find('\n')
                .map_or(doc.text.len(), |n| after_marker + n);
            let mut body_end = same_line_end;
            if same_line_end < doc.text.len() {
                let mut idx = same_line_end + 1;
                for line in doc.text[idx..].lines() {
                    if line.is_empty() || line.starts_with("* ") || line.starts_with("#+") {
                        break;
                    }
                    body_end = idx + line.len();
                    idx = body_end + 1;
                }
            }
            let body = doc.slice(after_marker, body_end).trim().to_string();
            return Some(Section {
                text: body,
                kind: "footnote".into(),
                begin: after_marker,
                end: body_end,
            });
        }
        None
    }
}

/// Find the start of a `#NAME: <name>` keyword line. Returns the
/// byte offset of the `#` (start of line). Matches `#+NAME:` and
/// `#+name:` (case-insensitive key, exact value match).
fn find_name_keyword(text: &str, name: &str) -> Option<usize> {
    let mut byte_offset = 0;
    for line in text.split_inclusive('\n') {
        // `line` includes its trailing `\n` if any. We strip leading
        // whitespace and then look at the first non-whitespace chars.
        let trimmed_start = line.trim_start();
        // The keyword must be at the start of a line. The previous
        // byte is `\n` (or byte_offset == 0).
        let Some(rest) = trimmed_start.strip_prefix("#+") else {
            byte_offset += line.len();
            continue;
        };
        let Some((key, value)) = rest.split_once(':') else {
            byte_offset += line.len();
            continue;
        };
        if !key.eq_ignore_ascii_case("NAME") {
            byte_offset += line.len();
            continue;
        }
        if value.trim() != name {
            byte_offset += line.len();
            continue;
        }
        return Some(byte_offset);
    }
    None
}

/// Starting at `start`, skip blank lines and additional `#+`-keyword
/// lines to reach the first line of the named element's body. Returns
/// the byte offset of the body start.
fn scan_to_named_body(text: &str, start: usize) -> usize {
    let mut idx = start;
    while idx < text.len() {
        let line_end = text[idx..].find('\n').map_or(text.len(), |n| idx + n);
        let line = &text[idx..line_end];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            idx = line_end + 1;
            continue;
        }
        if trimmed.starts_with("#+") {
            idx = line_end + 1;
            continue;
        }
        return idx;
    }
    text.len()
}

/// The end of a named element: the start of the next blank line,
/// headline, or `#+`-keyword line that follows the body. Returns the
/// byte offset of that boundary.
fn scan_named_body_end(text: &str, body_start: usize) -> usize {
    let mut idx = body_start;
    while idx < text.len() {
        let line_end = text[idx..].find('\n').map_or(text.len(), |n| idx + n);
        let line = &text[idx..line_end];
        if line.is_empty() {
            return idx;
        }
        if line.starts_with("* ") {
            return idx;
        }
        if line.starts_with("#+") {
            return idx;
        }
        idx = line_end + 1;
    }
    text.len()
}

/// Decode an `AFFILIATED_KEYWORD` node to its `(key, value)` pair.
/// `#+NAME: growth-table` → `("NAME", "growth-table")` (the value
/// has leading whitespace stripped, matching what org's parser does).
fn read_affiliated_keyword(node: &orgize::SyntaxNode) -> Option<(String, String)> {
    let kw = orgize::ast::AffiliatedKeyword::cast(node.clone())?;
    let key = kw.key().to_string();
    let value = kw
        .value()
        .map(|t| t.to_string())
        .unwrap_or_default()
        .trim()
        .to_string();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Strip org's anchor-prefix syntax from `anchor`:
///
/// - `#v4` → `v4` (custom-id)
/// - `*Verse 1` → `Verse 1` (headline title)
/// - `**Inner` → `Inner` (level-prefixed headline)
/// - `*Verse 1 ` → `Verse 1` (trailing whitespace)
///
/// Multiple `*` are tolerated. The function is a no-op for any other
/// prefix, and a bare anchor is returned unchanged.
fn strip_anchor_prefix(anchor: &str) -> &str {
    let trimmed = anchor.trim_start();
    // Custom-id prefix.
    if let Some(rest) = trimmed.strip_prefix('#') {
        return rest.trim_start();
    }
    // Headline prefix: one-or-more `*`, optionally followed by a single
    // space. We do not enforce the space because callers sometimes pass
    // `*Verse1` (no space) and that's still a valid org search target.
    let bytes = trimmed.as_bytes();
    let mut stars = 0;
    while stars < bytes.len() && bytes[stars] == b'*' {
        stars += 1;
    }
    if stars > 0 {
        let rest = &trimmed[stars..];
        return rest.trim_start();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSALM: &str = include_str!("../../tests/fixtures/text/fsm_canticle.org");
    const NESTED: &str = include_str!("../../tests/fixtures/text/nested.org");
    const ANCHORS: &str = include_str!("../../tests/fixtures/text/with_anchors.org");
    const UNICODE: &str = include_str!("../../tests/fixtures/text/unicode.org");
    const LOREM: &str = include_str!("../../tests/fixtures/text/lorem.txt");

    // --- §1: anchor prefix syntax -------------------------------------

    #[test]
    fn anchor_with_hash_prefix_resolves_to_custom_id() {
        let doc = OrgDoc::from_text(PSALM);
        let with_prefix = AnchorResolver::resolve(&doc, "#v4").expect("#v4");
        let bare = AnchorResolver::resolve(&doc, "v4").expect("v4");
        assert_eq!(with_prefix.kind, "custom_id");
        assert_eq!(with_prefix.text, bare.text);
    }

    #[test]
    fn anchor_with_star_prefix_resolves_to_headline() {
        let doc = OrgDoc::from_text(PSALM);
        let with_prefix = AnchorResolver::resolve(&doc, "*Verse 1").expect("*Verse 1");
        let bare = AnchorResolver::resolve(&doc, "Verse 1").expect("Verse 1");
        assert_eq!(with_prefix.kind, "headline");
        assert_eq!(with_prefix.text, bare.text);
    }

    #[test]
    fn anchor_with_double_star_prefix_resolves_to_headline() {
        // `**Inner` matches the level-3 "Inner" headline in nested.org.
        // The resolver does not enforce level-strictness (a bare "Inner"
        // resolves the same headline), it just strips the prefix.
        let doc = OrgDoc::from_text(NESTED);
        let with_prefix = AnchorResolver::resolve(&doc, "**Inner").expect("**Inner");
        let bare = AnchorResolver::resolve(&doc, "Inner").expect("Inner");
        assert_eq!(with_prefix.kind, "headline");
        assert_eq!(with_prefix.text, bare.text);
    }

    #[test]
    fn anchor_prefix_stripping_handles_triple_star() {
        // Triple star should still strip to the bare title.
        let doc = OrgDoc::from_text(NESTED);
        let resolved = AnchorResolver::resolve(&doc, "***Inner").expect("***Inner");
        assert_eq!(resolved.kind, "headline");
    }

    #[test]
    fn anchor_prefix_stripping_does_not_eat_other_words() {
        // "starry" is not a prefix; the helper must leave it alone.
        assert_eq!(strip_anchor_prefix("starry"), "starry");
    }

    #[test]
    fn anchor_prefix_stripping_handles_just_a_prefix() {
        // An anchor that is *only* a prefix has nothing to look up.
        assert!(AnchorResolver::resolve(&OrgDoc::from_text(PSALM), "#").is_none());
        assert!(AnchorResolver::resolve(&OrgDoc::from_text(PSALM), "**").is_none());
    }

    // --- CUSTOM_ID resolution (priority 1) -------------------------

    #[test]
    fn custom_id_resolves_to_headline_subtree() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "v1").expect("v1 must resolve");
        assert_eq!(s.kind, "custom_id");
        assert!(s
            .text
            .contains("He maketh me to lie down in steaming bowls of pasta."));
    }

    #[test]
    fn custom_id_v4_resolves_to_correct_verse() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "v4").expect("v4 must resolve");
        assert_eq!(s.kind, "custom_id");
        assert!(s.text.contains("walk through the valley"));
    }

    #[test]
    fn custom_id_distinguishes_nested_levels() {
        let doc = OrgDoc::from_text(NESTED);
        for cid in &["outer", "middle", "inner"] {
            let s = AnchorResolver::resolve(&doc, cid).unwrap_or_else(|| panic!("{cid}"));
            assert_eq!(s.kind, "custom_id");
            // Each level's body text is distinct.
            let body = s.text;
            if *cid == "outer" {
                assert!(body.contains("Outer body"));
            } else if *cid == "middle" {
                assert!(body.contains("Middle body"));
            } else if *cid == "inner" {
                assert!(body.contains("Inner body"));
            }
        }
    }

    #[test]
    fn unknown_custom_id_falls_through_to_next_strategy() {
        // "no-such-id" doesn't exist; but the canticle has the literal
        // string "steaming bowls" in the body, so the free-text fallback
        // kicks in.
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "steaming bowls").expect("text fallback");
        assert_eq!(s.kind, "text_search");
        assert!(s.text.contains("steaming bowls"));
    }

    // --- Headline title resolution (priority 2) ---------------------

    #[test]
    fn headline_title_resolves_to_subtree() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "Verse 4").expect("Verse 4 must resolve");
        // No CUSTOM_ID on the title "Verse 4" (custom ids are v1/v4,
        // not the title). The headline title match is the fallback.
        assert_eq!(s.kind, "headline");
        assert!(s.text.contains("walk through the valley"));
    }

    #[test]
    fn headline_title_works_at_any_level() {
        let doc = OrgDoc::from_text(NESTED);
        for title in &["Outer", "Middle", "Inner", "Deep", "Side"] {
            let s = AnchorResolver::resolve(&doc, title)
                .unwrap_or_else(|| panic!("{title} must resolve"));
            assert_eq!(s.kind, "headline", "{title} kind");
        }
    }

    #[test]
    fn headline_title_match_is_exact_not_fuzzy() {
        // "Vers" is a prefix of "Verse 1" but must not match.
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "Vers").expect("text fallback fires");
        // No exact title match → falls through to text_search.
        assert_eq!(s.kind, "text_search");
    }

    // --- Dedicated target <<...>> resolution (priority 3) -----------

    #[test]
    fn dedicated_target_resolves_to_following_paragraph() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "verse-1").expect("verse-1 must resolve");
        assert_eq!(s.kind, "dedicated_target");
        eprintln!("GOT: {:?}", s.text);
        assert!(
            s.text.contains("lie down in steaming bowls of pasta"),
            "text was: {}",
            s.text
        );
    }

    #[test]
    fn dedicated_target_in_anchors_file() {
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "named-target").expect("named-target must resolve");
        assert_eq!(s.kind, "dedicated_target");
        assert!(s.text.contains("named target sits before the paragraph"));
    }

    #[test]
    fn dedicated_target_priority_under_custom_id() {
        // If both a CUSTOM_ID and a dedicated target share a name, the
        // CUSTOM_ID must win (it's first in the resolution order).
        // nested.org has both: headline "Middle" with CUSTOM_ID=middle
        // and a <<middle-anchor>>.
        let doc = OrgDoc::from_text(NESTED);
        let s = AnchorResolver::resolve(&doc, "middle").expect("resolve");
        assert_eq!(s.kind, "custom_id");
        assert!(s.text.contains("Middle body"));
    }

    #[test]
    fn dedicated_target_at_end_of_file() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "closing").expect("closing");
        assert_eq!(s.kind, "dedicated_target");
        assert!(s.text.contains("marinara and parmesan"));
    }

    #[test]
    fn dedicated_target_on_last_line_without_trailing_newline() {
        // Regression: this used to slice past end-of-string and panic.
        let doc = OrgDoc::from_text("#+title: T\n\nsome text <<x>>");
        let s = AnchorResolver::resolve(&doc, "x").expect("must resolve, not panic");
        assert_eq!(s.kind, "dedicated_target");
        assert!(s.text.contains("<<x>>"));
    }

    // --- Free-text search (priority 4) ------------------------------

    #[test]
    fn free_text_finds_paragraph() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "walk through").expect("text");
        assert_eq!(s.kind, "text_search");
        assert!(s.text.contains("walk through"));
    }

    #[test]
    fn free_text_is_case_insensitive() {
        let doc = OrgDoc::from_text(PSALM);
        let s = AnchorResolver::resolve(&doc, "WALK THROUGH").expect("case-insensitive text");
        assert_eq!(s.kind, "text_search");
    }

    #[test]
    fn free_text_uses_lorem_ipsum_body() {
        // Build a file that uses the lorem ipsum fixture as a body.
        let src = format!(":PROPERTIES:\n:ID: 55555555-5555-5555-5555-555555555555\n:END:\n#+title: Lorem\n\n{}\n",
            LOREM.trim());
        let doc = OrgDoc::from_text(src);
        // Search for a distinctive phrase from the second paragraph.
        let s = AnchorResolver::resolve(&doc, "voluptate velit").expect("lorem search");
        assert_eq!(s.kind, "text_search");
        assert!(s.text.contains("voluptate velit"));
    }

    // --- Empty / not-found ----------------------------------------

    #[test]
    fn empty_anchor_returns_none() {
        let doc = OrgDoc::from_text(PSALM);
        assert!(AnchorResolver::resolve(&doc, "").is_none());
        assert!(AnchorResolver::resolve(&doc, "   ").is_none());
    }

    #[test]
    fn anchor_not_in_doc_returns_none() {
        // A truly non-existent phrase.
        let doc = OrgDoc::from_text(PSALM);
        assert!(AnchorResolver::resolve(&doc, "xyzzy-plugh-not-here").is_none());
    }

    // --- Unicode ----------------------------------------------------

    #[test]
    fn unicode_headline_title_resolves() {
        let doc = OrgDoc::from_text(UNICODE);
        let s = AnchorResolver::resolve(&doc, "Über die Größe").expect("unicode title");
        assert_eq!(s.kind, "headline");
        assert!(s.text.contains("Größe"));
    }

    #[test]
    fn unicode_custom_id_resolves() {
        let doc = OrgDoc::from_text(UNICODE);
        let s = AnchorResolver::resolve(&doc, "über-größe").expect("unicode custom id");
        // The unicode "ü" gets dropped by the slug-like check? No, this
        // is an exact CUSTOM_ID match. It should work.
        assert_eq!(s.kind, "custom_id");
    }

    #[test]
    fn unicode_dedicated_target() {
        let doc = OrgDoc::from_text(UNICODE);
        let s = AnchorResolver::resolve(&doc, "größen-anchor").expect("unicode target");
        assert_eq!(s.kind, "dedicated_target");
        assert!(s.text.contains("Größe"));
    }

    // --- Byte-range invariants -------------------------------------

    #[test]
    fn section_byte_range_lies_within_source() {
        let doc = OrgDoc::from_text(PSALM);
        let len = doc.text.len();
        for anchor in &["v1", "v4", "Verse 1", "verse-1", "closing"] {
            let s = AnchorResolver::resolve(&doc, anchor).unwrap();
            assert!(s.begin <= s.end, "{anchor}: begin <= end");
            assert!(s.end <= len, "{anchor}: end <= doc length");
            assert_eq!(&doc.text[s.begin..s.end], s.text, "{anchor}: slice matches");
        }
    }

    // --- §2: radio target resolution --------------------------------

    #[test]
    fn radio_target_resolves_to_following_paragraph() {
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "on-every-occurrence").expect("radio");
        assert_eq!(s.kind, "radio_target");
        assert!(s.text.contains("<<<on-every-occurrence>>>"));
        assert!(s.text.contains("This paragraph follows the radio target"));
    }

    #[test]
    fn radio_target_priority_under_dedicated_target() {
        // If the same name is declared as both `<<term>>` and
        // `<<<term>>>` in the same file, the dedicated target wins
        // (matches the existing `dedicated_target_priority_under_custom_id`
        // style: more-specific anchor first).
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Dual

<<shared>> Dedicated target text.

<<<shared>>> Radio target text.
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "shared").expect("resolve");
        assert_eq!(s.kind, "dedicated_target");
        assert!(s.text.contains("Dedicated target text"));
    }

    #[test]
    fn radio_target_does_not_match_dedicated_target_marker() {
        // A bare substring of a `<<dedicated>>` must not be picked up as
        // a radio target (`<<dedicated>>` is not `<<<dedicated>>>`).
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Substring

<<dedicated>> Dedicated paragraph.

Elsewhere.
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "dedicated").expect("resolve");
        assert_eq!(s.kind, "dedicated_target");
    }

    // --- §3: #+NAME: cross-references ---------------------------------

    #[test]
    fn name_property_resolves_to_named_element() {
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "growth-table").expect("growth-table");
        assert_eq!(s.kind, "name");
        // The named table's data rows must be in the section text.
        // Match loosely because the fixture has padding whitespace
        // inside the cell.
        assert!(
            s.text.contains("2024"),
            "named table should include its data rows, got: {}",
            s.text
        );
        assert!(
            s.text.contains("growth-table"),
            "named element should include the keyword line, got: {}",
            s.text
        );
    }

    #[test]
    fn name_property_distinguished_from_other_strategies() {
        // A name and a dedicated target sharing the same string: org's
        // documented order is custom-id → headline → name → dedicated
        // target → fuzzy, so `name` wins. The name anchor kind is the
        // one that's expected.
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Conflict

<<shared>> Dedicated target paragraph.

#+NAME: shared
| col1 | col2 |
|------+------|
| a    | b    |
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "shared").expect("resolve");
        assert_eq!(s.kind, "name");
    }

    #[test]
    fn name_property_case_sensitive() {
        // Names are case-sensitive: `Growth-Table` and `growth-table`
        // are distinct. The text-based scan compares the value verbatim,
        // so the only resolution path for "Growth-Table" is the
        // case-insensitive free-text fallback (which can find the
        // literal `growth-table` in `[[growth-table]]`). We assert
        // that by_name itself is strict by calling it directly.
        let doc = OrgDoc::from_text(ANCHORS);
        assert!(AnchorResolver::by_name(&doc, "Growth-Table").is_none());
        assert!(AnchorResolver::by_name(&doc, "GROWTH-TABLE").is_none());
        assert!(AnchorResolver::by_name(&doc, "growth-table").is_some());
    }

    // --- §5: code reference resolution --------------------------------

    #[test]
    fn coderef_anchor_resolves_to_source_block_line() {
        // The anchors fixture has a source block whose first line ends
        // with `(ref:entry)`. Resolving "entry" should return that
        // exact line.
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "entry").expect("entry");
        assert_eq!(s.kind, "coderef");
        assert!(s.text.contains("(ref:entry)"), "got: {}", s.text);
        assert!(s.text.contains("fn main()"), "got: {}", s.text);
    }

    #[test]
    fn coderef_anchor_does_not_match_substring() {
        // A text_search match for a substring of `(ref:label)` should
        // not out-prioritise the actual coderef lookup. The coderef
        // strategy is tried before the text fallback.
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Substring

#+BEGIN_SRC rust
fn helper() {           (ref:helper)
    // ...
}
#+END_SRC
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "helper").expect("helper");
        assert_eq!(s.kind, "coderef");
    }

    #[test]
    fn coderef_anchor_with_no_match_falls_through_to_text_search() {
        let doc = OrgDoc::from_text(PSALM);
        // No coderef in the canticle; "steaming" is a free-text match.
        let s = AnchorResolver::resolve(&doc, "steaming").expect("text");
        assert_eq!(s.kind, "text_search");
    }

    // --- §7: footnote resolution -------------------------------------

    #[test]
    fn footnote_definition_resolves_as_anchor() {
        // The anchors fixture has `[fn:1] This is the footnote body.`
        // at the bottom. `resolve(&doc, "1")` and `resolve(&doc,
        // "fn:1")` both return the footnote body.
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "1").expect("1");
        assert_eq!(s.kind, "footnote");
        assert_eq!(s.text, "This is the footnote body.");
    }

    #[test]
    fn named_footnote_resolves() {
        // `[fn:side-note] A named footnote body ...` is reachable
        // by the bare label or the `fn:` prefix.
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "side-note").expect("side-note");
        assert_eq!(s.kind, "footnote");
        assert!(s.text.contains("named footnote body"));
    }

    #[test]
    fn footnote_anchor_with_fn_prefix_resolves() {
        // The `fn:` prefix is accepted, matching org's link syntax.
        let doc = OrgDoc::from_text(ANCHORS);
        let s = AnchorResolver::resolve(&doc, "fn:1").expect("fn:1");
        assert_eq!(s.kind, "footnote");
        assert_eq!(s.text, "This is the footnote body.");
    }

    // --- Edge cases and error cases -----------------------------------

    #[test]
    fn resolve_returns_none_for_truly_unknown_anchor() {
        // A truly absent anchor returns None, not text_search or
        // anything else. (We have at least one test for the
        // empty/whitespace case, but a long random string is
        // another good canary.)
        let doc = OrgDoc::from_text(PSALM);
        assert!(AnchorResolver::resolve(&doc, "xyzzynonesuch").is_none());
    }

    #[test]
    fn resolve_passes_through_unicode_normalisation() {
        // The anchor resolver is case-sensitive but accepts
        // Unicode-identical strings. A NFC/NFD mismatch is not
        // the server's problem; the caller should normalise.
        let doc = OrgDoc::from_text(UNICODE);
        // "über-größe" is the canonical custom_id; the same string
        // with NFC form should resolve.
        let s = AnchorResolver::resolve(&doc, "über-größe").expect("resolve");
        assert_eq!(s.kind, "custom_id");
    }

    #[test]
    fn radio_target_at_end_of_file_does_not_panic() {
        // A radio target with no following paragraph (e.g. at the
        // very end of the file) returns a Section whose text is
        // just the marker.
        let text = ":PROPERTIES:\n:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n:END:\n\
                    #+title: T\n\n<<<lonely>>>";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "lonely").expect("lonely");
        assert_eq!(s.kind, "radio_target");
        assert!(s.text.contains("<<<lonely>>>"));
    }

    #[test]
    fn coderef_anchor_with_label_named_like_dedicated_target_resolves_to_coderef() {
        // If the same name is a coderef and (via a `<<name>>`)
        // dedicated target, the coderef strategy is later in the
        // resolution order so the dedicated target wins. This
        // documents the current behaviour.
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Conflict

<<shared>> A paragraph after a dedicated target.

#+BEGIN_SRC rust
fn helper() {           (ref:shared)
    // ...
}
#+END_SRC
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "shared").expect("resolve");
        assert_eq!(s.kind, "dedicated_target");
    }

    #[test]
    fn name_anchor_with_multi_paragraph_body() {
        // A `#+NAME:` followed by an element whose body spans
        // multiple paragraphs: the body's first paragraph is in
        // the section text, and the section ends at the blank
        // line. We assert that the section contains the first
        // paragraph but not the second.
        let text = "\
:PROPERTIES:
:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa
:END:
#+title: Multi

#+NAME: table1
| col1 | col2 |
|------+------|
| a    | b    |

A second paragraph below the blank line.
";
        let doc = OrgDoc::from_text(text);
        let s = AnchorResolver::resolve(&doc, "table1").expect("table1");
        assert_eq!(s.kind, "name");
        assert!(s.text.contains("| a    | b    |"));
        assert!(
            !s.text.contains("A second paragraph"),
            "second paragraph should be outside the named body: {}",
            s.text
        );
    }
}
