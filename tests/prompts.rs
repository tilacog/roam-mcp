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
async fn both_prompts_are_listed() {
    run_with_server(server(), |peer| async move {
        let prompts = peer.list_all_prompts().await.expect("list prompts");
        let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"summarize-node"),
            "summarize-node should be listed, got: {names:?}"
        );
        assert!(
            names.contains(&"link-suggestions"),
            "link-suggestions should be listed, got: {names:?}"
        );

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
