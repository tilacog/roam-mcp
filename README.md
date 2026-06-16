# org-roam-mcp

An MCP (Model Context Protocol) server for [org-roam](https://www.orgroam.com/) knowledge bases, written in Rust.

It lets MCP clients (Claude Desktop, Claude Code, etc.) **search, read, traverse, and create** org-roam notes — nodes, headlines, anchors, backlinks, reflinks, unlinked references — without trampling on a running Emacs.

## Features

- **Read tools:** `search_nodes`, `list_nodes` (paginated), `list_orphans` (notes with no edges in the `id:` link graph), `search_text` (full-text body search), `get_node`, `get_node_by_path`, `get_node_section`, `get_backlinks`, `get_forward_links`, `find_by_ref`, `get_refs`, `list_tags`, `tag_cooccurrences`, `list_anchors`, `unlinked_references`, `validate_node`, `find_invalid_nodes`, `get_daily_note`, `list_dailies`, `server_info`, `sync_database`.
  - `validate_node` is overloaded: pass `{body: "..."}` to validate a raw org source against the org-roam spec and structural well-formedness (returns a flat issue list with line/column, `isError: true` on failure), or pass `{id: "..."}` to run the cross-node check against the index (stale `:ID:`, empty title, dangling `id:` links).
  - `find_invalid_nodes` walks every `.org` file in the vault and returns a flat per-issue list (soft-capped at 10k entries). Read-only; never writes to disk or DB. Useful for auditing a vault after upgrading org-roam or after a bulk import.
  - `sync_database` makes the db-sync engine observable and controllable from MCP. With `force:false` (default) it reports the sync mode, active backend, last-sync time, and the **drift** between the filesystem-scanner view and the `org-roam.db` view — un-synced writes appear as `missing_in_sqlite`, stale db rows as `missing_in_scanner`. With `force:true` it triggers a sync (`backend`: `auto` / `scanner` / `sqlite`), blocking by default or returning a `sync_id` when `wait:false`. In `--sync-mode never` it is a no-op with a warning, and it remains available (read-only with respect to `.org` files) even in `--read-only` mode. Requires `emacsclient` reachability for a `sqlite` sync; unreachability is surfaced as a warning.
- **Write tools:** `create_node`, `update_node`, `delete_node`, `rename_node`, `append_to_node`, `prepend_to_node`, `add_link`, `insert_anchor`, `daily_capture`. Writes never touch `org-roam.db` directly. In `--read-only` mode they are removed from the tool router entirely — not listed, not callable.
  - `create_node` and `update_node` validate the resulting file against the org-roam spec before writing. A failed validation returns a structured `isError: true` result with the issue list and does not modify disk. `update_node` with `preview: true` returns the would-be file text plus a `valid` flag and the issue list, so callers can iterate.
  - `delete_node` removes the whole file for a file-level node, or just the subtree for a headline node. `rename_node` updates the title and renames the file (preserving any leading org-roam timestamp). `prepend_to_node` is the front-insert counterpart to `append_to_node`. `add_link` writes an `[[id:...]]` link between two existing nodes.
- **Resources:** `org-roam://node/{id}` for node content; `org-roam://node/{id}#{anchor}` for sub-node sections.
- **Prompts:** `summarize_node`, `suggest_links` — reusable MCP prompt templates.
- **Two index backends:** reads from `org-roam.db` when present (fast, canonical) and falls back to a filesystem scanner (`orgize` + `walkdir`) otherwise. Both backends are pinned to the same observable behavior by a shared conformance test suite. In scanner mode the index is rebuilt after every write and on external file changes, so reads always see the latest state. The scanner understands both the org-roam v2 conventions (`#+filetags:`, `ROAM_REFS`) and the v1 keywords (`#+ROAM_TAGS:`, `#+ROAM_KEY:`).
- **Sub-node addressing:** dedicated targets `<<...>>`, `CUSTOM_ID`, headline titles, and free-text search — mirrors how org `::` link suffixes work.
- **Forgiving `id:` links:** `[[id:<UUID>]]` resolves by `:ID:` as usual, but an `[[id:<slug>]]` link that names a file by its basename slug (`[[id:bistritz]]` → `20260613205004-bistritz.org`) also resolves into the link graph when the slug is unambiguous, so hand-authored and agent-written links still produce backlinks. `search_nodes` matches a query against title, alias, **and** tag.
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

## CLI

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
├── main.rs        # CLI (clap), transport selection
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
    ├── query.rs   # search, get, backlinks, refs, tags
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
