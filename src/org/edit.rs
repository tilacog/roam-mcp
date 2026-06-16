//! In-place editing primitives for org text.
//!
//! These are pure functions over a file's text — no IO, no MCP types — so
//! they can be unit-tested in isolation. The write tools (`update_node`,
//! `rename_node`, `prepend_to_node`, ...) compose them.
//!
//! Everything here operates on the file-level *preamble*: the region above
//! the first headline, holding the top `:PROPERTIES:` drawer and the
//! `#+keyword:` lines (`#+title:`, `#+filetags:`, ...). This is the part of
//! a node that carries its metadata; the rest is free-form body text.
//!
//! All edits round-trip the file through a `split('\n')` / `join('\n')`
//! pair, which preserves the trailing-newline structure exactly.

/// Apply `f` to the file's lines and write the result back. Splitting and
/// re-joining on `'\n'` round-trips the trailing newline: a file ending in
/// `'\n'` keeps its final empty element, so the joined text is byte-identical
/// when `f` makes no change.
fn edit_lines(text: &mut String, f: impl FnOnce(&mut Vec<String>)) {
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    f(&mut lines);
    *text = lines.join("\n");
}

/// True for a line that opens an org headline: one or more leading `*`
/// followed by a space. Headlines must start at column 0.
fn is_headline(line: &str) -> bool {
    let stars = line.chars().take_while(|&c| c == '*').count();
    stars >= 1 && line[stars..].starts_with(' ')
}

/// Index of the first headline line, or `lines.len()` when there is none.
/// Everything before it is the preamble.
fn first_headline_idx(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|l| is_headline(l))
        .unwrap_or(lines.len())
}

/// `(start, end)` line indices of the top `:PROPERTIES:` ... `:END:` drawer
/// within the preamble, if present. `start` is the `:PROPERTIES:` line,
/// `end` the `:END:` line.
fn drawer_bounds(lines: &[String], preamble_end: usize) -> Option<(usize, usize)> {
    let start = lines[..preamble_end]
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(":PROPERTIES:"))?;
    let end = lines[start + 1..preamble_end]
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(":END:"))
        .map(|i| i + start + 1)?;
    Some((start, end))
}

/// The property key of a drawer line like `:KEY: value`, if it is one.
fn drawer_key(line: &str) -> Option<&str> {
    let t = line.trim();
    let rest = t.strip_prefix(':')?;
    let idx = rest.find(':')?;
    Some(&rest[..idx])
}

/// The keyword of a line like `#+title: value`, if it is one.
fn keyword_key(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("#+")?;
    let idx = rest.find(':')?;
    Some(&rest[..idx])
}

/// Set, replace, or remove a `#+key:` keyword line in the preamble.
///
/// `Some(v)` sets it (replacing any existing line, case-insensitively);
/// `None` removes it. A new `#+title:` lands right after the drawer; other
/// new keywords land after the last existing keyword (or after the drawer).
pub fn set_keyword(text: &mut String, key: &str, value: Option<&str>) {
    edit_lines(text, |lines| {
        let pe = first_headline_idx(lines);
        let existing =
            (0..pe).find(|&i| keyword_key(&lines[i]).is_some_and(|k| k.eq_ignore_ascii_case(key)));
        match (existing, value) {
            (Some(i), Some(v)) => lines[i] = format!("#+{key}: {v}"),
            (Some(i), None) => {
                lines.remove(i);
            }
            (None, Some(v)) => {
                let at = keyword_insertion_idx(lines, pe, key);
                lines.insert(at, format!("#+{key}: {v}"));
            }
            (None, None) => {}
        }
    });
}

/// Where a brand-new keyword line should be inserted in the preamble.
fn keyword_insertion_idx(lines: &[String], preamble_end: usize, key: &str) -> usize {
    let base = drawer_bounds(lines, preamble_end).map_or(0, |(_, end)| end + 1);
    if key.eq_ignore_ascii_case("title") {
        return base;
    }
    // After the last existing keyword in the preamble, else just after the
    // drawer (or the top of the file).
    (base..preamble_end)
        .rev()
        .find(|&i| keyword_key(&lines[i]).is_some())
        .map_or(base, |i| i + 1)
}

/// Set, replace, or remove a `:KEY: value` line in the top property drawer.
///
/// `Some(v)` sets it; `None` removes it. If the file has no top drawer and
/// `value` is `Some`, a drawer is created at the very top of the file.
pub fn set_drawer_property(text: &mut String, key: &str, value: Option<&str>) {
    edit_lines(text, |lines| {
        let pe = first_headline_idx(lines);
        match drawer_bounds(lines, pe) {
            Some((start, end)) => {
                let existing = (start + 1..end)
                    .find(|&i| drawer_key(&lines[i]).is_some_and(|k| k.eq_ignore_ascii_case(key)));
                match (existing, value) {
                    (Some(i), Some(v)) => lines[i] = format!(":{key}: {v}"),
                    (Some(i), None) => {
                        lines.remove(i);
                    }
                    (None, Some(v)) => lines.insert(end, format!(":{key}: {v}")),
                    (None, None) => {}
                }
            }
            None => {
                if let Some(v) = value {
                    lines.insert(0, ":END:".to_string());
                    lines.insert(0, format!(":{key}: {v}"));
                    lines.insert(0, ":PROPERTIES:".to_string());
                }
            }
        }
    });
}

/// Line index at which a file-level node's body begins: just past the top
/// drawer and the run of `#+keyword:` lines (and the blank lines between
/// them). Blank lines *after* the header but before the first real body
/// line are kept in the header region.
fn body_start_idx(lines: &[String]) -> usize {
    let pe = first_headline_idx(lines);
    let mut i = 0;
    loop {
        while i < pe && lines[i].trim().is_empty() {
            i += 1;
        }
        if i >= pe {
            break;
        }
        if lines[i].trim().eq_ignore_ascii_case(":PROPERTIES:") {
            let mut j = i + 1;
            while j < pe && !lines[j].trim().eq_ignore_ascii_case(":END:") {
                j += 1;
            }
            i = (j + 1).min(pe);
        } else if keyword_key(&lines[i]).is_some() {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Byte offset (into `text`) at which a file-level node's body begins.
/// Used to *prepend* into a file node without disturbing its header.
#[must_use]
pub fn body_start_offset(text: &str) -> usize {
    let stripped: Vec<String> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    let bs = body_start_idx(&stripped);
    // Re-walk the original lines (with their '\n') to sum byte lengths.
    text.split_inclusive('\n').take(bs).map(str::len).sum()
}

/// Replace a file-level node's body — everything after the header block —
/// with `new_body`, keeping the property drawer and keywords intact and
/// leaving exactly one blank line between the header and the new body.
pub fn replace_file_body(text: &mut String, new_body: &str) {
    edit_lines(text, |lines| {
        let bs = body_start_idx(lines);
        lines.truncate(bs);
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let body = new_body.trim_end_matches('\n');
        if body.is_empty() {
            lines.push(String::new());
        } else {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            for l in body.split('\n') {
                lines.push(l.to_string());
            }
            lines.push(String::new());
        }
    });
}

/// Given the byte offset of a headline's `*` marker, return the offset at
/// which that headline's body begins: just past the title line, any
/// planning line (`SCHEDULED:` / `DEADLINE:` / `CLOSED:`), and the headline's
/// property drawer. Used to *prepend* content into a headline node without
/// landing inside its metadata.
#[must_use]
pub fn headline_body_offset(text: &str, headline_start: usize) -> usize {
    let mut pos = line_after(text, headline_start);
    while pos < text.len() {
        let line = current_line(text, pos);
        let trimmed = line.trim_start();
        if is_planning_line(trimmed) {
            pos = line_after(text, pos);
        } else if trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            let mut p = line_after(text, pos);
            loop {
                if p >= text.len() {
                    pos = p;
                    break;
                }
                let closing = current_line(text, p).trim().eq_ignore_ascii_case(":END:");
                p = line_after(text, p);
                if closing {
                    pos = p;
                    break;
                }
            }
        } else {
            break;
        }
    }
    pos
}

/// Byte offset just past the newline ending the line containing `pos`
/// (or end-of-text if it is the final line).
fn line_after(text: &str, pos: usize) -> usize {
    text[pos..].find('\n').map_or(text.len(), |n| pos + n + 1)
}

/// The line containing `pos`, without its trailing newline.
fn current_line(text: &str, pos: usize) -> &str {
    let end = text[pos..].find('\n').map_or(text.len(), |n| pos + n);
    &text[pos..end]
}

/// True for an org planning line under a headline.
fn is_planning_line(trimmed: &str) -> bool {
    trimmed.starts_with("SCHEDULED:")
        || trimmed.starts_with("DEADLINE:")
        || trimmed.starts_with("CLOSED:")
}

/// Render a tag list as a `#+filetags:` value (`:a:b:`). `None` for an
/// empty list, so callers can pass the result straight to [`set_keyword`]
/// and have the keyword removed when the list is empty.
#[must_use]
pub fn render_filetags(tags: &[String]) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    let mut s = String::from(":");
    for t in tags {
        s.push_str(t);
        s.push(':');
    }
    Some(s)
}

/// Render an alias list as a `:ROAM_ALIASES:` value (`"A" "B"`). `None` for
/// an empty list.
#[must_use]
pub fn render_alias_list(aliases: &[String]) -> Option<String> {
    if aliases.is_empty() {
        return None;
    }
    Some(
        aliases
            .iter()
            .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Render a refs list as a `:ROAM_REFS:` value: bare for URLs / `@citekeys`,
/// quoted for any value containing whitespace. `None` for an empty list.
#[must_use]
pub fn render_ref_list(refs: &[String]) -> Option<String> {
    if refs.is_empty() {
        return None;
    }
    Some(
        refs.iter()
            .map(|r| {
                if r.chars().any(char::is_whitespace) {
                    format!("\"{}\"", r.replace('"', "\\\""))
                } else {
                    r.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str =
        ":PROPERTIES:\n:ID:       abc-123\n:END:\n#+title: Original\n#+filetags: :a:b:\n\nBody line one.\nBody line two.\n";

    #[test]
    fn set_keyword_replaces_existing_title() {
        let mut t = FILE.to_string();
        set_keyword(&mut t, "title", Some("New Title"));
        assert!(t.contains("#+title: New Title"));
        assert!(!t.contains("Original"));
        // The drawer and body are untouched.
        assert!(t.contains(":ID:       abc-123"));
        assert!(t.contains("Body line one."));
    }

    #[test]
    fn set_keyword_replaces_filetags() {
        let mut t = FILE.to_string();
        set_keyword(
            &mut t,
            "filetags",
            render_filetags(&svec(&["x", "y"])).as_deref(),
        );
        assert!(t.contains("#+filetags: :x:y:"));
        assert!(!t.contains(":a:b:"));
    }

    #[test]
    fn set_keyword_none_removes_filetags() {
        let mut t = FILE.to_string();
        set_keyword(&mut t, "filetags", None);
        assert!(!t.contains("#+filetags"));
        assert!(t.contains("#+title: Original"));
    }

    #[test]
    fn set_keyword_adds_new_keyword_after_existing() {
        let mut t = ":PROPERTIES:\n:ID: x\n:END:\n#+title: T\n".to_string();
        set_keyword(&mut t, "filetags", Some(":new:"));
        // Lands after the title, not before it.
        let title_at = t.find("#+title").unwrap();
        let tags_at = t.find("#+filetags").unwrap();
        assert!(title_at < tags_at, "got: {t}");
    }

    #[test]
    fn set_drawer_property_adds_and_replaces() {
        let mut t = FILE.to_string();
        set_drawer_property(
            &mut t,
            "ROAM_ALIASES",
            render_alias_list(&svec(&["Nick"])).as_deref(),
        );
        assert!(t.contains(":ROAM_ALIASES: \"Nick\""));
        // Replacing keeps a single line.
        set_drawer_property(
            &mut t,
            "ROAM_ALIASES",
            render_alias_list(&svec(&["A", "B"])).as_deref(),
        );
        assert_eq!(t.matches(":ROAM_ALIASES:").count(), 1);
        assert!(t.contains(":ROAM_ALIASES: \"A\" \"B\""));
    }

    #[test]
    fn set_drawer_property_none_removes() {
        let mut t = FILE.to_string();
        set_drawer_property(&mut t, "ROAM_REFS", Some("https://example.com"));
        assert!(t.contains(":ROAM_REFS: https://example.com"));
        set_drawer_property(&mut t, "ROAM_REFS", None);
        assert!(!t.contains("ROAM_REFS"));
        // The drawer is still well-formed.
        assert!(t.contains(":PROPERTIES:") && t.contains(":END:"));
    }

    #[test]
    fn set_drawer_property_creates_drawer_when_absent() {
        let mut t = "#+title: No drawer\n\nBody\n".to_string();
        set_drawer_property(&mut t, "ID", Some("fresh-id"));
        assert!(t.starts_with(":PROPERTIES:\n:ID: fresh-id\n:END:\n"));
    }

    #[test]
    fn replace_file_body_keeps_header_and_one_blank() {
        let mut t = FILE.to_string();
        replace_file_body(&mut t, "Brand new body.\nSecond line.");
        assert!(t.contains("#+filetags: :a:b:"));
        assert!(!t.contains("Body line one."));
        assert!(t.contains("Brand new body.\nSecond line.\n"));
        // Exactly one blank line between header and body.
        assert!(t.contains(":a:b:\n\nBrand new body."), "got: {t}");
    }

    #[test]
    fn replace_file_body_on_headerless_file() {
        let mut t = "#+title: T\n".to_string();
        replace_file_body(&mut t, "hello");
        assert_eq!(t, "#+title: T\n\nhello\n");
    }

    #[test]
    fn body_start_offset_points_past_header() {
        let off = body_start_offset(FILE);
        assert_eq!(&FILE[off..], "Body line one.\nBody line two.\n");
    }

    #[test]
    fn edits_do_not_touch_headline_keywords() {
        // A `#+keyword:` look-alike below a headline must not be treated as
        // file-level metadata.
        let mut t =
            ":PROPERTIES:\n:ID: x\n:END:\n#+title: T\n\n* Section\n#+caption: not file level\n"
                .to_string();
        set_keyword(&mut t, "filetags", Some(":tag:"));
        // The new keyword lands in the preamble, above the headline.
        let tags_at = t.find("#+filetags").unwrap();
        let head_at = t.find("* Section").unwrap();
        assert!(tags_at < head_at, "got: {t}");
        assert!(t.contains("#+caption: not file level"));
    }

    #[test]
    fn headline_body_offset_skips_planning_and_drawer() {
        let t =
            "* Heading\nSCHEDULED: <2026-06-13>\n:PROPERTIES:\n:ID: x\n:END:\nbody starts here\n";
        let off = headline_body_offset(t, 0);
        assert_eq!(&t[off..], "body starts here\n");
    }

    #[test]
    fn headline_body_offset_without_metadata() {
        let t = "* Heading\nbody\n";
        let off = headline_body_offset(t, 0);
        assert_eq!(&t[off..], "body\n");
    }

    fn svec(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }
}
