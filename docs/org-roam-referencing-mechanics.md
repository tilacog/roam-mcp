# Referencing & Citing Mechanics in Org-roam

A compact field guide to every way you can point at things — from plain Org machinery to org-roam's own layer on top of it.

## 1. Org-native linking

### `id:` links
The backbone of org-roam. Any file or headline with an `:ID:` property can be linked by UUID:

```org
[[id:3f2a8c1e-9b47-4d2a-8e51-7c0d9f6b2a14][Psalm 23]]
```

Rename-proof and move-proof: the link survives file renames and heading refiles. Create IDs with `org-id-get-create`.

Since Org 9.6, `id:` links accept a `::` search suffix:

```org
[[id:UUID::verse-4]]        ; jump to a dedicated target
[[id:UUID::some text]]      ; free-text search fallback
```

### `file:` links with search options
Older, path-based, fragile to renames — but supports the richest search syntax:

```org
[[file:notes.org::42]]            ; line 42
[[file:notes.org::*Heading]]      ; headline by title
[[file:notes.org::#custom-id]]    ; CUSTOM_ID property
[[file:notes.org::my-target]]     ; dedicated target or text search
```

### Dedicated targets `<<target>>`
Explicit anchors placed anywhere — next to a paragraph, verse, table row:

```org
<<verse-4>> Yea, though I walk through the valley...
```

Linked via `[[id:UUID::verse-4]]` or internally as `[[verse-4]]`. Robust to surrounding edits.

### Radio targets `<<<term>>>`
Define once; *every* literal occurrence of the term in the same file automatically becomes a link back to the definition. File-local only.

### `CUSTOM_ID` headline property
A stable, human-readable anchor for headlines (also becomes the HTML/LaTeX anchor on export):

```org
* Shepherd imagery
:PROPERTIES:
:CUSTOM_ID: shepherd
:END:
```

Linked as `[[#shepherd]]` (same file) or `[[file:notes.org::#shepherd]]`.

### Internal links
Within a single file: `[[*Some Headline]]`, `[[#custom-id]]`, or `[[My Target]]` (matches dedicated targets, then `#+NAME:` elements, then fuzzy text).

### `#+NAME:` cross-references
Name any block, table, or figure and link to it:

```org
#+NAME: growth-table
| year | nodes |
```

→ `[[growth-table]]`. On export these become numbered cross-references.

### Code references
Label lines inside src blocks with `(ref:label)` and link as `[[(label)]]` — line-level anchors in code.

### Footnotes
`[fn:1]`, inline `[fn:: text]`, or named `[fn:name]` — light intra-document referencing with export support.

### Citations (`org-cite`, Org 9.5+)
Native bibliographic citations against a `.bib` file:

```org
#+bibliography: refs.bib
[cite:@nora2023; see @smith2020 p. 41]
```

Styles like `[cite/t:@key]` (textual), rendered on export via citeproc or natbib/biblatex. Front ends: **citar**, **org-ref** (the older `cite:key` ecosystem).

## 2. Org-roam layer

### Nodes
Org-roam's unit of reference: any **file** or **headline** bearing an `:ID:`. Paragraphs are *not* nodes — anchor them with dedicated targets, or promote them to headlines.

### `org-roam-node-insert`
Interactive insertion of `id:` links by node title; creates the node if missing. Append `::target` by hand for sub-node precision.

### Backlinks
Org-roam indexes every `id:` link in `org-roam.db`; the `org-roam-buffer` shows who links *to* the current node, with surrounding context. Granularity is node-level — `::` suffixes are not separately indexed.

### `roam:` links
Title-based links (`[[roam:Some Title]]`), mostly a v1 legacy / capture convenience; org-roam replaces them with `id:` links when the target resolves.

### `ROAM_REFS` property
Marks a node as *the* note for an external resource — a URL or a citation key:

```org
:PROPERTIES:
:ID:       ...
:ROAM_REFS: https://example.com/article @nora2023
:END:
```

Any `[cite:@nora2023]` or link to that URL elsewhere then shows up as a **reflink** to this node. This is how literature notes get tied to bibliography entries.

### `ROAM_ALIASES` property
Alternate titles for a node, so it's findable (and completable) under several names:

```org
:ROAM_ALIASES: "Ps 23" "The Shepherd Psalm"
```

### Unlinked references
The backlink buffer can also surface places where a node's title/alias appears as plain text without a link — candidates for explicit linking.

### Bibliography integrations
- **citar-org-roam** — citar as the front end; creates/jumps to the org-roam note for a cite key via `ROAM_REFS`.
- **org-roam-bibtex (ORB)** — the equivalent bridge for the org-ref/helm-bibtex ecosystem.

## 3. Quick chooser

| You want to reference… | Use |
|---|---|
| A whole note or heading | `id:` link |
| A specific paragraph/verse | `<<target>>` + `[[id:UUID::target]]` (Org ≥ 9.6) |
| A headline with a readable anchor | `CUSTOM_ID` + `[[id:UUID::#anchor]]` |
| A table, figure, or block | `#+NAME:` + internal link |
| A line of code | coderef `(ref:label)` |
| A book/paper | `org-cite` + `ROAM_REFS` on its literature note |
| A URL's canonical note | `ROAM_REFS` |
| Same term everywhere in one file | radio target `<<<term>>>` |
