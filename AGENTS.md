# AGENTS.md

Guidance for AI coding agents working on `org-roam-mcp` — an MCP (Model
Context Protocol) server for [org-roam](https://www.orgroam.com/) knowledge
bases, written in Rust. Read this before making non-trivial changes.

The human-facing overview is [`README.md`](README.md). The deep-dive
recipes and tool reference live in [`docs/`](docs/) (especially
[`docs/HOW_TO_USE.md`](docs/HOW_TO_USE.md)). This file is the agent
contract: build, test, style, the rules that are not negotiable, and the
traps that have cost time.

---

## 1. What this project is

- A **library-first** Rust crate (`org_roam_mcp`) with two binaries:
  - `src/main.rs` — MCP server, wires the library to stdio or streamable HTTP.
  - `src/bin/cli.rs` — `org-roam-cli` companion, calls the same library code for testing without an MCP client.
- It exposes ~28 MCP tools (read, write, prompts, resources) over a
  JSON-RPC transport. Default transport is stdio.
- The data layer has **two interchangeable index backends** behind a
  shared trait (`src/index/mod.rs`):
  - `src/index/sqlite.rs` — reads `org-roam.db` (fast, canonical).
  - `src/index/scan.rs` — filesystem walker using `orgize` + `walkdir`
    (fallback / `--no-db`).
  Both are pinned to the same observable behaviour by a shared
  conformance test suite. Don't change one backend's semantics without
  updating the other.
- After every write, a debounced `DbSyncer` (`src/sync.rs`) triggers
  `org-roam-db-sync` via `emacsclient` (live) or `emacs --batch`
  (fallback). The server itself **never writes to `org-roam.db`**.

## 2. Project layout

```
src/
├── main.rs        # MCP server: CLI (clap), transport selection
├── bin/
│   └── cli.rs     # org-roam-cli companion binary
├── lib.rs         # re-exports
├── server.rs      # RoamServer: tool/prompt routers + file watcher
├── config.rs      # runtime configuration
├── index/         # RoamIndex trait + sqlite & scan backends
├── org/           # parse.rs (orgize), anchors.rs (sub-node addressing)
├── sync.rs        # DbSyncer: debounced emacsclient / emacs --batch
├── util/          # slugify, atomic_write, Emacs lockfile detection
└── tools/         # query.rs, content.rs, write.rs
tests/             # integration tests (one per surface area)
docs/              # HOW_TO_USE.md, referencing-mechanics, emacs-tests-plan
```

## 3. Build, test, lint

Use [`just`](https://github.com/casey/just) — it is what CI runs. The
Justfile is the source of truth for the local dev loop.

| Task                      | Command                  |
|---------------------------|--------------------------|
| Build release             | `cargo build --release`  |
| Run all tests             | `cargo test`             |
| Full CI (fmt+clippy+test+CRAP) | `just ci`           |
| Format check              | `just check-fmt`         |
| Format                    | `just fmt`               |
| Clippy (pedantic)         | `just clippy`            |
| Coverage (LCov)           | `just coverage`          |
| CRAP metric gate (≤30)    | `just crap`              |

Rules enforced by `just ci`:

- `cargo fmt --check` must pass. Run `just fmt` before committing.
- `cargo clippy --all-targets -- -W clippy::pedantic -D warnings` —
  **pedantic is the baseline, warnings are denied**. Treat new clippy
  lints from your branch as failures, not as TODOs.
- The CRAP metric gates at 30 — every function must score ≤ 30 (CRAP =
  cyclomatic × (1 − coverage)² + uncalled²). Add a test that exercises
  the branch you're adding, or refactor the function to be simpler.
  Don't disable the gate.
- `cargo test` must pass. Integration tests live in `tests/`.
- Toolchain is pinned via `rust-toolchain.toml` (stable +
  `rustfmt`, `clippy`, `llvm-tools-preview`).

## 4. Style and code conventions

- **Edition 2021, MSRV 1.75** (see `Cargo.toml`).
- Modules are small and purpose-specific. If you're adding a new
  MCP tool, put it in the right file under `src/tools/`:
  - read-only data lookups → `tools/query.rs`
  - reading/anchoring node content → `tools/content.rs`
  - anything that writes files → `tools/write.rs`
  Then wire the handler in `src/server.rs` (the `#[tool_router]` /
  `#[tool_handler]`-decorated impl block).
  If the tool is useful from the command line, add a matching subcommand
  to `src/bin/cli.rs` and a `cmd_*` dispatch function.
- `search_nodes` uses a tiered fuzzy scorer (`fuzzy_score` in
  `tools/query.rs`): exact → prefix → substring → subsequence with
  density bonus. Keep these tiers in sync if you change matching logic.
- `org-roam.db` is opened **read-only** (`SQLITE_OPEN_READ_ONLY` +
  `PRAGMA query_only = 1`). `SQLITE_BUSY` is retried with backoff.
  Do not change this — writes to the DB must go through the file layer
  so Emacs stays the source of truth.
- All file writes use **write-to-temp + atomic rename** and refuse to
  proceed if an Emacs lockfile (`.#filename`) is present. See
  `src/util/`.
- The scanner must understand both **org-roam v2** (`#+filetags:`,
  `ROAM_REFS`) and **v1** (`#+ROAM_TAGS:`, `#+ROAM_KEY:`) keywords.
  Don't drop v1 support — there are still real vaults using it.
- `[[id:<UUID>]]` links resolve by `:ID:` as usual. `[[id:<slug>]]`
  links that name a file by basename slug also resolve when the slug
  is unambiguous. Preserve this forgiving behaviour when touching
  `org/anchors.rs` or link-graph code.
- Logging goes to **stderr only**. The default transport is stdio —
  any byte on stdout breaks the JSON-RPC channel. The tracing
  subscriber in `main.rs` is configured with `with_writer(std::io::stderr)`
  for exactly this reason. Don't print to stdout.
- Use `tracing::{info, debug, warn, error}` with the
  `org_roam_mcp=…` target, not `println!`/`eprintln!`.

## 5. Non-negotiable rules

These are not guidelines. If a task tells you to violate them, stop
and surface the conflict to the user.

1. **Never write to `org-roam.db` directly.** The DB is read-only from
   the server's perspective. Mutations go to `.org` files via atomic
   rename, and `DbSyncer` is responsible for telling Emacs to sync.
2. **Never print to stdout.** Stdout is the MCP transport. Logging
   belongs on stderr.
3. **Never silently delete a node.** `delete_node` exists, but it is a
   destructive tool — confirm with the user before calling it on
   anything that looks load-bearing (file-level nodes, nodes with
   backlinks).
4. **Don't break the conformance contract between backends.** A change
   to `src/index/sqlite.rs` or `src/index/scan.rs` that changes
   observable query results must come with a matching change in the
   other backend and the shared conformance tests under `tests/`.
5. **Don't bump the CRAP gate or silence clippy pedantic** to make CI
   green. Fix the code or add a test.
6. **Don't add new dependencies casually.** This is a security-sensitive
   binary that runs against the user's notes. Each new dep must
   justify its weight.

## 6. Known traps (read before you act)

These are documented in `README.md` §"Quirks". They have cost real
debugging time; the next agent should not have to re-learn them.

- **Batch `create_node` over some MCP clients.** A few MCP clients
  (e.g. pi's gateway) mis-serialize multiple `create_node` calls
  issued in a single parallel tool block, surfacing as
  `args: must be string` validation errors on a subset of the calls,
  non-deterministically, with no indication of which one failed. The
  server handles each call correctly in isolation. The workaround
  on the **caller** side: issue `create_node` calls **serially** for
  batches, and retry any failed call on its own. Listing the resulting
  `.org` files on disk tells you which writes landed.
  - This is a **client bug**, not a server bug. Do not "fix" the
    server by, e.g., weakening argument validation. The README and
    `docs/HOW_TO_USE.md` are the right places to surface this.
- **Em-dashes in titles amplify the above.** A title containing `—`
  (U+2014) triggers the client serializer bug more often, especially
  when the body also contains `[[id:…][…]]` links. Prefer ASCII `-` /
  `--` in titles; em-dashes in the body are fine.
- **`sync_database` semantics.** `force:false` reports drift between
  the scanner view and the SQLite view (`missing_in_sqlite`,
  `missing_in_scanner`); `force:true` triggers a sync, blocking by
  default or returning a `sync_id` when `wait:false`. In
  `--sync-mode never` it is a no-op with a warning. In `--read-only`
  mode it stays available read-only. Don't conflate "no writes to
  files" with "no side effects" — db-sync can still touch
  `org-roam.db` and require `emacsclient` reachability.
- **CRAP gate and the CLI binary.** `src/bin/cli.rs` has 0% test
  coverage. CRAP = CC × (CC + 1) for uncovered functions, so every
  function must have CC ≤ 5. The `dispatch` function in `cli.rs` is
  split into four `try_dispatch_*` helpers (max 3 handled arms +
  wildcard each) for exactly this reason. Follow that pattern when
  adding new subcommands.
- **`--read-only` removes write tools from the router entirely.**
  They are not listed, not callable, not stubbed. A client that asks
  for `create_node` in read-only mode will get a "tool not found"
  error, not a permission error.
- **`update_node` is idempotent, keyed on `:ID:`.** It edits in place
  so backlinks survive. If you find yourself writing a "create or
  update" helper, route through `update_node` with `preview:true`
  first when the node might exist, otherwise `create_node`.
- **Sub-node addressing mirrors org `::` link suffixes.** Dedicated
  targets `<<…>>`, `CUSTOM_ID`, headline titles, and free-text search
  are all valid anchors. Read `src/org/anchors.rs` before adding new
  addressing modes.

## 7. Test layout and how to add tests

- **Unit tests** live next to the code (`#[cfg(test)] mod tests` in
  `src/main.rs`, inline in other modules).
- **Integration tests** live in `tests/`:
  - `tests/sqlite_index.rs` / `tests/scan_index.rs` — backend-specific
    behaviour. Most new backend features get one test in each file.
  - `tests/common/mod.rs` — shared fixture builders. **Use these**
    instead of building a vault by hand in every test.
  - `tests/fixtures/` and `tests/common/sample-vault/` — golden
    fixtures; extend them rather than re-creating inline.
  - `tests/mcp_integration.rs`, `tests/new_tools.rs` — end-to-end
    tool invocations over the in-memory transport.
  - `tests/write_roundtrip.rs` — write-then-read cycles.
  - `tests/sync_database.rs` — sync semantics, drift reporting,
    debouncing.
  - `tests/http_transport.rs` — streamable HTTP transport.
  - `tests/harness_check.rs` — a small smoke test that the harness
    itself is wired correctly.
- When adding a new MCP tool:
  1. Write a test that calls it through the in-memory transport
     (`tests/mcp_integration.rs` and/or `tests/new_tools.rs` pattern).
  2. If the tool has read or write semantics that differ between
     backends, add a conformance case to **both**
     `tests/sqlite_index.rs` and `tests/scan_index.rs`.
  3. Run `just ci` locally before pushing — the PR will be rejected
     if the CRAP gate or clippy pedantic complains.

## 8. Documentation map

Read these, in this order, before changing the corresponding code:

- [`README.md`](README.md) — features, CLI flags, sync modes, quirks.
- [`docs/HOW_TO_USE.md`](docs/HOW_TO_USE.md) — practical recipes for
  the full tool surface. The most useful doc for understanding
  intended behaviour.
- [`docs/org-roam-referencing-mechanics.md`](docs/org-roam-referencing-mechanics.md) —
  how `id:` links, `file:` links, `ROAM_REFS`, and org-native
  references interact. Required reading before touching link-graph
  code.
- [`docs/emacs-tests-plan.org`](docs/emacs-tests-plan.org) — the
  Emacs-side test plan. Update this if you change anything that
  affects the `emacsclient` round-trip.

## 9. When you finish

- `just ci` must pass locally before you report the task done.
- Don't change git authorship or committer info unless the user
  explicitly asks you to for that specific change.
- Don't add a CHANGELOG entry, bump the version, or push a tag
  unless the user asks.
- If you changed user-facing behaviour (CLI flags, tool
  signatures, sync semantics), update `README.md` and the
  relevant `docs/` file in the same change. The docs and the
  code are reviewed together.
