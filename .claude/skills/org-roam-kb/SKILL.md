---
name: org-roam-kb
description: >
  Use the org-roam MCP server as a personal knowledge graph to gather context,
  answer questions, and navigate linked notes. Trigger this skill whenever
  the user asks about something that might be in their notes or knowledge base,
  when you need background context before answering a research question, when
  the user says "check my notes", "what do I know about X", "look in my vault",
  or when doing research on any topic where prior personal notes could inform
  the answer. Also trigger when the user asks you to add, update, or link
  notes, capture a finding, or maintain their vault. Prefer using the knowledge
  base proactively — even if the user doesn't mention it, if the question is
  substantive and the user has a roam vault connected, check it first.
---

# org-roam Knowledge Base

The `mcp__org-roam__*` tools expose a personal knowledge graph (org-roam vault).
Use them to find what the user already knows, follow connections between ideas,
and synthesize a grounded answer before relying on training data alone.

## When to reach for this

- User asks a question about a topic, person, project, or concept → search first
- User says "check my notes / vault / knowledge base" → explicit instruction
- You need background before answering something substantive → scout the graph
- User wants to capture, update, or connect notes → write tools apply

## Search protocol

Always start broad, then narrow.

**Step 1 — title search**
```
mcp__org-roam__search_nodes(query: "<topic>", limit: 15)
```
Matches node titles and aliases. This is the fastest entry point.

**Step 2 — full-text search** (run in parallel with step 1)
```
mcp__org-roam__search_text(query: "<topic>", limit: 20)
```
Finds nodes that *mention* the topic in their body but don't have it in the title. Essential for cross-cutting concepts.

**Step 3 — tag expansion** (if results are thin or you want adjacencies)
```
mcp__org-roam__tag_cooccurrences(tag: "<tag>")
```
Shows which other tags appear alongside the target tag. Use this to discover related areas you might have missed (e.g., searching "emacs" surfaces "org-roam", "lisp", "pkm").

Merge the three result sets, deduplicate by id, and rank by relevance before fetching content.

## Graph navigation

From a promising node, follow its edges to build a richer picture.

```
mcp__org-roam__get_backlinks(id: "<id>")   # who links to this node
mcp__org-roam__get_forward_links(id: "<id>") # what this node links to
```

Run both in parallel. Each backlink or forward link is a candidate for another `get_node` fetch. Follow 1–2 hops max before synthesizing — going deeper usually yields diminishing returns.

Use `tag_cooccurrences` on any relevant tag you find to discover thematically related clusters you might not have thought to search for directly.

## Reading node content

For each high-value node:

```
mcp__org-roam__get_node(id: "<id>")
```

Returns metadata + full body. For long notes, first check their structure:

```
mcp__org-roam__list_anchors(id: "<id>")      # list headlines / sections
mcp__org-roam__get_node_section(id: "<id>", anchor: "<headline>")  # read one section
```

Read section-by-section rather than discarding long notes as "too big."

## Efficient patterns

**"Tell me about X"** — the standard loop:
1. `search_nodes` + `search_text` in parallel
2. Pick top 3–5 nodes by relevance
3. `get_node` each (parallel)
4. `get_backlinks` + `get_forward_links` on the most central node
5. Synthesize from what you found; cite node titles

**"What's connected to X?"** — graph walk:
1. Find id via `search_nodes`
2. `get_backlinks` + `get_forward_links` in parallel
3. `get_node` each neighbor
4. Report the neighborhood, not a flat list

**"Is there anything in my notes about Y?"** — completeness check:
1. `search_nodes` + `search_text` with Y
2. If thin: `list_tags` to see if a relevant tag exists, then `search_nodes(tags: ["<tag>"])`
3. Report honestly: "Found N nodes" or "Nothing matched — this may be a gap."

**Reference lookup** — if the user mentions a URL or citation key:
```
mcp__org-roam__find_by_ref(ref: "<url-or-citekey>")
```
Returns nodes whose `ROAM_REFS` match. Use before `search_nodes` when you have a concrete ref.

## Writing back

When capturing findings or updating notes, always verify the target first:

```
mcp__org-roam__search_nodes(query: "<target title>")  # confirm the node exists
mcp__org-roam__update_node(id: ..., preview: true)    # dry-run before writing
mcp__org-roam__update_node(id: ..., preview: false)   # real write
```

For appending without full rewrite:
```
mcp__org-roam__append_to_node(id: "<id>", content: "...", headline: "<section>")
```

For today's daily note:
```
mcp__org-roam__daily_capture(content: "...", headline: "<section title>")
```

Issue `create_node` calls **serially** (not in parallel) when creating multiple nodes — some MCP clients mis-serialize parallel create calls. After a batch of writes, confirm with:
```
mcp__org-roam__sync_database(force: false)
```
If `drift.missing_in_sqlite` is non-empty, follow with `sync_database(force: true, wait: true)`.

## What NOT to do

- Don't fetch the entire node list before searching — use `search_nodes` or `search_text` with a query
- Don't follow link chains more than 2 hops without a clear reason — synthesize from what you have
- Don't silently skip a `validate_node` step after a write if something seems off (stale id, empty title)
- Don't call `delete_node` without explicit user confirmation; it's destructive and irreversible
