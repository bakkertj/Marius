# Marius

A local-first personal knowledge management system. Your notes stay ordinary
Markdown files on your own disk; Marius builds an index, a tag system and a
knowledge graph around them without taking them hostage.

Named for **Marius Michel Pasha** (1819–1907), a French sailor who was
shipwrecked off the Ottoman coast and spent the rest of his life putting
lighthouses where ships kept being lost — more than a hundred of them.

> **Status: offline core + desktop app.** The core is built and tested —
> vaults, Markdown, durable file handling, SQLite indexing, search, tags with
> provenance, the link graph — and a native GPUI desktop app now sits on it:
> live search, a real Markdown editor with highlighting and word-grouped undo,
> tag browser, provenance chips, dark/light. Verified interactively on macOS.
> AI, sync and plugins are designed for but not yet implemented. See
> [docs/STATUS.md](docs/STATUS.md).

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
platform-specific to install. Developed on macOS; CI builds and smoke-tests on
Windows and Linux too. The desktop UI is not started on any platform — see
[docs/STATUS.md](docs/STATUS.md).

Marius refuses to create filenames that would not open on Windows (a colon, a
reserved device name), always indexes existing ones anyway, and `marius doctor`
lists any already in your vault with a command to fix them.

## The desktop app

```console
$ cd apps/desktop && cargo run --release -- ~/path/to/your/notes
```

Folder tree when idle (drag a note or a whole folder to move it — links are rewritten), live search as you type. ⌘K search · Enter opens · ⌘E reading mode (wiki links clickable) · ⌘N new note (`folder/name` creates folders) · ⌘S save · Esc back · ⌘⇧L theme.

## Install

Requires Rust 1.88 or later (developed against 1.99).

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
  graph/ llm/ llm-policy/ classification/ embeddings/ import/   (scaffolded)
  ui-gpui/         Desktop UI: tested buffer/highlight models + GPUI shell
apps/
  cli/             The `marius` command
  desktop/         The desktop app (own workspace; gpui is 2.5 GB of deps)
  sync-server/     (not started; excluded from the workspace)
docs/
  ARCHITECTURE.md  How it fits together and why
  TRACEABILITY.md  Every requirement ID → where it is implemented and tested
  DECISIONS.md     The specification's open questions, answered
  STATUS.md        What works today, and what does not
  SPIKE-GPUI.md    Desktop UI feasibility: what was measured, and what wasn't
```

## Testing

```console
$ cargo test --workspace     # 525 tests
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
