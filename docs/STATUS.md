# Status

What works today, what is designed but not built, and what is next. Precise
per-requirement status is in [TRACEABILITY.md](TRACEABILITY.md), which is
checked against the specification on every `cargo test`.

**As of this writing: 730 tests (644 core + 86 UI), zero clippy warnings, formatted.**

## Platform position

| Platform | Core + CLI | Desktop UI |
|---|---|---|
| macOS | **verified in CI** (developed here) | works, interactively verified |
| Windows | **verified in CI** — build + full test suite pass on `windows-latest` | **works, verified in real use** (creating, renaming, importing, AI tagging against a work gateway) |
| Linux | **verified in CI** (`ubuntu-latest`) | builds in CI; untested interactively |

The core is pure Rust with SQLite built from its bundled amalgamation, so there
is no platform-specific code and no system dependency to install. Path handling
normalises to `/` internally and converts to OS paths at the boundary; CRLF line
endings are preserved; the data directory follows `%APPDATA%` on Windows; the
directory `fsync` after an atomic rename is skipped on Windows, where opening a
directory as a file is not permitted, and that is non-fatal by construction.

`.github/workflows/ci.yml` builds, tests and smoke-tests the CLI on
`windows-latest`, `macos-latest` and `ubuntu-latest` on every push. The
desktop app is used daily on Windows against a real work vault — including
`.htm`/`.msg` email import and AI tagging through a corporate
OpenAI-compatible gateway (OS trust roots and the Windows system proxy are
honoured). Linux remains the untested desktop platform.

### Filenames that do not travel

macOS permits filenames Windows cannot open: a colon (`Re: harbour survey.md` —
which is exactly what capturing an email produces), the reserved device names
`aux`, `con`, `nul`, `com1`–`com9`, `lpt1`–`lpt9`, trailing dots and spaces, and
the 260-character path limit. `marius_model::portability` checks all of these on
every platform, since the file is created on the Mac. Three behaviours:

- **Indexing an existing file: always allowed.** A note is never hidden from its
  owner because of its name.
- **Creating, renaming or moving: refused**, with a suggested name.
- **`marius doctor` reports** every file in the vault that will not open on
  Windows, with a paste-able `marius rename` command to fix it.

## The desktop application (macOS and Windows, verified in use)

The full keyboard shortcut table and the inline note syntax (tags,
frontmatter, wiki links, callouts, math, templates) are documented in the
[README](../README.md).

`apps/desktop` + `crates/ui-gpui` — a GPUI three-pane application:

- **Document explorer**: a folder tree with fold/unfold and recursive counts
  when idle; typing in the one search box (⌘K) switches to live FTS results
  with highlighted snippets, arrow navigation,
  Enter opens and focuses the editor (UI-002, UI-003, SRCH-003).
- **A real Markdown editor built on a fully-tested buffer model** — typing,
  IME contract, selection, word motions, visual-line vertical movement,
  word-grouped undo/redo, Markdown list/quote/task continuation, syntax
  highlighting from the same scanner the indexer uses, wiki-links underlined
  live (broken ones distinct), soft wrap, autosave on document switch
  (MD-001, MD-004, MD-009).
- **Tag browser with counts; properties strip** showing the open document's
  tags coloured by provenance (UI-002, UI-010).
- **Dark and light themes**, WCAG-checked by unit test, toggled live (⌘⇧L).
- **Drag and drop**: drag a note — or a whole folder — onto a folder, or
  onto the count line for the vault root, to move it. Both go through the
  same services as `marius move`, so links are rewritten in the referring
  files. A folder move rewrites all three directions — links into the moved
  set, folder-relative links out of it, and path-style links within it —
  planned before anything moves, refused whole on any collision, and the
  vacated directory chain is swept. Bare `[[wiki names]]` never need
  rewriting and are never touched.
- **Knowledge graph (⌘G)**: a force-directed canvas of the vault — explicit
  links solid, machine-proposed edges in accent, shared-tag kinship faint
  (GRAPH-013); global and local views with an adjustable hop radius
  (GRAPH-005); pan, zoom about the cursor, drag nodes (pinned while held),
  hover for titles, click to open (GRAPH-001); node ceiling with
  best-connected-survive truncation and broad-tag caps keep huge vaults
  drawable (GRAPH-014). Assembly (`core::graph`) and the deterministic
  force simulation (`ui-gpui/graph.rs`) are headlessly tested; the canvas
  is the thin part.
- **Settings in the app (⌘,)**: per-task model configuration —
  provider, model, base URL, credential *name* (never the key,
  SEC-001), trusted-with-personal toggle — with a connection test that
  lists a local Ollama's models (saying whether the chosen one is
  pulled) or dials a remote's `GET /models`, proving DNS, route, TLS
  trust and credential without sending a word of note content (also
  `marius model test <task>`); plus the vault's privacy floor and the
  auto-classify switch. Failures print the full error cause chain, and
  the footer shows a long status whole in a hover tooltip.
  Writes the same Config and VaultPolicy the CLI reads, so
  `marius model show` and the pane can never disagree.
- **Search conveniences and migration**: fuzzy title matching
  (`marius search --fuzzy`, shared matcher with the palette), saved
  searches validated at save time (`--save`/`--saved`,
  `marius searches`), sortable results; and `marius export` /
  `marius import-metadata` carry a vault's *identities* — document ids,
  tag provenance, review decisions, rejections, asserted edges — into a
  re-opened copy by path match and id adoption (KB-014).
- **Table editing (MD-013)**: inside a pipe-table row, Tab hops to the
  next cell (growing the row from the last), ⇧Tab hops back, Enter
  continues the table with an empty row, and Enter on an empty row ends
  it — the same grammar lists already speak.
- **Tabs and a saved workspace**: ⌘⇧T opens a tab (each with its own
  live buffer — switching never drops unsaved work), ⌘W closes (saving
  first; the last tab empties instead), ⌃Tab cycles; the strip shows
  dirty dots and appears only with two or more tabs. The open tabs,
  the active one and the zoom persist per vault and resume at launch.
- **Command palette (⌘⇧P)**: fuzzy-ranked access to every built-in
  action and every enabled plugin's commands; **backlinks panel (⌘B)**
  with links-in, links-out, co-tag kin and unresolved targets, each a
  click away; **daily notes and templates** (`marius daily`,
  `marius new --template`, palette entries) with `{{date}}`/`{{time}}`/
  `{{title}}` expansion; **Obsidian callouts** rendered with typed
  accents, and math blocks shown distinctly (badged, sideways-scrolling)
  until a real typesetter exists.
- **Pane-true rendering and zoom**: the preview column now fills and
  shrinks with its pane (the split view had been clipping both edges of
  any pane narrower than the 740px measure); code blocks keep their
  lines and scroll sideways instead of wrapping or clipping; ⌘+/⌘−
  scale the content fonts (editor, preview, split) with ⌘0 to reset —
  Ctrl on other platforms — and the chrome stays put.
- **Semantic search from the search box**: `?query` + Enter retrieves
  by meaning, `??query` fuses with keyword rank — both on a worker
  thread, since the query embedding is a model call. Results land in
  the ordinary list with the matching passage as the snippet.
- **The intelligence reaches the desktop**: ⌘T asks the configured
  model for tags on its own worker thread (a second WAL connection; the
  interface never freezes for a model), proposals appear as `~#tag`
  chips with ✓/✗ review in place, and a generated note carries a banner
  naming its model, sources, and whether regeneration may still touch
  it. `marius classify` works through the untagged backlog; with
  `vault policy --auto-classify true`, `marius watch` proposes tags for
  every changed note automatically (AI-010), always unreviewed, always
  behind the rejection gates.
- **Filesystem watching (IM-010)**: edits made by other programs — including
  a `git pull` or a sync client — appear in the tree, tags and search within
  a second, with no reindex. A batch containing an unfamiliar or vanished
  path takes the full-reconcile road so an external rename keeps the
  document's identity (AC-003, proven end-to-end in `core/tests/watch.rs`);
  the open document is reloaded only when the buffer is clean — unsaved
  edits always win over the disk. `marius watch` is the CLI face.
- ⌘N creates a note named from the search box — `research/ideas/Title`
  creates the folders with it, the only honest way a folder is born in a
  Markdown vault (no empty-directory ghosts); Escape loops back to search.

Interactively verified on macOS by synthesizing real keystrokes and asserting
on the files — see `apps/desktop/testing/README.md`, which also records three
macOS platform traps that masquerade as application bugs. **Rendered reading mode is in (⌘E)**: headings, emphasis, quotes, fenced
code, task lists, tables, images from the vault, and clickable wiki links —
reading mode is also how you browse. Rendering goes through a pure,
unit-tested block model; the first user-reported bug (body text drawn in the
default black over the dark theme, because `StyledText` does not inherit the
surrounding colour) is fixed by threading a theme-derived base style through
every inline. ⌘⇧E opens the live side-by-side split, the preview following every keystroke — MD-003's three modes are complete.
The editor's remaining known gaps: Home/End use logical lines under soft
wrap, no line numbers, single window, no tabs beyond the editor/preview
split.

## Works today

You can use Marius as an offline Markdown knowledge manager from the command
line.

- **Vaults** — open an existing directory in place with no conversion; several
  at once; rename, detach, remove (files always left alone).
- **Indexing** — scan, reconcile against the index, detect external edits,
  **recognise a file moved or renamed outside the application and keep its
  identity**, resumable in batches, problems reported per file.
- **Markdown** — CommonMark and GFM, YAML frontmatter with unknown fields
  preserved verbatim (and malformed blocks recovered), `[[wiki links]]` with
  aliases, headings, block refs and embeds, inline `#tags` that are not fooled
  by code fences or URLs, document outline, heading-aware passages.
- **Durability** — every write is atomic; a crash cannot truncate a note.
  Revision history with recoverable bodies; delete moves to a vault trash.
- **Search** — full-text with phrases, booleans, prefixes and highlighted
  snippets; filters for tags, path, vault, type, title, date and state;
  one global query across vaults; every result names its vault.
- **Tags** — manual, hierarchical, aliased, renameable and mergeable, with
  provenance on every assignment and durable rejections.
- **Graph** — links and backlinks, embeds, computed shared-tag kinship,
  cross-vault edges that survive a detached vault, unresolved links tracked and
  completed when their target appears.
- **Housekeeping** — `doctor`, backup, integrity check, full index rebuild that
  keeps every piece of authoritative metadata.
- **The privacy policy engine and AI tagging** — `marius model set tagging
  ollama <model>` then `marius tag suggest <note> --apply`. Every request
  passes `marius-llm-policy::decide` first: `NoInference` material is denied
  outright, `Confidential` material is pinned to local models (a self-hosted
  server is *not* local), `Personal` material goes remote only to providers
  the user marked trusted, and one restricted source restricts the whole
  request. Every request — refusals included — becomes an audit row
  (`marius audit`), with tokens, duration, and whether content left the
  device. Proposals land as `ai_generated`/unreviewed for `marius tag
  approve/reject`; rejections remain durable against reclassification.
  Adapters: Ollama (verified against a live daemon) and OpenAI-compatible
  (OpenRouter/self-hosted), keys by environment-variable name only.
- **Semantic and hybrid search, and grounded Q&A** — `marius embed` chunks
  and embeds a vault (idempotent: staleness follows the content hash, the
  embedding space is keyed by model and dimensions, and a model change
  starts the space afresh); `marius search --mode semantic|hybrid` ranks by
  cosine or fuses with BM25 by reciprocal rank; `marius ask` answers a
  question from the notes alone — numbered passages in, `[n]`-cited prose
  out, invented citations dropped, and an off-topic question refused
  *before* any model call rather than improvised over. Policy applies per
  document when embedding (a confidential note in a permissive vault is
  skipped, not sent) and the strictest retrieved source governs a question
  (AI-008). The protocol is proven over real loopback sockets; a live
  quality run awaits a real instruct/embedding model in the local daemon.
- **Self-hosted sync** — `marius-sync-server` (one binary, one SQLite
  file, a bearer token) plus `marius vault backend`, `marius vault join`
  and `marius sync`. Documents with their content, tags, assignments,
  review states, relationships and rejections replicate; derived rows
  (links, inline-tag assignments, search index) are re-derived on each
  device, so a synced vault needs no index shipping. Pull first, then
  push; at-least-once and idempotent by operation id; per-entity revision
  heads detect concurrent edits, which are surfaced and never resolved by
  discarding a side — the later writer's journal entry goes `Conflicted`
  with both halves described. A rejection made on one device suppresses
  machine proposals on every other. Deletions travel as tombstones and
  land in the receiving device's trash. Plain HTTP beyond the loopback is
  refused (SYNC-013); run the server behind a TLS proxy. Proven by
  two-device round-trip tests over real HTTP and verified live with two
  CLI data directories against the production binary.
- **Generated notes and relationship discovery** — `marius summarize`
  writes a synthesis note as a real Markdown file with a Sources section
  of wiki links, marked `Derived` and excludable from listings, with a
  provenance row naming model, prompt and sources. Regeneration replaces
  the body in place only until a person edits it — then the gate flips
  permanently and fresh runs land beside the user's copy (integrity rule
  6, AC-010); `marius note promote` adopts a note as the user's own.
  `marius discover` has embeddings nominate candidate pairs and a model
  judge each with a one-sentence rationale stored on the edge; proposals
  arrive unreviewed for `marius link confirm/reject`, and a rejected pair
  is never re-proposed. Every request audited, policy-gated per source.
- **Importers** — `marius import` takes Markdown, plain text, HTML
  (converted, title and source in frontmatter), `.eml` and Outlook
  `.msg` (headers become frontmatter, attachments land beside the note
  with a clickable "Attachments" section, and attached PDFs/Word files
  are full-text searchable) and `.mbox` (one note per message, thread
  identifiers kept). An S/MIME-signed message is unwrapped — the real
  body and attachments replace the `smime.p7m` wrapper, the signature
  noted (not verified); an encrypted one is named for what it is. Names are made Windows-portable on the way in,
  collisions get numbered variants, everything indexes immediately.
  PDF (extractor fenced; textless scans refused with an OCR pointer),
  Word and EPUB extract to searchable Markdown;
  `marius import --url` clips a web page with its styles and scripts
  stripped; files dropped onto the window import in place and the
  palette captures the clipboard into a new note. OCR remains open.
- **JavaScript plugins** — `marius plugin install/enable/grant/run`. A
  plugin is a directory with `manifest.json` and `main.js`, run inside
  Boa (pure Rust: no native toolchain anywhere, including Windows). It
  starts disabled with no capabilities; each power is granted by the
  user, one at a time, and checked at the single native entry point —
  a manifest request is never a grant. File access goes through the
  document service, so source protection applies to plugins exactly as
  to everything else; a throwing or looping plugin is an error message,
  not a crash. The `marius.*` API: search, read, write, log,
  registerCommand. The Obsidian-compatibility shim over this runtime is
  the part that remains.

## Not built

Everything above is live; this is what is not.

- **iOS** (ARCH-003, UI-007) — phase 7. CloudKit sync with it; the
  self-hosted sync service is the transport that exists.
- **OCR** — a scanned PDF (no text layer) is refused with an honest message,
  never imported as an empty note.
- **A math typesetter** — `$…$` is recognised and displayed distinctly, not
  rendered.
- **The Obsidian plugin-compatibility shim** — the Boa runtime, capability
  grants and the `marius.*` API are live; the Obsidian-shaped wrapper over
  them is not.
- **Editor conveniences** — line numbers; Home/End follow logical lines under
  soft wrap; single window.
- **Tag consolidation suggestions** (TAG-016) and a vault-wide
  `tag delete` command — per-note remove/reject exist everywhere, including
  ✗ on accepted chips.

## Honest gaps in what is marked done

- **Performance is now measured, on generated 10,000- and 100,000-document
  vaults.** The 100k run found and killed a real quadratic: SQLite's
  planner chose the low-selectivity `(provenance, review)` index for the
  per-document derived-row deletes, scanning every derived row in the
  vault for each document indexed — 30k notes took 104 s. The standard
  `+column` planner hint pins it to the primary key; the same 30k now
  takes 15 s and 100k indexes in 62 s, linear.
  (8.3 MB of Markdown, 19,835 links, 33,410 tag assignments) on Apple Silicon:

  | Target | Result |
  |---|---|
  | NFR-003: vault opens in ~2 s | **50 ms** to reconcile 10,000 unchanged documents |
  | NFR-004: search in ~200 ms | **under 10 ms** across five query shapes |
  | First full index (no target) | 21–23 s one-time, ~2.2 ms per document |
  | NFR-005: 100,000 documents | **measured**: first index 62 s, unchanged pass 3.3 s, searches 70–140 ms, instant reopen, 544 MB database (revision bodies included) |

  NFR-002 still has no UI to measure against. The first index is single
  threaded and is the obvious candidate for `rayon`.
- **The quick reconcile has a known blind spot.** An edit that preserves both
  file size and modification time to the second is invisible to it. `marius
  index --full` always hashes. See [DECISIONS.md](DECISIONS.md).
- **Search can under-report on a heavily filtered text query.** Candidates come
  from a bounded window; `SearchResults::truncated` says so rather than
  returning a quietly short answer.
- **`missing_docs` is off in `marius-model`.** Every module, type, function and
  non-obvious field is documented with the requirement it serves, but the lint
  would demand prose on `pub id: DocumentId` and 198 similar fields. Worth
  revisiting before publishing the crates to a registry.

## Recently fixed, found by measuring

Three problems that only appeared once there was a realistic amount of data:

- **The change journal grew without bound.** With no sync backend nothing ever
  acknowledged an operation, so `prune_acknowledged` could never reclaim one —
  73,263 rows, about 25 MB, from a single 10,000-document index. Local-only
  operations are now acknowledged on commit (with no remote, this database
  *is* the authority), a configurable retention window bounds them, and
  `marius maintain` prunes.
- **Two N+1 queries.** Reconciling a vault made one query per document (10,001
  for a 10,000-document vault); search fetched tags per hit. Both are single
  queries now — reconcile went from 103 ms to 50 ms.
- **`marius maintain` grew the database** from 80 MB to 96 MB, because `VACUUM`
  rewrites through the write-ahead log and the checkpoint ran before it rather
  than after. It now reclaims.

## Suggested next steps

In the order that keeps each step useful on its own:

1. **Linux desktop verification** — the app builds; nobody has used it there.
2. **Vault-wide tag management** — `marius tag delete <name>`, and surfacing
   rename/merge (storage supports both) in the CLI and palette.
3. **Parallel first index** — single-threaded today; the obvious `rayon` win.
4. **OCR for scanned PDFs**, now that attachments are content-searchable.
5. **The Obsidian plugin shim**, once a second real plugin exists to keep it
   honest.
