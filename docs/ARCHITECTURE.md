# Architecture

How Marius is put together, and why it is put together that way. Requirement
IDs refer to `knowledge_management_requirements.md`; the full mapping is in
[TRACEABILITY.md](TRACEABILITY.md).

## The shape of the problem

A knowledge manager has an unusual constraint: **the application is not the
system of record.** The user's Markdown files are, and they can change behind
your back — edited in another editor, moved in a file manager, restored from a
backup, synced by a tool that rewrites timestamps. Everything else follows from
taking that seriously (ARCH-005, KB-005).

So the data has three tiers, and knowing which tier a fact lives in answers
most design questions:

| Tier | Holds | On conflict | Recoverable? |
|---|---|---|---|
| **Markdown files** | Document content, frontmatter, inline tags, links | Always wins | They *are* the backup |
| **Database, authoritative** | Stable IDs, AI tag provenance, review decisions, suppressed relationships, revision history, change journal | Preserved | **No** — this is what must not be lost |
| **Database, derived** | FTS5 index, passages, vectors | Discarded and rebuilt | Yes, from the tiers above |

`Database::rebuild_derived` truncates exactly the third tier, which is what
makes NFR-007 and integrity rule 3 true rather than aspirational. The
`migrations::DERIVED_TABLES` constant is the single list, so a new derived table
cannot be forgotten.

## Crate graph

```text
                       ┌──────────────┐
                       │ apps/cli     │  the `marius` command
                       └──────┬───────┘
                              │
                       ┌──────▼───────┐
                       │ core         │  services: vaults, documents, rename,
                       └──┬───┬───┬───┘  tags, search, graph
              ┌───────────┘   │   └──────────────┐
      ┌───────▼──────┐ ┌──────▼──────┐   ┌───────▼──────┐
      │ indexing     │ │ search      │   │ storage      │
      │ scan/files/  │ │ query lang  │   │ SQLite, FTS5 │
      │ reconcile    │ └──────┬──────┘   │ journal      │
      └───┬──────┬───┘        │          └───────┬──────┘
          │      └────────────┴──────────────────┤
   ┌──────▼───────┐                      ┌───────▼──────┐
   │ markdown     │                      │ model        │
   │ parse/spans  │─────────────────────▶│ vocabulary   │
   └──────────────┘                      │ no I/O       │
                                         └──────────────┘
```

Dependencies point one way only. `model` has no I/O, no database and no async
runtime, so the rules that protect the user's content are unit-testable in
isolation and reusable unchanged on desktop, on iOS, and inside the sync
service (ARCH-001, UI-008).

## The four invariants, and where each lives

The product's trustworthiness rests on four rules. Each is implemented **once**,
in the lowest layer that can express it, so no caller can get it wrong.

### 1. Identity is permanent

`model::ids` — UUIDv7 typed identities, opaque above that module.
`document.path` is an ordinary mutable column; `document.id` is what every tag,
relationship, revision and embedding references. Renaming is therefore an
`UPDATE` of one column (KB-006, ARCH-011, integrity rules 1–2).

The hard part is recognising a move made *outside* the application.
`indexing::reconcile` treats a disappeared path plus an appeared path with the
same content hash as one document that moved. Without this, a drag in Finder
would mint a new ID and break every link pointing at the note.

### 2. Source content belongs to the user

`model::provenance::Authorship::is_source_protected`. Writes to a user's file
happen in exactly three places, all user-initiated:

- `core::Marius::save_document` — you pressed save.
- `core::Marius::relocate` → `markdown::rewrite` — you renamed a document, so
  the `[[links]]` pointing at it are updated (MD-007).
- `markdown::frontmatter::upsert_key` — you explicitly authorised a writeback
  (TAG-009).

The indexer never writes. Nothing reachable from a model's output writes.

All writes go through `indexing::files::write_atomic`: temp file in the same
directory, flush, `fsync`, rename, `fsync` the directory. A crash leaves either
the old file or the new one, never a fragment (NFR-001).

Frontmatter is edited by *surgery on the original bytes*, never by
reserialising. A YAML round-trip would reorder keys, drop comments and requote
strings — showing up as noise in the user's own git history. "Preserve unknown
fields" (MD-008) is read as "preserve the bytes".

### 3. Your decisions outrank the machine's

`model::provenance` reduces the whole question to one predicate:

```rust
provenance.is_machine_writable() && !review.pins_assertion()
```

Everything that could overwrite user intent consults it. It is enforced at the
*storage* boundary — `tags::assign` and `relationships::upsert` refuse rather
than trusting callers — so a future classifier cannot bypass it by accident.

Each writer owns only its own rows, which is subtler than it sounds and was
worth a dedicated pair of functions:

| Writer | May replace | Must not touch |
|---|---|---|
| Indexer (`replace_file_tags`, `replace_link_edges`) | `derived` rows read out of the file | A model's proposals, your manual tags |
| Classifier (`replace_machine_tags`, `replace_machine_edges`) | `ai_generated` rows it created | The inline `#tags` in your body |

**Rejections are their own tables**, keyed by *what was rejected* rather than by
the row that was deleted. A later pass proposing the same tag or edge — with a
fresh ID, from a different model — is still recognised and suppressed. That is
what makes AC-006 and integrity rule 7 survive reindexing and sync.

A directed edge belongs to the document it starts from. Re-reading `b.md` must
not delete the backlink that `a.md`'s file asserts; `replace_edges` scopes its
`DELETE` by `source_document` for directed kinds and by either endpoint for
undirected ones.

### 4. Privacy restrictions only tighten

`model::policy::Sensitivity` is ordered, and combining takes the maximum. An
unlabelled or imported document defaults to `Personal`, not `Unrestricted`
(AI-007). `Sensitivity::across` over an empty set yields the conservative
default, so "nothing to check" can never widen permission. One confidential
source restricts a whole request (AI-008). A document's own override can only
tighten its vault's floor — including when the override is read back out of
frontmatter on every reindex.

## Decisions worth explaining

### Span-accurate Markdown scanning, by hand

`markdown::scan` is hand-written rather than delegated to the CommonMark
parser, for two reasons. MD-007 requires *rewriting* a link inside the user's
file, which needs the exact byte range of the target text — not something an
event-based parser exposes. And `[[wiki links]]` are not CommonMark at all, so
a parser sees them as ordinary text it may split across events arbitrarily.

The scan masks code, math and HTML comments first, then extracts from what is
left, so a `#tag` inside a fenced block is sample text rather than a tag. The
tests enumerate the false positives this prevents: headings, URL fragments,
HTML entities, `issue#42`, `#2024`.

`markdown::text` still uses pulldown-cmark for what it is genuinely good at —
flattening tables, lists, footnotes and block quotes into indexable prose.

### Shared-tag edges are computed; inferred edges are stored

A tag on 2,000 notes implies two million pairwise edges. `shared_tag_neighbours`
derives them on demand, ranked by overlap. Inferred and user-asserted edges
*are* stored, because they cannot be recomputed and must survive reindexing.
This is the spec's own graph-interpretation note, made structural: storing a
`SharedTag` row is a hard error.

### Cross-vault edges carry no foreign key

A cross-vault edge has to keep meaning something while the far vault is on an
unmounted drive or a detached device. A `REFERENCES document(id)` would make
that impossible, so a dangling endpoint is a normal state that read paths
resolve, not a violation the database forbids (SYNC-012, AC-009). Removing a
vault's *registration* leaves the edge metadata standing.

### The change journal exists before sync does

Every mutation appends a `SyncOperation`, whether or not a backend is
configured. The spec's development note requires sync-ready change tracking
from the first phase; until a backend exists the journal is the undo and audit
trail that SEC-006 and AI-010 ask for. Deletions replicate as tombstones, never
as absence, so a device that was offline during a delete cannot resurrect the
row. Ordering uses a durable Lamport counter rather than a wall clock, because a
laptop and a phone disagree about the time.

### Reconciliation has two speeds

`Thoroughness::Quick` trusts matching size *and* mtime and skips the hash,
which is what keeps a two-second vault open (NFR-003) reachable; a full hash
means reading every byte of every note on every open. The cost is that an edit
preserving both size and mtime to the second is invisible — the same trade-off
`git status` makes. `Full` hashes unconditionally, `Rebuild` re-emits
everything, and both are reachable from the CLI, so nothing is unrecoverable.

### Search: two candidate sources, filtered in memory

Relevance lives in FTS5; tag logic, dates and document state live in ordinary
tables. Rather than forcing both into one statement, whichever side is
selective produces candidates and the other filters them. The honest cost is
that a text query whose candidates are mostly filtered out can under-report;
`SearchResults::truncated` says so plainly instead of returning a quietly short
answer.

BM25 is weighted per column — title 10, aliases 8, tags 5, path 2, body 1 —
because the names a person remembers should beat a passing mention in a long
note. There is a test asserting that ordering rather than assuming it.

### Embedding vectors are keyed by their space

A vector is stored under `model@version:dimensions`. Vectors from different
spaces are never compared, and a model change invalidates rather than silently
mixing (the specification's open question 5). Similarity is a brute-force cosine
scan: exact, no index to keep in sync, and fast enough well past 100,000
chunks. An approximate index earns its complexity only beyond that.

## What is scaffolded but not built

`graph`, `llm`, `llm-policy`, `classification`, `embeddings` and `import` exist
as crates with their contracts expressed in `model` — `InferenceTask`,
`Sensitivity`, `Locality`, `InferenceRecord`, `Citation`, `ClaimSupport`,
`GeneratedNoteProvenance` — and with the storage tables they will need already
migrated. `ui-gpui`, `apps/desktop` and `apps/sync-server` are excluded from the
workspace: GPUI is a fast-moving git dependency, and the sync service belongs to
a later phase. See [STATUS.md](STATUS.md).

The ordering is deliberate. The specification's development note asks for
security, durable identities, sync-ready change tracking and source-content
protection to be designed in from the first phase — so those were built first,
and the layers that depend on them can be added without renegotiating any of
the four invariants.
