//! Filetag parsing and serialization — the single source of truth for the
//! `#+filetags:` keyword (org-roam v2) and the v1 `#+ROAM_TAGS:` keyword.
//!
//! These are pure functions over a file's text. The scanner
//! (`crate::index::scan`) re-exports them so both backends share the
//! exact same parsing behaviour, and the filetag-management MCP tools
//! (`tools::query`, `tools::write`) call into them so on-disk truth
//! wins over the in-memory index.
//!
//! Tag syntax: org-roam v2 `#+filetags: :a:b:` uses colon-delimited
//! tags. A literal `::` is an *empty* segment between two tags, not a
//! single tag — `#+filetags: :a::b:` parses to `["a", "b"]`. The v1
//! `#+ROAM_TAGS:` keyword is a quoted/whitespace list
//! (`#+ROAM_TAGS: "multi word" single`).

use crate::org::edit;

/// Trimmed values of every `#+key:` keyword line in `text`, matching the
/// key case-insensitively.
#[must_use]
pub fn keyword_values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
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

/// Parse a `#+filetags:` value (`:a:b:`) into its tags. Splits on `:`;
/// empty segments (e.g. the `::` in `:a::b:`) simply contribute nothing.
#[must_use]
pub fn parse_filetags_value(s: &str) -> Vec<String> {
    s.split(':')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Parse a quoted/whitespace list (`ROAM_ALIASES`, `ROAM_REFS`, v1
/// `ROAM_TAGS`). Tokens are split on whitespace outside quotes; a
/// double-quoted run is one token even when it contains spaces.
#[must_use]
pub fn parse_string_list(s: &str) -> Vec<String> {
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

/// File-level tags: `#+filetags: :a:b:` plus the org-roam v1 form
/// `#+ROAM_TAGS: a b "multi word"`. Duplicates removed, order preserved.
#[must_use]
pub fn file_level_tags(text: &str) -> Vec<String> {
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

/// Deduplicate `tags`, preserving first-seen order and dropping empty
/// strings. Used to normalize `add_tag` / `set_tags` inputs.
#[must_use]
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tags {
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.clone()) {
            out.push(t.clone());
        }
    }
    out
}

/// Reconcile a file's tags to exactly `new_tags`: sets `#+filetags:` to
/// the rendered value (removing the keyword when `new_tags` is empty)
/// and removes any v1 `#+ROAM_TAGS:` keyword so the merged
/// [`file_level_tags`] view afterward equals `new_tags` exactly.
///
/// This guarantees no v1/v2 drift: a file that previously carried tags via
/// `#+ROAM_TAGS:` is migrated to the v2 `#+filetags:` form in place.
pub fn apply_file_tags(text: &mut String, new_tags: &[String]) {
    edit::set_keyword(text, "filetags", edit::render_filetags(new_tags).as_deref());
    edit::set_keyword(text, "roam_tags", None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_delimited_filetags() {
        assert_eq!(
            parse_filetags_value(":work:urgent:"),
            vec!["work", "urgent"]
        );
        assert_eq!(parse_filetags_value(":one:"), vec!["one"]);
    }

    #[test]
    fn double_colon_is_an_empty_segment() {
        // `::` is an empty segment between two tags, not a tag itself.
        assert_eq!(parse_filetags_value(":a::b:"), vec!["a", "b"]);
        assert_eq!(parse_filetags_value(":::"), Vec::<String>::new());
        assert_eq!(parse_filetags_value(":"), Vec::<String>::new());
    }

    #[test]
    fn filetags_keyword_is_case_insensitive() {
        assert_eq!(
            file_level_tags("#+FILETAGS: :one:two:\n"),
            vec!["one", "two"]
        );
        assert_eq!(file_level_tags("#+Filetags: :one:\n"), vec!["one"]);
    }

    #[test]
    fn missing_tag_keywords_yield_empty() {
        assert!(file_level_tags("#+title: No tags here\n").is_empty());
        assert!(file_level_tags("").is_empty());
    }

    #[test]
    fn parses_roam_tags_keyword() {
        assert_eq!(
            file_level_tags("#+ROAM_TAGS: hub projects\n"),
            vec!["hub", "projects"]
        );
    }

    #[test]
    fn roam_tags_supports_quoted_multiword_tags() {
        assert_eq!(
            file_level_tags("#+roam_tags: \"multi word\" single\n"),
            vec!["multi word", "single"]
        );
    }

    #[test]
    fn filetags_and_roam_tags_merge_without_duplicates() {
        let text = "#+filetags: :work:\n#+ROAM_TAGS: work extra\n";
        assert_eq!(file_level_tags(text), vec!["work", "extra"]);
    }

    #[test]
    fn file_level_tags_preserves_order() {
        let text = "#+filetags: :zebra:alpha:\n#+ROAM_TAGS: mango\n";
        assert_eq!(file_level_tags(text), vec!["zebra", "alpha", "mango"]);
    }

    #[test]
    fn render_filetags_round_trips() {
        let tags = vec!["a".to_string(), "b".to_string(), "multi word".to_string()];
        let rendered = edit::render_filetags(&tags).expect("non-empty");
        assert_eq!(rendered, ":a:b:multi word:");
        // Parsing it back yields the original list.
        assert_eq!(parse_filetags_value(&rendered), tags);
    }

    #[test]
    fn render_filetags_empty_is_none() {
        assert!(edit::render_filetags(&[]).is_none());
    }

    #[test]
    fn normalize_dedupes_preserving_first_seen_order() {
        let raw = vec![
            "b".to_string(),
            "a".to_string(),
            "b".to_string(),
            String::new(),
        ];
        assert_eq!(normalize_tags(&raw), vec!["b", "a"]);
    }

    #[test]
    fn normalize_drops_empties() {
        let raw = vec![String::new(), "x".to_string(), String::new()];
        assert_eq!(normalize_tags(&raw), vec!["x"]);
    }

    #[test]
    fn apply_file_tags_replaces_existing_filetags() {
        let mut text =
            ":PROPERTIES:\n:ID: abc\n:END:\n#+title: T\n#+filetags: :a:b:\n\nBody\n".to_string();
        apply_file_tags(&mut text, &["x".to_string(), "y".to_string()]);
        assert!(text.contains("#+filetags: :x:y:"));
        assert!(!text.contains(":a:b:"));
        assert_eq!(file_level_tags(&text), vec!["x", "y"]);
    }

    #[test]
    fn apply_file_tags_empty_removes_keyword() {
        let mut text = "#+title: T\n#+filetags: :a:b:\n\nBody\n".to_string();
        apply_file_tags(&mut text, &[]);
        assert!(!text.contains("#+filetags"));
        assert!(file_level_tags(&text).is_empty());
    }

    #[test]
    fn apply_file_tags_strips_v1_roam_tags() {
        let mut text = "#+title: T\n#+filetags: :a:\n#+ROAM_TAGS: extra\n\nBody\n".to_string();
        apply_file_tags(&mut text, &["a".to_string(), "new".to_string()]);
        // The v1 keyword is gone, and the merged view equals exactly the
        // requested set — no drift.
        assert!(!text.contains("ROAM_TAGS"));
        assert!(text.contains("#+filetags: :a:new:"));
        assert_eq!(file_level_tags(&text), vec!["a", "new"]);
    }

    #[test]
    fn apply_file_tags_migrates_v1_only_file_to_v2() {
        // A file that only carried v1 tags is migrated to #+filetags:.
        let mut text = "#+title: T\n#+ROAM_TAGS: legacy1 legacy2\n\nBody\n".to_string();
        apply_file_tags(&mut text, &["legacy1".to_string(), "legacy2".to_string()]);
        assert!(text.contains("#+filetags: :legacy1:legacy2:"));
        assert!(!text.contains("ROAM_TAGS"));
        assert_eq!(file_level_tags(&text), vec!["legacy1", "legacy2"]);
    }

    #[test]
    fn apply_file_tags_inserts_keyword_into_preamble_when_absent() {
        // A file with no existing tags gets the keyword inserted in the
        // preamble, above the first headline.
        let mut text = ":PROPERTIES:\n:ID: x\n:END:\n#+title: T\n\nBody\n".to_string();
        apply_file_tags(&mut text, &["one".to_string(), "two".to_string()]);
        let tags_at = text.find("#+filetags").expect("keyword inserted");
        let body_at = text.find("Body").unwrap();
        assert!(tags_at < body_at, "got: {text}");
        assert_eq!(file_level_tags(&text), vec!["one", "two"]);
    }

    #[test]
    fn apply_file_tags_preserves_body_and_drawer() {
        let mut text =
            ":PROPERTIES:\n:ID:       abc-123\n:END:\n#+title: Original\n#+filetags: :a:b:\n\nBody line one.\nBody line two.\n"
                .to_string();
        apply_file_tags(&mut text, &["z".to_string()]);
        assert!(text.contains(":ID:       abc-123"));
        assert!(text.contains("#+title: Original"));
        assert!(text.contains("Body line one."));
        assert!(text.contains("Body line two."));
        assert!(text.contains("#+filetags: :z:"));
    }

    #[test]
    fn apply_file_tags_idempotent() {
        let mut text = "#+title: T\n#+filetags: :a:b:\n\nBody\n".to_string();
        let want = vec!["a".to_string(), "b".to_string()];
        apply_file_tags(&mut text, &want);
        let mut once = text.clone();
        apply_file_tags(&mut once, &want);
        assert_eq!(text, once, "applying twice must be a no-op");
    }

    #[test]
    fn parse_string_list_quoted_and_unquoted() {
        assert_eq!(
            parse_string_list("\"Ps FSM\" \"The Noodly Psalm\""),
            vec!["Ps FSM", "The Noodly Psalm"]
        );
        assert_eq!(
            parse_string_list("https://example.com @nora2023"),
            vec!["https://example.com", "@nora2023"]
        );
        assert!(parse_string_list("").is_empty());
        assert_eq!(parse_string_list("\"a b\" c"), vec!["a b", "c"]);
    }

    #[test]
    fn keyword_values_collects_case_insensitively() {
        let text = "#+TITLE: A\n#+title: B\n#+roam_key: k1\n#+ROAM_KEY: k2\n";
        assert_eq!(keyword_values(text, "title"), vec!["A", "B"]);
        assert_eq!(keyword_values(text, "roam_key"), vec!["k1", "k2"]);
    }
}
