---
name: org-roam-kb
description: >
  Search and update a personal org-roam knowledge base. Activate when
  the user asks about their notes, wants to capture or link ideas, or
  when prior knowledge would inform the answer.
---

# org-roam Knowledge Base

## Quick reference

### Search (always start here)

```
search_nodes  → title + alias match
search_text   → full-text body match (run in parallel with search_nodes)
tag_cooccurrences → related tags for a given tag
```

Merge results, dedupe by id, rank by relevance.

### Read

```
get_node(id)              # metadata + body
get_node_section(id, anchor)  # one section (use list_anchors first to find anchors)
list_anchors(id)           # headlines, <<targets>>, CUSTOM_IDs
```

### Graph navigation

```
get_backlinks(id)    # who links to this node
get_forward_links(id) # what this node links to
```

Follow 1–2 hops max before synthesizing.

### Write

```
update_node(id, ..., preview: true)  # dry run first — always
append_to_node(id, content: "...", headline: "section")
create_node(...)                    # call serially, never in parallel
daily_capture(content: "...", headline: "section")
```

After any write: `sync_database(force: false)` → if `drift.missing_in_sqlite` is non-empty, follow with `sync_database(force: true, wait: true)`.

## Anchor fragments

`[[id:...::§4.1.2]]` links point to a `<<§4.1.2>>` target inside the target node. Strip the `::anchor` part and call `get_node_section(id, anchor: "§4.1.2")` instead of (or after) `get_node`.

## Serial writes

`create_node` must be called **serially**. Some MCP clients mis-serialize parallel calls, causing non-deterministic failures.

## Deep reference

The full usage guide, tool schema, write mechanics, and quirks are in `docs/HOW_TO_USE.md`.
