# Decisions

The specification's section 11 lists eight open engineering questions, and
notes they are "not blocking product requirements". Several of them do become
blocking once code exists, so each is recorded here with the decision taken,
the reasoning, and what would overturn it.

Questions 1, 2 and 8 depend on work not yet started and are deliberately left
open rather than guessed at.

---

## 1. Which iOS UI implementation?

**Open.** Unchanged from the specification.

Nothing built so far forecloses the choice, which was the point of putting the
domain rules in `marius-model` with no I/O and no UI assumption: a thin native
host can sit on the same core as GPUI (ARCH-003, UI-008). The decision wants a
GPUI-on-iOS feasibility prototype first, which is phase 0 work.

---

## 2. Which embedded JavaScript engine for plugins?

**Open.** Unchanged.

The capability model that constrains whichever engine is chosen is built
(`model::plugin`): installs land disabled with no grants, dangerous
capabilities need explicit consent, and a grant cannot widen a vault's privacy
policy. That is the part worth settling early, because it shapes the host API
regardless of engine.

---

## 3. Which frontmatter convention holds stable IDs?

**Decided: the database is authoritative; frontmatter writeback is opt-in per
vault.**

A new vault does not write anything into the user's files. `Vault::write_ids_to_frontmatter`
turns on a `marius-id` key, and `frontmatter::upsert_key` writes it by surgery
on the original bytes.

The reasoning: writing an ID into frontmatter by default would modify *every
file in the vault* on first open, which is an appalling first impression for a
tool whose main promise is that it does not touch your files. Identity across
renames and moves does not need it either — `reconcile` recognises a moved file
by content hash, which is how a drag in Finder keeps its ID without any
cooperation from the file.

What writeback *does* buy is identity that survives outside Marius: a vault
copied to a machine with no database, or an ID that must stay stable through a
tool that rewrites files wholesale. Those are real needs, so the mechanism
exists and the user chooses.

Overturned if: identity turns out to be lost often enough in practice that the
default should flip. The migration is additive either way.

---

## 4. Conflict resolution for concurrent Markdown edits?

**Decided: revision-based detection now, three-way merge when sync lands. Not a
CRDT.**

`SyncOperation::conflicts_with` detects a conflict from matching per-entity
revision counters stamped by different devices. SYNC-009 forbids silently
discarding either side, so a conflict becomes a `SyncState::Conflicted` row that
is never pruned by housekeeping, and the user decides.

Against a CRDT text type: it would mean the file on disk is no longer the whole
truth — reconstructing a document would require CRDT state, which conflicts
directly with ARCH-005 and with the promise that a vault is readable by any
other editor. A CRDT also cannot represent the thing that actually needs
converging here, which is *metadata* with provenance and review state.

A three-way merge fits the data that already exists: `document_revision` keeps
the common ancestor's hash and body, so a merge has all three sides without any
new storage. An optional CRDT editor for live collaboration remains possible
later as an additive feature on top, rather than as the storage model.

---

## 5. Which embedding index, and when does a model change trigger reindexing?

**Decided: brute-force exact cosine over vectors keyed by embedding space. A
model or version change invalidates by construction.**

A vector is stored under `model@version:dimensions`
(`EmbeddingModel::space_key`). Two spaces can coexist during a migration and are
never compared with each other, so changing models cannot silently degrade
results — the worst case is a cache miss. `chunks::stale_chunks` finds passages
whose vector was computed from a different content hash, which is also what
makes incremental embedding work (SRCH-013).

Against an approximate index (HNSW, IVF) for now: at 100,000 chunks a cosine
pass over 768-dimensional vectors is a few tens of milliseconds, with no index
to build, tune, persist or keep consistent with the documents. An ANN index is
worth its complexity an order of magnitude further out.

Overturned if: measured vault sizes make the scan exceed the interaction
budget. `chunks::embeddings_in_space` is the only read path, so an index slots
in behind it.

---

## 6. What should be synced versus computed locally?

**Decided: replicate authoritative metadata; recompute everything derived.**

`EntityKind` enumerates exactly what replicates — vaults, documents, aliases,
tags, assignments, rejections, relationships, revisions, generated-note
provenance, policy. The search index, passages and vectors are not in that list
and never will be: `migrations::DERIVED_TABLES` is the same list seen from the
other side.

Three reasons. Derived data is the bulk of the bytes and the cheapest thing to
recompute. Devices legitimately differ in what they can compute — a phone may
have no local embedding model — so a vector computed on a laptop may be
unreproducible and should not be treated as authoritative. And integrity rule 3
already requires rebuildability, so the sync protocol gets it for free.

The consequence worth stating: a newly synced device has full metadata
immediately and must rebuild its own index before search works. That is the
right trade — the alternative is replicating gigabytes of regenerable data.

---

## 7. Graph layout for large interactive native views?

**Partly decided: the data layer is built for progressive rendering; the layout
algorithm is the renderer's choice.**

What is settled is the part that constrains it. Shared-tag kinship is computed
on demand and limited, never materialised as edges, so a popular tag cannot
produce millions of them. `relationships::in_vaults` takes kind filters and a
limit, so a view can ask for a bounded subgraph rather than everything.
`core::neighbourhood` is the local view, which is what a reader uses most.

The expectation is a force-directed layout over a filtered neighbourhood, with
clustering for the global view (GRAPH-014) — but that belongs with the GPUI
renderer, where frame budget and interaction are measurable.

---

## 8. Cross-vault relationships when vaults use different sync backends?

**Open, with the hard part already solved.**

An edge stores both endpoints as `GlobalDocumentId` and carries **no foreign
key**, so a cross-vault edge survives its far vault being unmounted, detached
or deregistered (SYNC-012, AC-009 — tested). The journal records which backend
each operation is destined for, so an edge's history is attributable.

What remains open is the policy question: when vault A syncs via CloudKit and
vault B via a self-hosted service, which replica owns the edge? The current
behaviour — journal it against the source endpoint's vault, let the other side
see it as an inbound edge it did not author — is a defensible default and may
simply be the answer. It needs a real two-backend deployment to confirm.

---

## Decisions the specification did not ask about

A few choices were forced during implementation and are worth recording.

**Reconciliation has two speeds, and the quick one has a documented blind
spot.** Trusting matching size *and* mtime is what makes NFR-003's two-second
vault open reachable. The cost is that an edit preserving both to the second is
invisible — the same trade-off `git status` makes. `Thoroughness::Full` and
`Rebuild` are both reachable from the CLI, so nothing is unrecoverable, and the
blind spot is asserted in both directions by a test so it cannot widen quietly.

**Frontmatter is edited by byte surgery, never reserialised.** A YAML
round-trip reorders keys, drops comments and requotes strings. For a tool whose
users keep their vaults in git, that is not a cosmetic issue — it is noise in
every diff. MD-008's "do not discard unknown fields" is implemented as "do not
disturb the bytes".

**Invalid frontmatter is recovered, not discarded.** A pasted email header
block with an unquoted colon — `title: Re: harbour survey` — is invalid YAML, and
a strict parser throws away every other key with it. `Frontmatter::recover`
reads what it can line by line and reports that it did so. IM-004 asks for
imported metadata to be preserved where possible; this is where "where
possible" does real work.

**Each metadata writer owns only its own rows.** Re-reading a file must not
delete a model's proposals, and re-running a classifier must not delete the
inline `#tags` in a body. That needs two replacement functions scoped by
provenance rather than one, and the asymmetry is load-bearing — a test for each
direction caught it when the scoping was briefly wrong.

**An unknown `field:value` in a query is an error, not a literal search.** A
mistyped `tag:` would otherwise return nothing with no explanation. Quoting is
the escape hatch for searching such text literally, and `http://example.com` is
excluded from the rule by its `//`.
