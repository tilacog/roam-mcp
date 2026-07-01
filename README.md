# org-roam-mcp

An MCP server for [org-roam](https://www.orgroam.com/), written in Rust.

It lets MCP clients like Claude Desktop or Claude Code search, read, traverse, and create org-roam notes (nodes, headlines, anchors, backlinks, reflinks, unlinked references) without disturbing a running Emacs.

## Features

Read tools: `search_nodes`, `list_nodes`, `list_orphans`, `list_external_links`, `search_text`, `get_node`, `get_node_by_path`, `get_node_section`, `get_backlinks`, `get_forward_links`, `find_by_ref`, `get_refs`, `list_tags`, `tag_cooccurrences`, `list_anchors`, `unlinked_references`, `validate_node`, `find_invalid_nodes`, `get_daily_note`, `list_dailies`, `server_info`, `sync_database`, `list_tasks`, `get_outline`, `list_files`, `list_node_tags`, `has_tag`, `search_by_tag`.

Write tools: `create_node`, `update_node`, `delete_node`, `rename_node`, `append_to_node`, `prepend_to_node`, `add_link`, `insert_anchor`, `daily_capture`, `add_tag`, `remove_tag`, `set_tags`, `create_database`. Writes never touch `org-roam.db` directly, with one exception: `create_database` builds the `org-roam.db` cache from `.org` files when Emacs is not available. In `--read-only` mode they are removed from the tool router entirely.

A few details worth knowing:

- The scanner and the SQLite backend are pinned to the same observable behavior by a shared conformance test suite. The scanner understands both org-roam v2 (`#+filetags:`, `ROAM_REFS`) and v1 (`#+ROAM_TAGS:`, `#+ROAM_KEY:`) keywords.
- `[[id:<slug>]]` links resolve by basename slug too, so hand-written and agent-written links keep producing backlinks.
- `search_nodes` uses a tiered scorer: exact (1000), prefix (900), substring (700), subsequence with a density bonus (1 to 50).
- Sub-node addressing mirrors how org `::` suffixes work: `<<target>>`, `CUSTOM_ID`, headline titles, free text.
- Resources: `org-roam://node/{id}`, `org-roam://node/{id}#{anchor}`, `org-roam://vault/`.
- Prompts: `summarize-node`, `link-suggestions`, `orphan-triage`, `tag-suggestions`.
- After each write, a debounced `org-roam-db-sync` runs via `emacsclient` (live) or `emacs --batch` (fallback).

For the full tool reference and recipes, see [`docs/HOW_TO_USE.md`](docs/HOW_TO_USE.md).

## Quick start

Build:

```bash
cargo build --release
# binary at ./target/release/org-roam-mcp
```

Configure Claude Desktop. Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or the equivalent for your OS:

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

Configure Claude Code:

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

Default transport is stdio. Never mix stdout logging with the protocol, since it breaks the JSON-RPC channel.

### Companion CLI (`org-roam-cli`)

A second binary that calls the same library code as the MCP server, so output matches what an MCP client would receive.

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
org-roam-cli --roam-dir ~/org create-db
```

`--no-db` and `--db-path` work the same as the MCP server.

## How it works with Emacs

- `org-roam.db` is opened read-only (`SQLITE_OPEN_READ_ONLY` plus `PRAGMA query_only = 1`). `SQLITE_BUSY` is retried with backoff.
- All writes go to files via write-to-temp plus atomic rename. If an Emacs lockfile (`.#filename`) is present, the write is refused.
- After each write, `org-roam-db-sync` runs (see `--sync-mode`):
  1. `client-only` (default): runs `emacsclient --eval '(org-roam-db-sync)'` inside your live session.
  2. `full`: falls back to `emacs --batch` with a minimal `sync.el` when no daemon is reachable.
  3. `never`: skip entirely. Use `M-x org-roam-db-sync` manually, or rely on `org-roam-db-autosync-mode`.
- Multiple writes within the debounce window (default 2s) coalesce into one sync call. Concurrent syncs are serialized to avoid SQLite write-lock races.

## Creating `org-roam.db` without Emacs

The preferred way to build or refresh the `org-roam.db` cache is to let Emacs do it via `org-roam-db-sync` (triggered by `sync_database` with `force:true`, or automatically after each write). When Emacs is **not** available, use the native populator:

- MCP tool: `create_database`
  - `db_path`: optional override (defaults to `<roam_dir>/org-roam.db` or `--db-path`).
  - `overwrite`: `false` by default; set `true` to replace an existing database.
  - `validate`: `true` by default; opens the new database and reports its node count.
- CLI: `org-roam-cli --roam-dir ~/org create-db [--db-path PATH] [--overwrite]`

The native populator reuses the same filesystem scanner that powers `--no-db` mode, so the resulting database matches the scanner's view of the vault. It writes the org-roam v20 schema (`files`, `nodes`, `aliases`, `tags`, `refs`, `links`, `citations`) and uses SHA1 file hashes compatible with `org-roam-db-sync`. Link and citation positions are extracted from the source files; only full node property drawers are left as `nil` until `org-roam-db-sync` refreshes them.

Because the server normally opens `org-roam.db` read-only, the populator is the only code path that writes the database file directly. It is gated as a write tool and is unavailable in `--read-only` mode.

## Daily notes

`daily_capture` writes to `--dailies-dir` (default: the roam-dir root). If your vault follows the org-roam-dailies layout (`notes/daily/2026-01-12-dracula-readalong.org`), launch the server with:

```
--dailies-dir daily --dailies-format %Y-%m-%d
```

so daily notes land where Emacs expects them. The `server_info` tool reports `dailies.dir` and a `dailies.hint` when it is null, so misconfiguration is visible from the MCP side.

## Quirks

These are client- or transport-side gotchas, not server bugs:

- Batch `create_node` over some MCP clients. A few MCP clients (e.g. pi's gateway) mis-serialize multiple `create_node` calls issued in a single parallel tool block. Errors surface as `args: must be string` on a subset of calls, non-deterministically, with no indication of which one failed. The server handles each call correctly in isolation. Workaround on the caller side: call `create_node` serially for batches, and retry any failed call on its own. Listing the resulting `.org` files on disk tells you which writes landed.
- Em-dashes in titles amplify the above. A title containing `—` (U+2014) triggers the client serializer bug more often, especially when the body also contains `[[id:…][…]]` links. Use ASCII `-` or `--` in titles. Em-dashes in the body are fine.
- No `org-roam.db` warning in scanner mode. When the server runs with `--no-db` (or `org-roam.db` does not exist), `server_info` and `sync_database` return `"warnings": ["no org-roam.db present; drift is scanner-only"]`. The warning is informational, since the scanner *is* the index in that mode. It logs at `debug` level to avoid drowning out real issues.
- `update_node body` is the body, not the file. Passing the whole file (or anything starting with `:PROPERTIES:` / `#+title:`) used to silently produce nested drawers and a concatenated title. The tool now rejects such bodies with a clear `invalid_params` error. Pass only the lines you want after the header, and use the dedicated parameters for title, tags, aliases, refs, and the `:PROPERTIES:` drawer.
- `add_link` / `append_to_node` / `daily_capture headline=` must match the headline *title*. An unknown headline used to silently append content at the end of the file. The tools now refuse with `headline not found`.

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

### Emacs integration tests

The `emacs-tests` Cargo feature verifies that a database created by the native
Rust populator can be read by Emacs org-roam. It is off by default because it
requires Emacs and the org-roam package.

```bash
# Install Emacs and org-roam, then:
cargo test --features emacs-tests --test emacs_populator_roundtrip
# or:
just emacs-tests
```

The test auto-skips when Emacs or org-roam are missing. CI runs it with
`emacs-nox` and org-roam from MELPA.

## License

MIT OR Apache-2.0
