//! Lexical retrieval: rank vault nodes by relevance to a blob of text.
//!
//! Used by prompts (e.g. `link-suggestions`) that need to surface the
//! handful of notes a draft is actually about, rather than an arbitrary
//! slice of the vault. The signal is deliberately simple and explainable:
//! a node scores by how much of its title and aliases overlap the text, so
//! matches are easy to predict and never depend on backend row order.
//!
//! This is a lexical (surface-form) match, not a semantic one — a draft
//! that discusses a topic without ever naming a note's title or aliases
//! will not surface that note. Callers should treat the result as
//! candidates to judge, not as a final answer.

use std::collections::HashSet;
use std::sync::Arc;

use crate::index::{IndexResult, NodeMeta, NodeQuery, RoamIndex};

/// Words below this length carry too little signal to match on.
const MIN_TOKEN_LEN: usize = 3;

/// Very common English words that would otherwise match almost any draft.
/// Kept short and lowercase; only words of length >= `MIN_TOKEN_LEN` need
/// listing, since shorter ones are already dropped.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
    "one", "our", "out", "has", "him", "his", "how", "its", "who", "did", "yet", "this", "that",
    "with", "from", "they", "them", "then", "than", "have", "will", "would", "your", "what",
    "when", "which", "into", "about", "there", "their", "been", "were", "also", "such", "only",
    "some", "more", "most", "over", "very",
];

/// A candidate note plus the relevance score it earned.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node: NodeMeta,
    /// Higher is more relevant. Only meaningful relative to other
    /// candidates from the same call.
    pub score: usize,
}

/// Lowercase alphanumeric tokens of `s`, dropping short words and stopwords.
fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() >= MIN_TOKEN_LEN && !STOPWORDS.contains(&w.as_str()))
}

/// Relevance of one node to the already-lowercased `text` and its token set.
///
/// Scoring:
/// - +`title.len()` if the whole (lowercased) title occurs as a substring
///   of the text — the strongest, most specific signal.
/// - +`token.len()` for each title/alias token that also appears as a token
///   in the text — rewards specific, longer overlaps over incidental words.
fn score_node(node: &NodeMeta, text_lower: &str, text_tokens: &HashSet<String>) -> usize {
    let mut score = 0usize;
    let title_lower = node.title.to_lowercase();
    if title_lower.len() >= MIN_TOKEN_LEN && text_lower.contains(&title_lower) {
        score += title_lower.len();
    }
    let alias_tokens = node.aliases.iter().flat_map(|a| tokens(a));
    for token in tokens(&node.title).chain(alias_tokens) {
        if text_tokens.contains(&token) {
            score += token.len();
        }
    }
    score
}

/// Rank vault nodes by how strongly their title/aliases match `text`.
///
/// Nodes that score zero are dropped, so an empty result means nothing in
/// the vault lexically matches — an honest "no candidates" rather than a
/// filler list. Ties break by title (ascending) so the order is
/// deterministic regardless of which backend produced the nodes. Returns
/// at most `limit` candidates, best first.
///
/// # Errors
///
/// Returns an error if the backend enumeration fails.
pub fn relevant_candidates(
    index: &Arc<dyn RoamIndex>,
    text: &str,
    limit: usize,
) -> IndexResult<Vec<Candidate>> {
    let text_lower = text.to_lowercase();
    let text_tokens: HashSet<String> = tokens(text).collect();

    // No usable terms in the input — nothing can match.
    if text_tokens.is_empty() {
        return Ok(Vec::new());
    }

    // Enumerate the whole vault once; scoring is cheap per node.
    let nodes = index.find_nodes(&NodeQuery {
        query: None,
        tags: &[],
        limit: None,
    })?;

    let mut scored: Vec<Candidate> = nodes
        .into_iter()
        .filter_map(|node| {
            let score = score_node(&node, &text_lower, &text_tokens);
            (score > 0).then_some(Candidate { node, score })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.node.title.cmp(&b.node.title))
    });
    scored.truncate(limit);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexResult;

    /// Minimal in-memory index for exercising the pure ranking logic
    /// without touching `SQLite` or the filesystem.
    struct FakeIndex(Vec<NodeMeta>);

    fn node(id: &str, title: &str, aliases: &[&str], tags: &[&str]) -> NodeMeta {
        NodeMeta {
            id: id.to_string(),
            file: std::path::PathBuf::from(format!("{id}.org")),
            title: title.to_string(),
            level: None,
            todo: None,
            priority: None,
            olp: Vec::new(),
            pos: None,
            aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    impl RoamIndex for FakeIndex {
        fn find_nodes(&self, _q: &NodeQuery<'_>) -> IndexResult<Vec<NodeMeta>> {
            Ok(self.0.clone())
        }
        fn node(&self, _id: &str) -> IndexResult<Option<NodeMeta>> {
            Ok(None)
        }
        fn backlinks(&self, _id: &str) -> IndexResult<Vec<crate::index::LinkRecord>> {
            Ok(Vec::new())
        }
        fn forward_links(&self, _id: &str) -> IndexResult<Vec<crate::index::LinkRecord>> {
            Ok(Vec::new())
        }
        fn by_ref(&self, _r: &str) -> IndexResult<Vec<NodeMeta>> {
            Ok(Vec::new())
        }
        fn tags(&self) -> IndexResult<Vec<(String, usize)>> {
            Ok(Vec::new())
        }
        fn node_count(&self) -> IndexResult<usize> {
            Ok(self.0.len())
        }
        fn orphans(&self) -> IndexResult<Vec<NodeMeta>> {
            Ok(Vec::new())
        }
        fn random_node(&self) -> IndexResult<NodeMeta> {
            use crate::index::IndexError;
            use rand::prelude::IndexedRandom;
            if self.0.is_empty() {
                return Err(IndexError::NotFound("index is empty".into()));
            }
            let mut rng = rand::rng();
            self.0
                .choose(&mut rng)
                .cloned()
                .ok_or_else(|| IndexError::NotFound("index is empty".into()))
        }
        fn node_by_path(&self, _path: &std::path::Path) -> IndexResult<Option<NodeMeta>> {
            Ok(None)
        }
        fn nodes_with_external_links(
            &self,
        ) -> IndexResult<Vec<(NodeMeta, Vec<crate::index::LinkRecord>)>> {
            Ok(Vec::new())
        }
        fn source(&self) -> &'static str {
            "fake"
        }
    }

    fn vault() -> Arc<dyn RoamIndex> {
        Arc::new(FakeIndex(vec![
            node(
                "1",
                "Pastafarian Canticle",
                &["Ps FSM", "The Noodly Psalm"],
                &[],
            ),
            node("2", "Noodly Appendage imagery", &[], &[]),
            node("3", "Daily journal", &[], &[]),
            node("4", "Legacy conventions", &[], &[]),
        ]))
    }

    #[test]
    fn ranks_relevant_nodes_and_drops_irrelevant_ones() {
        let idx = vault();
        let draft =
            "Today I reflected on the Noodly Appendage and its imagery in Pastafarian worship.";
        let got = relevant_candidates(&idx, draft, 50).unwrap();
        let ids: Vec<&str> = got.iter().map(|c| c.node.id.as_str()).collect();

        assert!(ids.contains(&"2"), "Noodly Appendage imagery should match");
        assert!(ids.contains(&"1"), "Pastafarian Canticle should match");
        assert!(
            !ids.contains(&"3"),
            "Daily journal is unrelated and must be dropped, got: {ids:?}"
        );
        assert!(
            !ids.contains(&"4"),
            "Legacy conventions is unrelated and must be dropped, got: {ids:?}"
        );
        // The note with the most distinctive overlap ranks first.
        assert_eq!(ids.first(), Some(&"2"));
    }

    #[test]
    fn matches_through_an_alias() {
        let idx = vault();
        // "Noodly Psalm" is only an alias of node 1, not its title.
        let got = relevant_candidates(&idx, "a meditation on the Noodly Psalm", 50).unwrap();
        assert!(got.iter().any(|c| c.node.id == "1"), "alias should match");
    }

    #[test]
    fn empty_or_stopword_only_draft_yields_no_candidates() {
        let idx = vault();
        assert!(relevant_candidates(&idx, "", 50).unwrap().is_empty());
        assert!(
            relevant_candidates(&idx, "the and for with that they", 50)
                .unwrap()
                .is_empty(),
            "a draft of only stopwords must not match anything"
        );
    }

    #[test]
    fn limit_caps_the_result_count() {
        let idx = vault();
        let draft = "Noodly Appendage Pastafarian Canticle journal conventions";
        let got = relevant_candidates(&idx, draft, 1).unwrap();
        assert_eq!(got.len(), 1, "limit must cap the candidate count");
    }
}
