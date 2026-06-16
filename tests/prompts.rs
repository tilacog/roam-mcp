//! End-to-end prompt tests: spin up the server in-process and drive its
//! prompt endpoints (`list_prompts` / `get_prompt`) with the rmcp client.
//!
//! These exercise the full MCP path, not just the helper functions, so a
//! regression in prompt wiring — or in the retrieval that `link-suggestions`
//! depends on — fails here.

mod common;

use std::path::PathBuf;

use rmcp::model::{GetPromptRequestParams, GetPromptResult, PromptMessageContent};
use rmcp::object;
use tempfile::TempDir;

use common::run_with_server;
use org_roam_mcp::{Config, RoamServer};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-vault")
}

fn server() -> RoamServer {
    let cfg = Config::from_args(&fixture_dir(), true, true, None).unwrap();
    RoamServer::new(cfg).unwrap()
}

/// Concatenate the text content of a prompt result's messages.
fn prompt_text(result: &GetPromptResult) -> String {
    result
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            PromptMessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn all_prompts_are_listed() {
    run_with_server(server(), |peer| async move {
        let prompts = peer.list_all_prompts().await.expect("list prompts");
        let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
        for expected in [
            "summarize-node",
            "link-suggestions",
            "orphan-triage",
            "tag-suggestions",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} should be listed, got: {names:?}"
            );
        }

        // The required argument of each prompt is advertised.
        let summarize = prompts.iter().find(|p| p.name == "summarize-node").unwrap();
        let args: Vec<&str> = summarize
            .arguments
            .iter()
            .flatten()
            .map(|a| a.name.as_str())
            .collect();
        assert!(
            args.contains(&"id"),
            "summarize-node must declare `id`, got: {args:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn summarize_node_prompt_includes_title_and_body() {
    run_with_server(server(), |peer| async move {
        let result = peer
            .get_prompt(
                GetPromptRequestParams::new("summarize-node")
                    .with_arguments(object!({ "id": "11111111-1111-1111-1111-111111111111" })),
            )
            .await
            .expect("get_prompt summarize-node");
        let text = prompt_text(&result);
        assert!(
            text.contains("Pastafarian Canticle"),
            "prompt should name the note, got: {text}"
        );
        assert!(
            text.contains("shadow of the meatball"),
            "prompt should embed the note body, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn link_suggestions_retrieves_relevant_and_drops_irrelevant() {
    run_with_server(server(), |peer| async move {
        let draft =
            "Today I reflected on the Noodly Appendage and its imagery in Pastafarian worship.";
        let result = peer
            .get_prompt(
                GetPromptRequestParams::new("link-suggestions")
                    .with_arguments(object!({ "draft": draft })),
            )
            .await
            .expect("get_prompt link-suggestions");
        let text = prompt_text(&result);

        assert!(
            text.contains("## Candidate notes"),
            "expected the non-empty candidate branch, got: {text}"
        );
        // The relevant notes are surfaced...
        assert!(
            text.contains("22222222-2222-2222-2222-222222222222"),
            "Noodly Appendage imagery should be a candidate, got: {text}"
        );
        assert!(
            text.contains("11111111-1111-1111-1111-111111111111"),
            "Pastafarian Canticle should be a candidate, got: {text}"
        );
        // ...and the unrelated ones are not. This is what the old
        // alphabetical-slice behavior got wrong: it listed every node.
        assert!(
            !text.contains("Daily journal"),
            "unrelated 'Daily journal' must not be a candidate, got: {text}"
        );
        assert!(
            !text.contains("Legacy conventions"),
            "unrelated 'Legacy conventions' must not be a candidate, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn link_suggestions_reports_when_nothing_matches() {
    run_with_server(server(), |peer| async move {
        let result = peer
            .get_prompt(
                GetPromptRequestParams::new("link-suggestions").with_arguments(
                    object!({ "draft": "quantum chromodynamics lattice gauge renormalization" }),
                ),
            )
            .await
            .expect("get_prompt link-suggestions");
        let text = prompt_text(&result);
        assert!(
            text.contains("No notes in the org-roam vault lexically match"),
            "expected an honest no-match message, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn orphan_triage_lists_orphans_and_excludes_linked_notes() {
    run_with_server(server(), |peer| async move {
        let result = peer
            .get_prompt(GetPromptRequestParams::new("orphan-triage").with_arguments(object!({})))
            .await
            .expect("get_prompt orphan-triage");
        let text = prompt_text(&result);

        assert!(
            text.contains("## Orphan notes"),
            "expected the orphan list, got: {text}"
        );
        // 'Daily journal' has no id: links, so it is an orphan.
        assert!(
            text.contains("Daily journal"),
            "the orphan 'Daily journal' should be listed, got: {text}"
        );
        // The 1111<->2222 pair link each other, so neither is an orphan.
        assert!(
            !text.contains("11111111-1111-1111-1111-111111111111"),
            "linked 'Pastafarian Canticle' must not be triaged as an orphan, got: {text}"
        );
        assert!(
            !text.contains("22222222-2222-2222-2222-222222222222"),
            "linked 'Noodly Appendage imagery' must not be triaged as an orphan, got: {text}"
        );
    })
    .await;
}

/// A vault whose only two notes link each other, so it has no orphans.
fn fully_linked_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("a.org"),
        ":PROPERTIES:\n:ID: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n:END:\n\
         #+title: Note A\n\nSee [[id:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb]].\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.org"),
        ":PROPERTIES:\n:ID: bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\n:END:\n\
         #+title: Note B\n\nSee [[id:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa]].\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn orphan_triage_reports_when_there_are_no_orphans() {
    let dir = fully_linked_dir();
    let cfg = Config::from_args(dir.path(), true, true, None).unwrap();
    let server = RoamServer::new(cfg).unwrap();
    run_with_server(server, move |peer| async move {
        let result = peer
            .get_prompt(GetPromptRequestParams::new("orphan-triage").with_arguments(object!({})))
            .await
            .expect("get_prompt orphan-triage");
        let text = prompt_text(&result);
        assert!(
            text.contains("no orphans to triage"),
            "expected the no-orphans message, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn tag_suggestions_shows_vocabulary_current_tags_and_body() {
    run_with_server(server(), |peer| async move {
        let result = peer
            .get_prompt(
                GetPromptRequestParams::new("tag-suggestions")
                    .with_arguments(object!({ "id": "11111111-1111-1111-1111-111111111111" })),
            )
            .await
            .expect("get_prompt tag-suggestions");
        let text = prompt_text(&result);

        assert!(
            text.contains("## Existing tag vocabulary"),
            "expected the vocabulary section, got: {text}"
        );
        assert!(
            text.contains("## Current tags"),
            "expected the current-tags section, got: {text}"
        );
        // This note already carries the `pastafarianism` tag, which is also
        // part of the vault vocabulary.
        assert!(
            text.contains("pastafarianism"),
            "expected the node's existing tag, got: {text}"
        );
        // The note body is embedded so the model can judge relevance.
        assert!(
            text.contains("shadow of the meatball"),
            "expected the note body, got: {text}"
        );
    })
    .await;
}

#[tokio::test]
async fn completion_suggests_node_ids_for_id_argument() {
    run_with_server(server(), |peer| async move {
        // Typing part of a title resolves to the matching node's id, so a
        // user need not know the UUID up front.
        let by_title = peer
            .complete_prompt_simple("summarize-node", "id", "canticle")
            .await
            .expect("complete by title");
        assert!(
            by_title.contains(&"11111111-1111-1111-1111-111111111111".to_string()),
            "title 'canticle' should resolve to its node id, got: {by_title:?}"
        );
        assert!(
            !by_title.contains(&"22222222-2222-2222-2222-222222222222".to_string()),
            "unrelated node must not be suggested, got: {by_title:?}"
        );

        // An id prefix also matches.
        let by_prefix = peer
            .complete_prompt_simple("tag-suggestions", "id", "22222222")
            .await
            .expect("complete by id prefix");
        assert!(
            by_prefix.contains(&"22222222-2222-2222-2222-222222222222".to_string()),
            "id prefix should match, got: {by_prefix:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn completion_is_empty_for_freeform_arguments() {
    run_with_server(server(), |peer| async move {
        // `draft` is freeform prose — there is nothing meaningful to
        // complete, so the server returns no suggestions.
        let values = peer
            .complete_prompt_simple("link-suggestions", "draft", "anything")
            .await
            .expect("complete draft");
        assert!(
            values.is_empty(),
            "freeform argument should yield no completions, got: {values:?}"
        );
    })
    .await;
}
