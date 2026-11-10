# Marius

A local-first personal knowledge management system. Your notes stay ordinary
Markdown files on your own disk; Marius builds an index, a tag system and a
knowledge graph around them without taking them hostage.

Named for **Marius Michel Pasha** (1819–1907), a French sailor who was
shipwrecked off the Ottoman coast and spent the rest of his life putting
lighthouses where ships kept being lost — more than a hundred of them.

> **Status: the full system is built and in daily use.** Core, CLI and a
> native GPUI desktop app (macOS and Windows, both verified in use): editor,
> reading mode and live split, knowledge graph, tabs, command palette,
> settings. AI tagging, semantic/hybrid search and grounded Q&A run behind a
> privacy policy engine with a full audit log — local Ollama or any
> OpenAI-compatible endpoint. Importers (HTML, PDF, Word, EPUB, `.eml`,
> Outlook `.msg`, `.mbox`, web clip) with content-searchable attachments,
> filesystem watching, self-hosted sync, JavaScript plugins. Remaining gaps
> are listed honestly in [docs/STATUS.md](docs/STATUS.md).

## What it does today

```console
$ marius vault add Research ~/notes
3 added, 0 modified, 0 moved, 0 removed, 0 unchanged in 6 ms

$ marius search 'harbour tag:history -is:archived'
The Pharos of Alexandria
  Research:notes/alexandria.md
  Built on the island of Pharos … it stood at the {harbour} mouth …
  #antiquity #history #lighthouse

$ marius rename michel-pasha "Marius Michel Pasha"
research/michel-pasha.md -> research/Marius Michel Pasha.md
  updated 1 link(s) in 1 file(s)
    notes/alexandria.md

$ marius graph notes/alexandria.md
links to:
  -> research/Marius Michel Pasha.md  [link]
linked from:
  <- research/Marius Michel Pasha.md  [link]
shares tags with:
  ~  research/Marius Michel Pasha.md  (1 tag(s) in common)
```

## The four promises

Everything in the design follows from four rules, each enforced in one place
rather than scattered across the code:

1. **Your files are yours.** Markdown stays authoritative. Marius reads;
   it writes to a source document only when you ask it to — saving an edit, or
   updating `[[links]]` after a rename — and every write is atomic, so a crash
   cannot leave you with half a note.

2. **Identity is permanent.** A document's identity is independent of its
   filename, its folder and its vault. Rename a note in Finder and Marius
   recognises it by content, keeps its ID, and keeps every tag, backlink and
   graph edge pointing at it.

3. **Your decisions outrank the machine's.** Tags and relationships carry
   provenance. Automatic classification may only replace what it created
   itself. When you reject a suggestion, the rejection is stored as its own
   durable record — so reindexing, re-running a model, or syncing from another
   device cannot bring it back.

4. **Privacy restrictions only tighten.** Sensitivity levels are ordered, and
   combining them takes the strictest. An unlabelled document is treated
   conservatively, and one confidential source restricts a whole request.
   Nothing automatic can relax a restriction.

## Platforms

The core and the CLI are pure Rust with a bundled SQLite, so there is nothing
platform-specific to install. Developed on macOS; CI builds, tests and
smoke-tests Windows and Linux on every push. The desktop app is verified in
real use on macOS and Windows; Linux builds but is untested — see
[docs/STATUS.md](docs/STATUS.md).

Marius refuses to create filenames that would not open on Windows (a colon, a
reserved device name), always indexes existing ones anyway, and `marius doctor`
lists any already in your vault with a command to fix them.

## The desktop app

```console
$ cd apps/desktop && cargo run --release -- ~/path/to/your/notes
```

Folder tree when idle, live search as you type. Drag a note or a whole folder
within the tree to move it (links are rewritten); drop files from the OS onto
the window to import them.

### Keyboard shortcuts

| macOS | Windows / Linux | Does |
|---|---|---|
| ⌘K (also ⌘P, ⌘⇧F) | Ctrl+K | Focus search · Enter opens · Esc back |
| ⌘N | Ctrl+N | New note — the search box is the name; `folder/name` creates folders |
| ⌘S | Ctrl+S | Save (switching documents autosaves) |
| F2 | F2 | Rename or move the open note — edit the path, links are rewritten |
| ⌘E | Ctrl+E | Reading mode (wiki links and attachments clickable) |
| PgDn / PgUp, Space / ⇧Space, ↑↓, Home / End | same | Scroll the reading view (mouse wheel works too) |
| ⌘⇧E | Ctrl+Shift+E | Live side-by-side split |
| ⌘G | Ctrl+G | Knowledge graph |
| ⌘B | Ctrl+B | Backlinks / related panel |
| ⌘⇧P | Ctrl+Shift+P | Command palette (every action, plugins, templates, daily note) |
| ⌘T | Ctrl+T | Ask the model for tags — review with ✓ / ✗ on the chips |
| ⌘, | Ctrl+, | Settings: models per task, trust, vault privacy floor |
| ⌘⇧T / ⌘W | Ctrl+Shift+T / Ctrl+W | Open / close tab |
| ⌃Tab / ⌃⇧Tab | Ctrl+Tab / Ctrl+Shift+Tab | Next / previous tab |
| ⌘+ / ⌘− / ⌘0 | Ctrl++ / Ctrl+− / Ctrl+0 | Zoom content fonts / reset |
| ⌘⇧L | Ctrl+Shift+L | Toggle dark / light theme |
| ⌘R | Ctrl+R | Reindex the vault by hand (the watcher usually beats you to it) |
| Tab / ⇧Tab | Tab / Shift+Tab | In a pipe table: next / previous cell; Enter continues the table |

In the search box, `?query` searches by meaning and `??query` fuses semantic
with keyword rank (needs an embedding model configured in Settings).

## Writing notes: inline syntax

Notes are ordinary Markdown (CommonMark + GFM). Marius reads a few
conventions on top — nothing is required, and unknown frontmatter is
preserved untouched:

**Tags.** `#hardware` anywhere in the body, hierarchical with `/`:
`#hardware/switches` matches searches for `tag:hardware` too. Letters,
digits and `+ # . &` are kept; spaces, `_` and `-` normalise to `-`; case
folds (`#Moog` and `#moog` are one tag). Tags inside code fences, inline
code and URLs are ignored. Deleting the text removes the tag; tags added by
the AI or CLI live only in the index and never touch your file.

**Frontmatter.** A YAML block at the top. Marius understands:

```yaml
---
title: Harbour survey          # overrides the filename as display title
tags: [moog, hardware/switches] # `tag:` and `keywords:` also read
aliases: [survey-2026]          # alternate names [[survey-2026]] resolves
sensitivity: confidential       # unrestricted · personal · confidential · no_inference
---
```

`sensitivity:` can only tighten the vault's floor, never loosen it; a
`confidential` note is pinned to local models and a `no_inference` note is
never sent to any model.

**Wiki links.** `[[Note]]`, `[[Note|shown text]]`, `[[Note#Heading]]`,
`[[Note#^block-id]]`; `![[diagram.png]]` embeds. Links resolve by title,
path or alias; unresolved links are tracked and connect themselves when the
target appears. Renames (F2, `marius rename`) rewrite referring files.

**Callouts.** Obsidian-style, rendered with typed accents in reading mode:

```markdown
> [!warning] Mind the dredger
> The channel closes Tuesdays.
```

`note`, `tip`, `important`, `warning`, `caution`; the title is optional.

**Math.** `$e^{i\pi}$` inline and `$$ … $$` blocks are recognised and shown
distinctly (badged, scrolling sideways) — no typesetter yet.

**Tasks and tables.** `- [ ]` / `- [x]` render as checkboxes; pipe tables
get cell navigation in the editor (Tab, ⇧Tab, Enter).

**Templates and daily notes.** Files in `templates/` seed new notes
(palette → "New from template", `marius new --template`), with `{{date}}`,
`{{time}}` and `{{title}}` expanded. The palette's "Daily note" (or
`marius daily`) opens today's note, creating it from the daily template.

## Install

Requires Rust 1.89 or later (developed against 1.99).

```console
$ cargo build --release
$ ./target/release/marius --help
```

## Getting started

```console
# Open a directory you already have. Nothing in it is modified or converted.
$ marius vault add Notes ~/Documents/notes

# Keep the index current.
$ marius index

# Search. `marius help-query` explains the full syntax.
$ marius search 'lighthouse tag:history modified:>week'

# Tags, with provenance markers: = yours, ~ proposed, + you approved
$ marius tag add alexandria "ancient engineering"
$ marius tag list alexandria

# See what connects to a note.
$ marius graph alexandria

# Rename, and fix every link that pointed at it.
$ marius rename alexandria "Pharos of Alexandria"

# Check the database and index.
$ marius doctor

# Compact the index and prune old journal history.
$ marius maintain
```

Add `--json` to any command for machine-readable output.

Marius keeps its own state in `~/Library/Application Support/Marius` (macOS),
`%APPDATA%\Marius` (Windows) or `$XDG_DATA_HOME/marius` (Linux). Override it
with `--data-dir` or `MARIUS_DATA_DIR`. Nothing in that directory is needed to
keep your notes: it holds preferences and rebuildable indexes.

## Query language

```text
lighthouse harbour         both words
"harbour mouth"            exact phrase
-draft                     exclude
history OR engineering     either
light*                     prefix

tag:lighthouse             the tag, or any tag beneath it
tag:project/marius         hierarchical
-tag:draft                 not this tag
path:research              inside a folder
vault:work                 one vault only
type:email                 by document type
title:pharos               title only
modified:>2024-01-01       by date, or >week / >today
is:archived                only archived     (included by default)
is:generated               only AI notes     (included by default)
```

## Repository layout

```text
crates/
  model/           Domain vocabulary: identities, provenance, policy. No I/O.
  markdown/        Frontmatter, wiki links, tags, outline, chunking, rewriting.
  storage/         SQLite schema, repositories, FTS5 index, change journal.
  indexing/        Durable file I/O, vault scanning, reconciliation, pipeline.
  search/          Query language.
  core/            Application services: vaults, documents, search, graph.
  graph/           Link graph assembly and traversal.
  llm/ llm-policy/ Ollama + OpenAI-compatible adapters; the privacy policy engine.
  classification/  Tagging, summaries, grounded Q&A, relationship discovery.
  embeddings/      Chunk embedding and the vector store.
  import/          HTML, PDF, Word, EPUB, .eml, Outlook .msg, .mbox, web clip.
  sync-service/ plugin-host/   Sync client logic; the Boa JavaScript plugin runtime.
  ui-gpui/         Desktop UI: tested buffer/highlight models + GPUI shell
apps/
  cli/             The `marius` command
  desktop/         The desktop app (own workspace; gpui is 2.5 GB of deps)
  sync-server/     Self-hosted sync: one binary, one SQLite file, a bearer token.
docs/
  ARCHITECTURE.md  How it fits together and why
  TRACEABILITY.md  Every requirement ID → where it is implemented and tested
  DECISIONS.md     The specification's open questions, answered
  STATUS.md        What works today, and what does not
  SPIKE-GPUI.md    Desktop UI feasibility: what was measured, and what wasn't
```

## Testing

```console
$ cargo test --workspace     # ~645 tests; the UI crate adds ~85 more
$ cargo clippy --workspace --all-targets
```

The suite is written against the specification rather than against the
implementation. `crates/core/tests/acceptance.rs` works through the acceptance
scenarios by their IDs — `ac_003_renaming_a_linked_document_keeps_identity_and_fixes_links`
and so on — driving real files on disk and a real database with no test
doubles.

## Contributing

Two conventions worth knowing before changing anything:

- **Every non-obvious decision cites the requirement it serves.** Module
  documentation names the requirement IDs it implements; tests say which
  acceptance condition they check. If you cannot name the requirement, that is
  worth discussing before writing the code.
- **Tests state the behaviour, not the implementation.** A test named
  `a_rejected_tag_is_not_reproposed_even_by_a_different_model` should keep
  passing through a rewrite of how rejection is stored.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
