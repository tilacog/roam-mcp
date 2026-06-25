# org-roam-mcp

An MCP (Model Context Protocol) server for [org-roam](https://www.orgroam.com/) knowledge bases, written in Rust.

It lets MCP clients (Claude Desktop, Claude Code, etc.) **search, read, traverse, and create** org-roam notes — nodes, headlines, anchors, backlinks, reflinks, unlinked references — without trampling on a running Emacs.

## Features

- **Read tools:** `search_nodes`, `list_nodes` (paginated), `list_orphans` (notes with no edges in the `id:` link graph), `list_external_links` (notes containing non-id links), `search_text` (full-text body search), `get_node`, `get_node_by_path`, `get_node_section`, `get_backlinks`, `get_forward_links`, `find_by_ref`, `get_refs`, `list_tags`, `tag_cooccurrences`, `list_anchors`, `unlinked_references`, `validate_node`, `find_invalid_nodes`, `get_daily_note`, `list_dailies`, `server_info`, `sync_database`, `list_tasks`, `get_outline`, `list_files`, `list_node_tags`, `has_tag`, `search_by_tag`.
  - `list_node_tags` reads a node's `#+filetags:` (plus v1 `#+ROAM_TAGS:`) tags from disk, so on-disk truth wins. `has_tag` is the boolean check (exact, case-sensitive). `search_by_tag` finds nodes bearing a tag (exact, case-sensitive, paginated) and works across both backends — the scanner's tag filter is case-insensitive, so `search_by_tag` applies an exact re-filter to guarantee uniform behaviour. All three work for file-level and headline nodes (filetags are file-level).
  - `validate_node` is overloaded: pass `{body: "..."}` to validate a raw org source against the org-roam spec and structural well-formedness (returns a flat issue list with line/column, `isError: true` on failure), or pass `{id: "..."}` to run the cross-node check against the index (stale `:ID:`, empty title, dangling `id:` links, and broken or suboptimal file links).
  - `find_invalid_nodes` walks every `.org` file in the vault and returns a flat per-issue list (soft-capped at 10k entries). Read-only; never writes to disk or DB. Useful for auditing a vault after upgrading org-roam or after a bulk import. It checks for structural issues, org-roam spec violations, and broken `file:` links.
  - `list_tasks` returns nodes that carry a TODO keyword, filterable by `todo_states` (e.g. `["TODO","NEXT"]`), `priority` (`A`/`B`/`C`), and `tags`; paginated, sortable by title or priority.
  - `get_outline` returns the heading tree of the file containing a node — each entry has `level`, `title`, `todo`, `priority`, and `tags`. Useful for navigating large files without fetching the full body.
  - `list_files` enumerates every `.org` file in the vault regardless of whether it has a file-level `:ID:`. Each entry carries the absolute path, relative path, file size, Unix mtime, and the node ID + title when the index knows about the file. Complements `list_nodes` which only surfaces indexed nodes.
  - `sync_database` makes the db-sync engine observable and controllable from MCP. With `force:false` (default) it reports the sync mode, active backend, last-sync time, and the **drift** between the filesystem-scanner view and the `org-roam.db` view — un-synced writes appear as `missing_in_sqlite`, stale db rows as `missing_in_scanner`. With `force:true` it triggers a sync (`backend`: `auto` / `scanner` / `sqlite`), blocking by default or returning a `sync_id` when `wait:false`. In `--sync-mode never` it is a no-op with a warning, and it remains available (read-only with respect to `.org` files) even in `--read-only` mode. Requires `emacsclient` reachability for a `sqlite` sync; unreachability is surfaced as a warning.
- **Write tools:** `create_node`, `update_node`, `delete_node`, `rename_node`, `append_to_node`, `prepend_to_node`, `add_link`, `insert_anchor`, `daily_capture`, `add_tag`, `remove_tag`, `set_tags`. Writes never touch `org-roam.db` directly. In `--read-only` mode they are removed from the tool router entirely — not listed, not callable.
  - `create_node` and `update_node` validate the resulting file against the org-roam spec before writing. A failed validation returns a structured `isError: true` result with the issue list and does not modify disk. `update_node` with `preview: true` returns the would-be file text plus a `valid` flag and the issue list, so callers can iterate.
  - `delete_node` removes the whole file for a file-level node, or just the subtree for a headline node. `rename_node` updates the title and renames the file (preserving any leading org-roam timestamp). `prepend_to_node` is the front-insert counterpart to `append_to_node`. `add_link` writes an `[[id:...]]` link between two existing nodes.
  - `add_tag` / `remove_tag` / `set_tags` manage the file-level `#+filetags:` keyword in place. `add_tag` appends without overwriting (dedup, exact case-sensitive match, reports `added` vs `already_present`); `remove_tag` silently no-ops on absent tags; `set_tags` replaces the whole set (empty clears it). They edit the v2 `#+filetags:` keyword and strip any v1 `#+ROAM_TAGS:` keyword so there is no v1/v2 drift, and they work on file-level and headline nodes alike (filetags are file-level). Each returns the resulting tag list; a no-change call skips the write.
- **Resources:** `org-roam://node/{id}` for node content; `org-roam://node/{id}#{anchor}` for sub-node sections; `org-roam://vault/` for a JSON vault summary (node count, tag count, backend, roam dir).
- **Prompts:** `summarize-node`, `link-suggestions` (ranks vault notes by lexical overlap with a draft), `orphan-triage` (merge/link/delete recommendations for unlinked notes), `tag-suggestions` (proposes tags from the vault's existing vocabulary) — reusable MCP prompt templates. The `id` argument of `summarize-node` and `tag-suggestions` supports `completion/complete`, so clients can autocomplete node ids by title, alias, or id prefix.
- **Two index backends:** reads from `org-roam.db` when present (fast, canonical) and falls back to a filesystem scanner (`orgize` + `walkdir`) otherwise. Both backends are pinned to the same observable behavior by a shared conformance test suite. In scanner mode the index is rebuilt after every write and on external file changes, so reads always see the latest state. The scanner understands both the org-roam v2 conventions (`#+filetags:`, `ROAM_REFS`) and the v1 keywords (`#+ROAM_TAGS:`, `#+ROAM_KEY:`).
- **Sub-node addressing:** dedicated targets `<<...>>`, `CUSTOM_ID`, headline titles, and free-text search — mirrors how org `::` link suffixes work.
- **Forgiving `id:` links:** `[[id:<UUID>]]` resolves by `:ID:` as usual, but an `[[id:<slug>]]` link that names a file by its basename slug (`[[id:bistritz]]` → `20260613205004-bistritz.org`) also resolves into the link graph when the slug is unambiguous, so hand-authored and agent-written links still produce backlinks.
- **Fuzzy search:** `search_nodes` uses a tiered scorer against title, alias, and tag: exact match (1000) → prefix (900) → substring (700) → subsequence with density bonus (1–50). Results are ranked by score, so `"ztlk"` surfaces `"Zettelkasten"` even without a substring hit.
- **Automatic db sync:** after each write, triggers `org-roam-db-sync` via `emacsclient` (live session) or `emacs --batch` (fallback), with debouncing so rapid writes coalesce into one sync.

## Quick start

### Build

```bash
cargo build --release
# binary at ./target/release/org-roam-mcp
```

### Configure Claude Desktop

`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or equivalent:

```json
{
  "mcpServers": {
    "org-roam": {
      "command": "/absolute/path/to/org-roam-mcp",
      "args": ["--roam-dir", "/absolute/path/to/your/roam"]
    }
  }
}
```

### Configure Claude Code

```bash
claude mcp add org-roam -- /absolute/path/to/org-roam-mcp --roam-dir /absolute/path/to/your/roam
```

A Claude Code skill lives at `.claude/skills/org-roam-kb/` in this repo. It teaches the agent how to search, navigate, and write to the vault efficiently. To install it, symlink the directory into your skills folder:

```bash
ln -s /path/to/org-roam-mcp/.claude/skills/org-roam-kb ~/.claude/skills/org-roam-kb
```

## CLI

### MCP server (`org-roam-mcp`)

```
org-roam-mcp --roam-dir <DIR> [options]

  -d, --roam-dir <DIR>          Path to the org-roam directory (required)
  -r, --read-only               Disable all write tools
      --no-db                   Force the filesystem-scanner index backend
      --http <ADDR>             Serve over streamable HTTP (e.g. 127.0.0.1:8080)
      --db-path <PATH>          Override the location of org-roam.db

  --dailies-dir <DIR>           Subdirectory for daily notes (default: roam dir root)
  --dailies-format <PATTERN>    strftime pattern for daily filenames (default: %Y%m%d).
                                Use `--dailies-dir daily --dailies-format %Y-%m-%d`
                                to match org-roam-dailies' default layout.

  --sync-mode <MODE>            client-only (default) | full | never
  --sync-timeout <SECS>         Timeout for sync commands (default: 30)
  --sync-debounce <MS>          Coalesce writes within this window (default: 2000)
  --emacsclient-arg <ARG>       Extra arg forwarded to emacsclient (repeatable)
  --sync-init <PATH>            Custom sync.el for --sync-mode full
```

Default transport is **stdio**. Never mix stdout logging with the protocol — it will break the JSON-RPC channel.

### Companion CLI (`org-roam-cli`)

A second binary (`org-roam-cli`) calls the same library code as the MCP server, so the output is identical to what an MCP client would receive. Useful for testing and scripting without a running MCP session.

```bash
org-roam-cli --roam-dir ~/org ping
org-roam-cli --roam-dir ~/org info
org-roam-cli --roam-dir ~/org search "zettelkasten"
org-roam-cli --roam-dir ~/org node <id>
org-roam-cli --roam-dir ~/org tasks --state TODO --state NEXT
org-roam-cli --roam-dir ~/org outline <id>
org-roam-cli --roam-dir ~/org files
org-roam-cli --roam-dir ~/org tags
org-roam-cli --roam-dir ~/org backlinks <id>
org-roam-cli --roam-dir ~/org forward <id>
```

Flags `--no-db` and `--db-path` work the same as the MCP server.

## How it plays with Emacs

- `org-roam.db` is opened **read-only** (`SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only = 1`). `SQLITE_BUSY` is retried with backoff.
- All writes go to **files** via write-to-temp + atomic rename. If an Emacs lockfile (`.#filename`) is present, the write is refused.
- After each successful write, `org-roam-db-sync` is triggered automatically (see `--sync-mode`):
  1. **`client-only`** (default): runs `emacsclient --eval '(org-roam-db-sync)'` inside your live session — fast, same config, no DB lock contention.
  2. **`full`**: falls back to `emacs --batch` with a minimal generated `sync.el` when no daemon is reachable.
  3. **`never`**: skip entirely; use `M-x org-roam-db-sync` manually (or rely on `org-roam-db-autosync-mode`).
- Multiple writes within the debounce window (default 2 s) coalesce into one sync call. Concurrent syncs are serialized to avoid SQLite write-lock races.

## Daily notes

`daily_capture` writes to `--dailies-dir` (default: the roam-dir root).
If your vault follows the org-roam-dailies layout (`notes/daily/2026-01-12-dracula-readalong.org`),
launch the server with

```
--dailies-dir daily --dailies-format %Y-%m-%d
```

so daily notes land in the same place Emacs expects to find them. The
`server_info` tool reports the configured `dailies.dir` and a
`dailies.hint` when it is `null` so the misconfiguration is visible
from the MCP side without reading the source.

## Quirks

These are client- / transport-side gotchas, not server bugs, recorded so
the next caller does not lose time to them:

- **Batch `create_node` over some MCP clients.** A few MCP clients (e.g. pi's
  gateway) mis-serialize multiple `create_node` calls issued in a single
  parallel tool block, surfacing as a `args: must be string` validation
  error on a subset of the calls — non-deterministically, and with no
  indication of which call failed. The server handles each call correctly
  in isolation. Until the client is fixed: **call `create_node` serially**
  for batches, and retry any failed call on its own. Listing the resulting
  `.org` files on disk tells you which writes landed.
- **Em-dashes in titles amplify the above.** A title containing `—` (U+2014)
  triggers the client serializer bug more often, especially when the body
  also contains `[[id:…][…]]` links. Prefer ASCII `-` / `--` in titles;
  em-dashes in the body are fine.
- **No `org-roam.db` warning in scanner mode.** When the server is
  running with `--no-db` (or `org-roam.db` does not exist), `server_info`
  and `sync_database` return `"warnings": ["no org-roam.db present; drift
  is scanner-only"]`. The warning is informational — the scanner *is*
  the index in that mode — not a sign of a missing file. The warning
  is now `debug`-level in the log to stop it from drowning out
  real issues.
- **`update_node body` is the body, not the file.** Passing the whole
  file (or anything starting with `:PROPERTIES:` / `#+title:`) used to
  silently produce nested drawers and a concatenated title. The tool
  now rejects such bodies with a clear `invalid_params` error; pass
  only the lines you want after the header, and use the dedicated
  parameters for the title, tags, aliases, refs, and `:PROPERTIES:`
  drawer.
- **`add_link` / `append_to_node` / `daily_capture headline=`** must
  match the headline *title* (the matcher strips a leading `** `).
  An unknown headline used to silently append the content at the end
  of the file; the tools now refuse with `headline not found`.

## Project layout

```
src/
├── main.rs        # MCP server binary: CLI (clap), transport selection
├── bin/
│   └── cli.rs     # org-roam-cli companion binary
├── lib.rs         # re-exports
├── server.rs      # RoamServer struct + tool/prompt routers + file watcher
├── config.rs      # runtime configuration
├── index/
│   ├── mod.rs     # RoamIndex trait + shared types
│   ├── sqlite.rs  # org-roam.db reader + emacsql decoding
│   └── scan.rs    # filesystem scanner fallback
├── org/
│   ├── parse.rs   # orgize wrappers (OrgDoc, subtree ranges)
│   └── anchors.rs # <<target>>, CUSTOM_ID, headline/text search
├── sync.rs        # DbSyncer: debounced emacsclient / emacs --batch db-sync
├── util/          # slugify, atomic_write, Emacs lockfile detection
└── tools/
    ├── query.rs   # search, list_tasks, get_outline, list_files, backlinks, refs, tags
    ├── content.rs # read content, anchor resolution
    └── write.rs   # create, append, insert anchor, daily capture
```

## Testing

```bash
cargo test
just ci   # fmt check + clippy (pedantic) + tests
```

## License

MIT OR Apache-2.0
