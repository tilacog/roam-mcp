//! orgize wrappers: parse a `.org` file, expose nodes and byte ranges.

use std::path::Path;
use std::sync::Arc;

use orgize::ast::{Document, Headline};
use orgize::rowan::ast::AstNode as _;
use orgize::SyntaxKind;
use orgize::{Org, ParseConfig};

/// A parsed org document plus its original text.
///
/// The original `Org` is wrapped in `Arc` so cloning an `OrgDoc` is cheap
/// (it just bumps the refcount on the green tree).
pub struct OrgDoc {
    pub text: Arc<str>,
    pub org: Arc<Org>,
}

impl Clone for OrgDoc {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            org: self.org.clone(),
        }
    }
}

impl OrgDoc {
    /// Parse a `.org` file from disk. Returns an error if it cannot be read.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the file cannot be read.
    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_text(text))
    }

    /// Parse from a string of org text.
    pub fn from_text<S: Into<Arc<str>>>(text: S) -> Self {
        let text = text.into();
        let config = ParseConfig::default();
        let org = Arc::new(config.parse(&text));
        Self { text, org }
    }

    /// The document root.
    #[must_use]
    pub fn document(&self) -> Document {
        self.org.document()
    }

    /// All headlines in the file (depth-first order).
    #[must_use]
    pub fn headlines(&self) -> Vec<Headline> {
        let mut out = Vec::new();
        for n in self.document().syntax().descendants() {
            if n.kind() == SyntaxKind::HEADLINE {
                if let Some(h) = Headline::cast(n) {
                    out.push(h);
                }
            }
        }
        out
    }

    /// Find a headline by its `:ID:` property, if any.
    #[must_use]
    pub fn headline_by_id(&self, id: &str) -> Option<Headline> {
        self.headlines().into_iter().find(|h| {
            h.properties()
                .and_then(|p| p.get("ID"))
                .is_some_and(|v| v.trim() == id)
        })
    }

    /// Find a headline by `CUSTOM_ID`.
    #[must_use]
    pub fn headline_by_custom_id(&self, custom_id: &str) -> Option<Headline> {
        self.headlines().into_iter().find(|h| {
            h.properties()
                .and_then(|p| p.get("CUSTOM_ID"))
                .is_some_and(|v| v.trim() == custom_id)
        })
    }

    /// Extract a sub-section of the file as a string slice. `start`/`end`
    /// are byte offsets into the original `text`.
    #[must_use]
    pub fn slice(&self, start: usize, end: usize) -> &str {
        let s = start.min(self.text.len());
        let e = end.min(self.text.len()).max(s);
        &self.text[s..e]
    }

    /// Byte range covering `h` and its entire subtree (headline line, body,
    /// and nested headlines): from the headline start to the start of the
    /// next sibling headline/section, or to end-of-file when the headline
    /// closes the document.
    ///
    /// This is the single definition of "where a subtree ends" — anchor
    /// resolution, content reads, and the write tools all use it.
    #[must_use]
    pub fn subtree_range(&self, h: &Headline) -> (usize, usize) {
        let start: usize = h.start().into();
        let end = next_sibling_start(h).unwrap_or(self.text.len());
        (start, end)
    }
}

/// Start offset of the next sibling headline/section after `h`, if any.
fn next_sibling_start(h: &Headline) -> Option<usize> {
    let mut n = h.syntax().next_sibling()?;
    loop {
        match n.kind() {
            SyntaxKind::HEADLINE | SyntaxKind::SECTION => {
                return Some(n.text_range().start().into());
            }
            _ => n = n.next_sibling()?,
        }
    }
}

/// Result of an anchor-resolution request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The text content of the resolved section (paragraph, headline
    /// subtree, or matched range).
    pub text: String,

    /// What kind of anchor produced this: dedicated target, custom id,
    /// headline, or free text.
    pub kind: String,

    /// Byte range in the source file.
    pub begin: usize,
    pub end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures live in tests/fixtures/text/ and are compiled into the
    // binary so the tests are self-contained.

    const PSALM: &str = include_str!("../../tests/fixtures/text/fsm_canticle.org");
    const NESTED: &str = include_str!("../../tests/fixtures/text/nested.org");
    const MULTI: &str = include_str!("../../tests/fixtures/text/multi_headlines.org");
    #[allow(dead_code)]
    const ANCHORS: &str = include_str!("../../tests/fixtures/text/with_anchors.org");
    #[allow(dead_code)]
    const LINKS: &str = include_str!("../../tests/fixtures/text/with_links.org");
    #[allow(dead_code)]
    const REFS: &str = include_str!("../../tests/fixtures/text/with_refs.org");
    const UNICODE: &str = include_str!("../../tests/fixtures/text/unicode.org");
    const EMPTY: &str = include_str!("../../tests/fixtures/text/empty.org");
    const NO_ID: &str = include_str!("../../tests/fixtures/text/no_id.org");
    const LOREM: &str = include_str!("../../tests/fixtures/text/lorem.txt");

    // --- headlines() iteration ---------------------------------------

    #[test]
    fn psalm_has_three_headlines() {
        let doc = OrgDoc::from_text(PSALM);
        let h = doc.headlines();
        assert_eq!(h.len(), 3, "expected Verse 1, Verse 4, Closing");
        assert_eq!(h[0].title_raw().trim(), "Verse 1");
        assert_eq!(h[1].title_raw().trim(), "Verse 4");
        assert_eq!(h[2].title_raw().trim(), "Closing");
    }

    #[test]
    fn nested_has_five_headlines_at_varying_depths() {
        let doc = OrgDoc::from_text(NESTED);
        let h = doc.headlines();
        let titles: Vec<String> = h.iter().map(|x| x.title_raw().trim().to_string()).collect();
        assert_eq!(
            titles,
            vec!["Outer", "Middle", "Inner", "Deep", "Side"],
            "depth-first iteration must include every level"
        );
        // The first headline (Outer) is level 1; Inner is level 3.
        assert_eq!(h[0].level(), 1);
        assert_eq!(h[2].level(), 3);
    }

    #[test]
    fn empty_file_has_no_headlines() {
        let doc = OrgDoc::from_text(EMPTY);
        assert!(doc.headlines().is_empty());
    }

    #[test]
    fn no_id_file_has_headlines_but_no_resolvable_id() {
        let doc = OrgDoc::from_text(NO_ID);
        let h = doc.headlines();
        assert_eq!(h.len(), 2);
        for hl in &h {
            assert!(hl.properties().and_then(|p| p.get("ID")).is_none());
        }
    }

    // --- headline_by_id / headline_by_custom_id --------------------

    #[test]
    fn headline_by_id_finds_correct_level() {
        let doc = OrgDoc::from_text(NESTED);
        let h = doc
            .headline_by_id("eeeeee03-eeee-eeee-eeee-eeeeeeeeeeee")
            .expect("inner id");
        assert_eq!(h.title_raw().trim(), "Inner");
        assert_eq!(h.level(), 3);
    }

    #[test]
    fn headline_by_id_returns_none_for_missing() {
        let doc = OrgDoc::from_text(NESTED);
        assert!(doc
            .headline_by_id("00000000-0000-0000-0000-000000000000")
            .is_none());
    }

    #[test]
    fn headline_by_custom_id_finds_correct_subtree() {
        let doc = OrgDoc::from_text(NESTED);
        let h = doc
            .headline_by_custom_id("middle")
            .expect("middle custom id");
        assert_eq!(h.title_raw().trim(), "Middle");
        assert_eq!(h.level(), 2);
    }

    #[test]
    fn headline_by_custom_id_distinguishes_levels() {
        let doc = OrgDoc::from_text(NESTED);
        // outer/middle/inner all have unique custom ids.
        for cid in &["outer", "middle", "inner"] {
            assert!(doc.headline_by_custom_id(cid).is_some(), "{cid}");
        }
        // "deep" is the only headline without a custom id, so it must
        // not resolve to a custom_id lookup.
        assert!(doc.headline_by_custom_id("deep").is_none());
    }

    #[test]
    fn multi_headlines_each_headline_id_resolves() {
        let doc = OrgDoc::from_text(MULTI);
        // File-level ID is in the drawer, not on a headline, so
        // headline_by_id must not find it. Headline IDs do resolve.
        for id in &[
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
        ] {
            assert!(doc.headline_by_id(id).is_some(), "id {id}");
        }
        // The file-level id should NOT be a headline id.
        assert!(doc
            .headline_by_id("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .is_none());
    }

    #[test]
    fn multi_headlines_file_level_id_is_in_document_properties() {
        let doc = OrgDoc::from_text(MULTI);
        let id = doc
            .document()
            .properties()
            .and_then(|p| p.get("ID"))
            .map(|t| t.to_string());
        assert_eq!(id.as_deref(), Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"));
    }

    // --- slice -------------------------------------------------------

    #[test]
    fn slice_returns_exact_bytes() {
        let doc = OrgDoc::from_text(PSALM);
        // The file starts with the property drawer.
        let head = doc.slice(0, 13);
        assert_eq!(head, ":PROPERTIES:\n");
    }

    #[test]
    fn slice_clamps_out_of_bounds() {
        let doc = OrgDoc::from_text(PSALM);
        let huge = doc.slice(0, 10_000_000);
        assert_eq!(huge.len(), PSALM.len());
    }

    // --- subtree_range -------------------------------------------------

    #[test]
    fn subtree_range_ends_at_next_sibling() {
        let doc = OrgDoc::from_text(PSALM);
        let headlines = doc.headlines();
        let (begin, end) = doc.subtree_range(&headlines[0]);
        let sub = doc.slice(begin, end);
        assert!(sub.starts_with("* Verse 1"));
        assert!(!sub.contains("* Verse 4"), "must stop before next sibling");
    }

    #[test]
    fn subtree_range_of_last_headline_extends_to_eof() {
        let doc = OrgDoc::from_text(PSALM);
        let last = doc.headlines().into_iter().last().expect("last headline");
        let (_, end) = doc.subtree_range(&last);
        assert_eq!(end, doc.text.len());
    }

    #[test]
    fn subtree_range_includes_nested_children_but_not_siblings() {
        // "Middle" (level 2) contains "Inner" and "Deep"; "Side" is its
        // level-2 sibling and must be excluded.
        let doc = OrgDoc::from_text(NESTED);
        let middle = doc
            .headlines()
            .into_iter()
            .find(|h| h.title_raw().trim() == "Middle")
            .expect("middle");
        let (begin, end) = doc.subtree_range(&middle);
        let sub = doc.slice(begin, end);
        assert!(sub.contains("*** Inner"));
        assert!(sub.contains("**** Deep"));
        assert!(!sub.contains("** Side"), "sibling must be excluded");
    }

    // --- parsing the file-level / document properties ---------------

    #[test]
    fn file_level_title_comes_from_keyword() {
        let doc = OrgDoc::from_text(PSALM);
        let title = doc.document().title().expect("title keyword");
        assert_eq!(title, "Pastafarian Canticle");
    }

    #[test]
    fn file_level_aliases_decoded() {
        let doc = OrgDoc::from_text(PSALM);
        let aliases = doc
            .document()
            .properties()
            .and_then(|p| p.get("ROAM_ALIASES"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        // The drawer stores it as a single Lisp-encoded string.
        let parsed = parse_roam_aliases(&aliases);
        assert_eq!(parsed, vec!["Ps FSM", "The Noodly Psalm"]);
    }

    #[test]
    fn file_level_refs_decoded() {
        let doc = OrgDoc::from_text(PSALM);
        let raw = doc
            .document()
            .properties()
            .and_then(|p| p.get("ROAM_REFS"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        // The actual encoding in the drawer has surrounding parens for
        // the list. Handle both encodings.
        let parsed = parse_roam_refs(&raw);
        assert!(
            parsed.contains(&"https://en.wikipedia.org/wiki/Flying_Spaghetti_Monster".to_string())
        );
    }

    #[test]
    fn unicode_title_preserved() {
        let doc = OrgDoc::from_text(UNICODE);
        let title = doc.document().title().expect("title");
        assert_eq!(title, "Café — Über die Größe");
    }

    // --- headline tags & priority -----------------------------------

    #[test]
    fn psalm_verse_1_has_no_tags() {
        let doc = OrgDoc::from_text(PSALM);
        let h = doc.headlines().into_iter().next().expect("first headline");
        let tags: Vec<String> = h.tags().map(|t| t.to_string()).collect();
        assert!(tags.is_empty(), "no inline tags on Verse 1, got: {tags:?}");
    }

    // --- helpers used by the tests above ----------------------------

    fn parse_roam_aliases(s: &str) -> Vec<String> {
        // Drawer value is a printed lisp list of quoted strings.
        let trimmed = s.trim();
        let inner = trimmed
            .strip_prefix('(')
            .and_then(|x| x.strip_suffix(')'))
            .unwrap_or(trimmed);
        inner
            .split('"')
            .filter(|p| !p.trim().is_empty() && !p.trim_start().starts_with('('))
            .map(|p| p.trim().to_string())
            .collect()
    }

    fn parse_roam_refs(s: &str) -> Vec<String> {
        // Refs are space-separated tokens; URLs and @keys both fit.
        s.split_whitespace()
            .map(|x| x.trim_matches(|c| c == '(' || c == ')').to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    // --- lorem ipsum fixture integrity -------------------------------

    #[test]
    fn lorem_fixture_has_paragraphs() {
        // We split on blank lines and require at least 3 paragraphs.
        let paras: Vec<&str> = LOREM.split("\n\n").collect();
        assert!(
            paras.len() >= 3,
            "expected 3+ paragraphs, got {}",
            paras.len()
        );
    }

    // --- links fixture ----------------------------------------------

    #[test]
    fn links_fixture_has_file_level_node() {
        let doc = OrgDoc::from_text(LINKS);
        let id = doc
            .document()
            .properties()
            .and_then(|p| p.get("ID"))
            .map(|t| t.to_string());
        assert_eq!(id.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(doc.document().title().as_deref(), Some("Link variety"));
    }

    // --- refs fixture -----------------------------------------------

    #[test]
    fn refs_fixture_decodes_url_and_citekey() {
        let doc = OrgDoc::from_text(REFS);
        let raw = doc
            .document()
            .properties()
            .and_then(|p| p.get("ROAM_REFS"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        // The drawer value is whitespace-separated, not a Lisp list.
        // Confirm both ref types are present.
        assert!(raw.contains("https://example.com/article"));
        assert!(raw.contains("@nora2023"));
        // Sanity: also round-trip through the emacsql decoder as if
        // emacsql had printed a list, to mirror what the SQLite path
        // would see.
        let encoded = format!(
            "(\"{}\" \"{}\")",
            "https://example.com/article", "@nora2023"
        );
        let parsed = crate::index::sqlite::emacsql::unlisp(&encoded);
        let arr = parsed.as_array().expect("array");
        let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(items.len(), 2);
        assert!(items.contains(&"https://example.com/article"));
        assert!(items.contains(&"@nora2023"));
    }

    // --- anchors fixture -------------------------------------------

    #[test]
    fn anchors_fixture_has_expected_headlines() {
        let doc = OrgDoc::from_text(ANCHORS);
        let h = doc.headlines();
        let titles: Vec<String> = h.iter().map(|x| x.title_raw().trim().to_string()).collect();
        assert_eq!(
            titles,
            vec![
                "First section",
                "Second section",
                "Third section with code",
                "Fourth section with radio target",
                "Fifth section with named table",
            ],
            "depth-first iteration over the anchor fixture"
        );
        // The second headline has :CUSTOM_ID: second.
        let second = &h[1];
        let cid = second
            .properties()
            .and_then(|p| p.get("CUSTOM_ID"))
            .map(|t| t.to_string());
        assert_eq!(cid.as_deref(), Some("second"));
    }
}
