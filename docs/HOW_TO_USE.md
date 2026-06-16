# How to use `org-roam-mcp`

This document is a practical guide for **two audiences** who want to get
work done with the server:

- **Human users** driving an MCP-aware assistant (Claude Desktop,
  Claude Code, pi, etc.) and asking it to read, search, and extend
  their org-roam vault through natural language.
- **AI agents** that consume the server's tool surface directly to
  answer questions, traverse the link graph, and make structured edits
  to notes.

The first half is short, narrative, and framed as user stories. The
second half is a tool reference and a few recipe-style patterns you
can lift into your own workflows.

> If you have not installed or configured the server yet, see the
> [README](./README.md) for build, transport, and MCP client setup.
> All examples below assume the server is already running and exposed
> under an MCP client.

---

## TL;DR for agents

The server is a **read-write org-roam knowledge base** exposed over
MCP. The 27 tools split into three groups:

- **Read** (always available): `search_nodes`, `list_nodes`,
  `list_orphans`, `search_text`, `get_node`, `get_node_by_path`,
  `get_node_section`, `get_backlinks`, `get_forward_links`,
  `find_by_ref`, `get_refs`, `list_tags`, `tag_cooccurrences`,
  `list_anchors`, `unlinked_references`, `validate_node`,
  `get_daily_note`, `list_dailies`, `server_info`, `sync_database`.
- **Write** (removed in `--read-only` mode): `create_node`,
  `update_node`, `delete_node`, `rename_node`, `append_to_node`,
  `prepend_to_node`, `add_link`, `insert_anchor`, `daily_capture`.
- **Resources & prompts**: `org-roam://node/{id}` (+ `#anchor`),
  `summarize-node`, `link-suggestions`, `orphan-triage`,
  `tag-suggestions`.

Two index backends (auto-selected): the fast `org-roam.db` reader when
Emacs has a db, otherwise a filesystem scanner. Both backends return
the same shapes, so the same agent code works in both modes.

Writes never touch `org-roam.db` directly. Each write goes through a
file-rename, refuses to clobber an Emacs lockfile, and triggers a
debounced `org-roam-db-sync` so Emacs stays consistent.

---

## User stories

The stories below show realistic end-to-end loops. The bracketed bits
are the tool calls an agent (or a user with an agent) would make.

### 1. "Capture what just happened into today's daily"

You just finished a standup. You want it written down under a
*Standup* section of today's daily note without opening Emacs.

```
> Add a standup to today: shipped the org-roam-mcp sync tool, blocked
> on emacsclient timeout, will follow up on drift warnings.
```

What the agent does:

1. `daily_capture` with `content: "..."` and `headline: "Standup"` —
   creates today's daily if it doesn't exist, then appends the entry
   under a `* Standup` headline.
2. (Optional) `get_daily_note` later to confirm the entry landed and
   pick up the node id for follow-up notes.

```jsonc
// daily_capture
{
  "content": "Shipped org-roam-mcp sync tool. Blocked on emacsclient timeout. Will follow up on drift warnings.",
  "headline": "Standup"
}
```

### 2. "Find what I know about X"

You have a vague memory of writing about *org-roam* and want to pull
together everything — both note titles and body text.

```
> What do I know about org-roam in my vault?
```

What the agent does:

1. `search_nodes({ query: "org-roam", limit: 20 })` — matches titles
   and aliases.
2. `search_text({ query: "org-roam", limit: 50 })` — full-text, picks
   up notes that mention the term in the body but not the title.
3. `tag_cooccurrences({ tag: "org-roam" })` — surface related tags so
   the answer can suggest adjacent topics (`emacs`, `knowledge-graph`,
   `pkm`).

The agent stitches the three result sets into one short report.

### 3. "What links to this note, and what does it link to?"

You want the link neighborhood of a node — its parents and its
children — so you can decide whether to merge, refactor, or extend it.

```
> Show me the link graph around my "org-roam-mcp" note.
```

What the agent does:

1. `search_nodes({ query: "org-roam-mcp" })` → pick the right id.
2. `get_backlinks({ id })` → who points at it.
3. `get_forward_links({ id })` → who it points at.
4. For each interesting neighbor: `get_node({ id })` for the body.

The agent can now produce a short, structured answer like
"`org-roam-mcp` is referenced from `mcp-servers` and links to
`emacs`, `org-roam`, and `sqlite`."

### 4. "Turn plain-text mentions into proper org-roam links"

You have a note that mentions *Psalm 23* by name in several places but
hasn't linked it. You want to find the mentions and link them.

```
> Find every place in my vault that says "Psalm 23" in plain text
> but isn't already linked, and link the first three to the node.
```

What the agent does:

1. `search_nodes({ query: "Psalm 23" })` → get the id.
2. `unlinked_references({ id })` → file paths + byte offsets +
   snippets of plain-text occurrences (already-linked mentions are
   skipped).
3. For each snippet, the agent opens the file (via `update_node`
   with `preview: true` first, then real), rewrites the plain text
   to `[[id:<UUID>][Psalm 23]]`, and saves.

`unlinked_references` is the right primitive here: it already filters
out matches inside `[[...]]` links, so the agent only has to do
mechanical rewrites.

### 5. "I have a draft paragraph — find existing notes I should link to"

You wrote a paragraph and want to know which of your existing notes
should be referenced.

```
> Given this draft, which of my notes should I link to?

Draft: "Org-roam's graph database gives Emacs a fast index, but
schema migrations have been a recurring source of pain for me."
```

What the agent uses:

- The built-in **`link-suggestions` prompt**. The agent calls the
  prompt with the draft text and gets back a structured prompt that
  lists candidate nodes; the model then ranks them and returns
  `[[id:UUID][Title]]` suggestions with one-sentence reasons.

Or, the agent does the same loop manually:

1. `search_nodes({ limit: 50 })` for the full candidate list (or a
   narrower query like `tags: ["emacs", "org-roam"]`).
2. Score candidates by keyword overlap with the draft.
3. Return the top N with the suggested link form.

### 6. "Tidy my vault: find orphans and dangling links"

A periodic maintenance task: surface notes that are unreachable and
notes that point at things that no longer exist.

```
> Find orphan notes and any node with broken links.
```

What the agent does:

1. `list_orphans({ limit: 100 })` — notes with no `id:` edges in or
   out. They exist in the vault but are unreachable. These are the
   prime candidates for merge/link/delete.
2. For a few specific notes the user names, `validate_node({ id })`
   — reports a stale `:ID:`, an empty title, or dangling `id:` links.
3. For each dangling id, decide: rename, recreate, or remove.

### 7. "Read a long note section-by-section"

Some notes are long; you want to walk through one headline at a time
without scrolling.

```
> Walk me through my "Rust async runtime" note, one section at a time.
```

What the agent does:

1. `list_anchors({ id })` → headline titles, `CUSTOM_ID`s, and
   `<<target>>` names.
2. For each anchor in order, `get_node_section({ id, anchor })` →
   just the bytes of that section, plus `begin`/`end` byte offsets
   into the file.
3. The agent narrates section by section, optionally calling
   `get_node_by_path` once to confirm the file and grab the title.

This is also the path used by the **`org-roam://node/{id}#anchor`**
resource, which a client like Claude Desktop can fetch directly when
the user clicks an `[[id:UUID::anchor]]` link.

### 8. "I just wrote a note — make sure it actually synced"

You used `create_node` from the agent. You want to verify the file
is in the index **and** that the SQLite db Emacs reads from agrees
with the filesystem.

```
> Confirm my last write synced to org-roam.db.
```

What the agent does:

1. `sync_database({ force: false })` — no-op write; returns a
   `SyncReport` with:
   - `active_backend`: `sqlite` (if db present) or `scanner`.
   - `last_sync`: timestamp of the most recent successful sync.
   - `drift.missing_in_sqlite`: node ids the scanner sees but the
     db doesn't yet (your write, if any).
   - `drift.missing_in_scanner`: node ids the db knows but the
     filesystem no longer has (stale rows).
2. If drift is non-empty, `sync_database({ force: true, wait: true,
   timeout_ms: 30000 })` to actually trigger `org-roam-db-sync` via
   `emacsclient`. Default mode is `client-only`, so this reuses the
   running Emacs session — no `--batch` fork.
3. Call `sync_database` again with `force: false` to confirm the
   drift list is now empty.

### 9. "Build a reading list for a topic I'm starting to learn"

```
> I want to learn about knowledge graphs. Find my notes on the
> topic, group them by sub-tag, and identify any obvious gaps.
```

What the agent does:

1. `search_nodes({ query: "knowledge graph" })` and
   `search_text({ query: "knowledge graph" })` for breadth.
2. For each candidate, `tag_cooccurrences({ tag })` to surface
   sub-tags (e.g. `[[id:foo][rdf]]` tends to co-tag with
   `[[id:bar][semantic-web]]`).
3. Cross-reference with `list_orphans` to find candidate notes that
   exist on the topic but aren't yet linked into the graph.
4. Suggest two or three `create_node` calls to fill the gaps, and
   one `add_link` call each to wire the new notes into the existing
   graph.

### 10. "Append research notes over a long session"

You are doing an open-ended research pass; you want every finding
appended to one node without you having to remember where it lives.

```
> Add a finding to my "org-roam-mcp — open questions" note: does
> the scanner backend handle very large vaults?
```

What the agent does:

1. `search_nodes({ query: "open questions" })` to find the id (or
   the user supplies the id).
2. `append_to_node({ id, content: "..." })`. For more structure,
   pass `headline: "Performance"` to keep the new bullet grouped.
3. After multiple appends, the agent can `search_text({ query:
   "very large vault" })` to confirm all the bullets are findable
   from the rest of the vault.

### 11. "Subscribe to a node so I get notified of edits"

You want your client to be notified when a particular note changes
(file watcher → `notifications/resources/updated`).

```
> Watch my "active project" note and tell me if it changes.
```

What the agent does:

- The MCP `resources/subscribe` channel — the server exposes one
  resource template: `org-roam://node/{id}`. The agent subscribes
  with that URI. The server's file watcher emits a resource-updated
  notification for the URI on any `.org` change to the
  corresponding file.

This is the right primitive for a long-lived session like Claude
Code: subscribe, work on other things, react when a notification
arrives.

---

## Agent patterns

These are the recurring shapes agents should reach for. None of them
are new tools — they are compositions of the tool list above.

### A. "Discover → open → summarize"

```
search_nodes({ query })          # find candidate ids
  → pick best id
  → get_node({ id })             # metadata + body
  → get_backlinks({ id })        # for "why is this note interesting"
  → summarize-node prompt        # turn the body into a short answer
```

This is the default flow for "tell me about X in my vault."

### B. "Verify before write"

Every structural write should be preceded by a small read to confirm
the target is what the agent thinks it is.

```
search_nodes or get_node         # confirm id
  → preview the change
update_node({ id, ..., preview: true })   # dry run; returns the would-be text
  → if it looks right, call again with preview: false
```

`update_node`'s `preview` flag is designed for this — it returns the
candidate file text and a `changed: true|false` flag, without writing
to disk.

### C. "Idempotent edit"

For any edit that might run more than once (cron-style, or in a
retry loop), prefer `update_node` over a `create_node` → `append`
chain:

- It is keyed on `:ID:`, so the same call replays cleanly.
- It refuses to change `:ID:`, so the link graph stays stable.
- `tags: []` removes the `#+filetags:` line; an omitted `tags`
  leaves it alone. This is the right semantics for "set the tags
  to exactly this list."

### D. "Sync confirm"

After any batch of writes, end the loop with:

```
sync_database({ force: false })           # report
  → if drift.missing_in_sqlite is non-empty:
sync_database({ force: true, wait: true, timeout_ms: 30000 })
  → re-check with force: false
```

This makes the eventual-consistency window visible instead of
invisible. The default `client-only` mode talks to your live Emacs
session, so the round trip is fast.

### E. "Catch broken writes before they ship"

Before declaring a session done, a careful agent walks the list of
ids it touched and calls `validate_node` on each. The tool flags:
- a `:ID:` the index knows but the file no longer carries,
- an empty title,
- dangling `id:` forward links.

### F. "Capture without context"

The single most common agent task is: "save this fact, find it
later." The minimum loop is:

```
daily_capture({ content: "the fact", headline: "Captures" })
  → if it is a one-off, optionally later:
search_text({ query: "the fact" })
```

`search_text` matches bodies, not titles, so anything you append
to a daily note is reachable later even if you never promoted it
to its own node.

---

## Tool reference (quick)

The full schema is exposed by the MCP client — clients like
`mcp inspector` or the Claude Desktop tools panel will show types
and required fields inline. This table is the *shape* of the call,
not the full schema.

### Reads

| Tool | Notable params | Returns |
| --- | --- | --- |
| `search_nodes` | `query`, `tags`, `limit` | List of node metadata (id, title, aliases, tags, file) |
| `list_nodes` | `filter`, `tags`, `limit`, `offset`, `sort` | Paginated node list + total |
| `list_orphans` | `limit`, `offset`, `sort` | Notes with no `id:` edges, paged |
| `search_text` | `query`, `limit` | File path + line + snippet + node_id (when in a file node) |
| `get_node` | `id` | Metadata + body |
| `get_node_by_path` | `path` | Same as `get_node`; resolves a `.org` path to its `:ID:` |
| `get_node_section` | `id`, `anchor` | Sub-section text + `begin`/`end` byte offsets |
| `get_backlinks` | `id` | Nodes whose `id:` links resolve to `id` |
| `get_forward_links` | `id` | Outgoing links of every kind, with dest metadata |
| `find_by_ref` | `ref` | Nodes whose `ROAM_REFS` matches a URL or `@citekey` |
| `get_refs` | `id` | The `ROAM_REFS` (and v1 `ROAM_KEY`) declared by a node |
| `list_tags` | — | Tag → count of nodes |
| `tag_cooccurrences` | `tag`, `limit` | For nodes bearing `tag`, which other tags appear with it |
| `list_anchors` | `id` | `<<target>>`s, headlines, `CUSTOM_ID`s in a node |
| `unlinked_references` | `id`, `limit` | Plain-text mentions of a node's title/aliases |
| `validate_node` | `id` | Structural issues (stale id, empty title, dangling links) |
| `get_daily_note` | `date` (YYYY-MM-DD; default today) | Daily note body or `exists: false` |
| `list_dailies` | `limit` | Dailies newest-first, with id and title |
| `server_info` | — | Backend, node count, version, sync config |
| `sync_database` | `force`, `wait`, `timeout_ms`, `backend` | Sync report (drift, last sync, warnings) |

### Writes (removed in `--read-only`)

| Tool | Notable params | Effect |
| --- | --- | --- |
| `create_node` | `title`, `tags`, `body`, `refs`, `aliases` | New file with a fresh `:ID:` |
| `update_node` | `id`, `title?`, `body?`, `tags?`, `aliases?`, `refs?`, `properties?`, `preview?` | Idempotent in-place edit; `preview:true` is a dry run |
| `delete_node` | `id` | Whole file for file nodes; subtree for headline nodes |
| `rename_node` | `id`, `title`, `rename_file?` | Change `#+title:` and (by default) the filename |
| `append_to_node` | `id`, `content`, `headline?` | Append to body, or under a named headline |
| `prepend_to_node` | `id`, `content`, `headline?` | Insert at start of body, or at start of a named headline's body |
| `add_link` | `id` (source), `target`, `description?`, `headline?` | Write `[[id:target][desc]]` into the source |
| `insert_anchor` | `id`, `search_text`, `anchor_name` | Place `<<name>>` before a matched paragraph; returns the `[[id:UUID::name]]` link |
| `daily_capture` | `content?`, `headline?` | Create or open today's daily; optionally append |

#### Important: `body` is the body, not the file

`update_node`'s `body` parameter is the file's body proper — everything
after the property drawer and the `#+title:` / `#+filetags:` header
keywords. It is **not** the whole file. A common mistake is to read
the file, edit the text in your head, and pass the edited file back
as the `body`. That silently produces nested `:PROPERTIES:` drawers
and a `#title:` concatenated with itself (`"2026-06-16 2026-06-16
2026-06-16"`) because the tool faithfully inserts the body *after*
the existing header. The tool now rejects bodies that start with
`:PROPERTIES:` or `#+title:` with a clear error. Manage the title
and properties through their own parameters instead.

#### Important: `headline` is the title, not the `*` line

`add_link`, `append_to_node`, and `daily_capture` all take an optional
`headline` parameter that names the *title* of the target headline.
The matcher strips a leading `*`-marker run, so both `Spec section`
and `** Spec section` resolve to the same headline. If no headline
matches, the call is now rejected with a `headline not found` error
rather than silently appending at end of file (the previous behavior
that produced the "I added a link under `** Specification (2025-11-25)`
but it landed somewhere else" complaint).

#### Where daily notes live

`daily_capture` writes to the directory named by `Config::dailies_dir`,
which the server reads from the `--dailies-dir` CLI flag. When the
flag is unset, daily notes land at the roam-dir root, which is
usually *not* where your existing `notes/daily/20260112-dracula-readalong.org`
files live. If your vault has a daily-notes directory, start the
server with

```
--dailies-dir daily --dailies-format %Y-%m-%d
```

`server_info` now reports the configured `dailies.dir` and emits a
`dailies.hint` field naming the flag to set when `dir` is `null`.

### Resources

`org-roam://node/{id}` — the full body of a node. Append `#anchor`
(any of: `CUSTOM_ID`, headline title, `<<target>>` name, or free
text) to get just the matching sub-section.

### Prompts

- `summarize-node({ id })` — builds a prompt asking the model to
  summarize a node's body.
- `link-suggestions({ draft, limit? })` — builds a prompt that lists
  the nodes whose titles/aliases overlap the draft (ranked by lexical
  relevance, not an arbitrary slice) and asks the model to suggest
  which to link from the draft.
- `orphan-triage({ limit? })` — lists orphan notes (no incoming or
  outgoing `id:` links) and asks the model to recommend merge / link /
  delete / keep for each.
- `tag-suggestions({ id, limit? })` — shows a node's body, its current
  tags, and the vault's existing tag vocabulary, and asks the model to
  propose tags that stay consistent with that vocabulary.

---

## How a write actually lands

This is the mechanics an agent should know about, in case something
goes wrong:

1. The server opens the file, computes the new text in memory, and
   re-stats the file's mtime. If the mtime changed between the read
   and the write (e.g. an Emacs save raced the call), the write is
   **refused** with a clear error and the agent should retry on a
   fresh read.
2. The new text is written via `util::atomic_write`: a sibling temp
   file, `fsync`, then rename. If an Emacs lockfile
   (`.#filename`) is present, the write is **refused** — the agent
   should not try to recover silently.
3. In scanner mode, the in-memory index is rebuilt so the next read
   sees the change immediately. In sqlite mode, the watcher reloads
   once the db is updated.
4. `org-roam-db-sync` is scheduled through the debounced syncer.
   Multiple writes within `--sync-debounce` (default 2 s) coalesce
   into one sync; concurrent syncs are serialized to avoid the
   `SQLITE_BUSY` race against Emacs.

The `sync_database` tool is the *observable* surface of all of this.
A good post-write assertion is:

```jsonc
{ "force": false }   // report only
```

and check `drift.missing_in_sqlite` is empty for the ids you wrote.

---

## Quirks worth knowing

These come from the README but are worth restating because they
change how an agent should batch its calls:

- **Serial `create_node` over some MCP clients.** A few clients
  mis-serialize parallel `create_node` calls, surfacing a
  `args: must be string` validation error on a non-deterministic
  subset. Call `create_node` one at a time for batches. After a
  batch, listing the resulting `.org` files on disk tells you
  which writes landed.
- **Em-dashes in titles amplify the above.** A title containing
  `—` (U+2014) trips the client serializer more often, especially
  when the body has `[[id:…][…]]` links. Prefer ASCII `-` / `--`
  in titles. Em-dashes in the body are fine.
- **Emacs is the source of truth in sqlite mode.** The
  `client-only` sync mode (default) talks to your live Emacs
  session, not a fork. If your Emacs isn't running, the call
  returns a warning and the db stays stale until you start it
  (or restart the server in `--sync-mode full`).
- **Read-only mode hides write tools entirely.** When the server
  is started with `--read-only`, the write tools are removed from
  the tool router at construction time — they are not listed and
  not callable. This is the right setting for a shared vault
  exposed to a less-trusted agent.
- **Anchors are node-scoped.** `get_node_section` resolves the
  anchor *within the node's body*, so an anchor in a sibling
  headline of the same file never matches. This matches how
  `org-roam`'s `::` link suffixes work.

---

## See also

- [README](../README.md) — install, configure, CLI flags, project
  layout.
- `src/tools/query.rs`, `src/tools/write.rs`, `src/tools/content.rs`
  — the source of truth for tool semantics. If the docs and the
  source ever disagree, the source wins.
