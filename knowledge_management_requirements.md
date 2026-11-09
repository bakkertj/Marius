# Personal Knowledge Management System — Software Requirements Specification

**Version:** 1.0  
**Status:** Initial baseline for implementation  
**Date:** 2026-11-08  
**License:** Apache License 2.0 (planned)  
**Platforms:** Windows, macOS, Linux, iOS  
**Primary implementation:** Rust; GPUI for desktop UI

## 1. Purpose and product vision

Build a local-first, open-source personal knowledge management (PKM) application that can replace Obsidian for Markdown-centric workflows while adding automatic tagging, persistent semantic relationships, email capture, cross-vault discovery, and retrieval using local and remote large language models (LLMs).

**Guiding principles**

1. **User ownership and portability:** User-authored source documents remain ordinary, editable Markdown files. The application must never lock essential knowledge inside a proprietary format.
2. **Original content protection:** AI may analyze and organize original documents, but must not autonomously modify their source content. AI can create separately identified derived notes and summaries.
3. **Local-first operation:** Editing, browsing, tagging, keyword search, and previously indexed retrieval work without network access.
4. **Intelligent retrieval:** Users can find knowledge by keywords, tags, semantics, backlinks, or graph traversal, including across multiple vaults.
5. **Provider independence:** OpenRouter, direct API providers, and locally hosted models are supported through interchangeable interfaces.
6. **Open extensibility:** An independently developed compatibility layer targets manually installed Obsidian plugins, first on desktop.
7. **Native experience:** Desktop UI uses GPUI rather than Electron; core application logic is portable Rust.

### 1.1 Requirement priorities

- **P0 — Essential:** Minimum viable, reliable core product.
- **P1 — Required for the full product:** Important capabilities following the foundation.
- **P2 — Advanced:** Later enhancements and optional integrations.

The priorities describe implementation sequencing, **not** whether the confirmed product capability is in scope. Some major P1 features, such as iOS and synchronization, remain committed product requirements.

### 1.2 Terminology

- **Vault:** An independently organized collection of user documents and related metadata.
- **Source document:** User-authored or imported material, protected from autonomous AI edits.
- **Derived note:** A separate AI-created document, summary, or synthesis with source citations and provenance.
- **Knowledge graph:** Persistent graph of documents, tags, and typed relationships.
- **Relationship provenance:** A record of whether a link was explicit, manually added, tag-based, or inferred by AI.
- **Synchronization backend:** The selected authoritative method for replicating a vault across devices (e.g., CloudKit or a self-hosted service).

## 2. Confirmed architectural decisions

| ID | Decision |
|---|---|
| ARCH-001 | Use Rust for the shared application core. |
| ARCH-002 | Use GPUI for Windows, macOS, and Linux desktop interfaces; do not use Electron. |
| ARCH-003 | Support iOS; investigate GPUI feasibility early and retain a thin native-UI fallback if necessary. |
| ARCH-004 | License original project code under Apache License 2.0, respecting third-party licenses. |
| ARCH-005 | Keep user source documents as authoritative Markdown files where possible. |
| ARCH-006 | Use SQLite locally for metadata, relations, indexes, and sync state. |
| ARCH-007 | Support PostgreSQL behind a self-hosted application/sync service. |
| ARCH-008 | Support CloudKit synchronization for macOS and iOS, with local SQLite on each device. |
| ARCH-009 | Use one authoritative sync backend per vault at a time; support controlled backend migration. |
| ARCH-010 | Support multiple vaults, global search, and cross-vault links and inferred relationships. |
| ARCH-011 | Make persistent IDs independent of filenames, paths, and vault moves. |
| ARCH-012 | AI can automatically update knowledge metadata, but cannot autonomously change original source content. |
| ARCH-013 | AI-derived notes are first-class, searchable documents, visibly identified as generated. |
| ARCH-014 | Support manual installation of Obsidian plugins via an independent, clean-room compatibility layer; prioritize desktop. |
| ARCH-015 | Treat pasted/imported email as knowledge rather than building a mail client. |
| ARCH-016 | Route LLM requests through a privacy and sensitivity-aware policy engine. |

## 3. Functional requirements

### 3.1 Vaults and document management

| ID | Priority | Requirement |
|---|---|---|
| KB-001 | P0 | Create, open, rename, close, and remove vaults without deleting their underlying files unless explicitly requested. |
| KB-002 | P0 | Open an existing directory of Markdown files as a vault without requiring conversion. |
| KB-003 | P0 | Create, rename, edit, move, copy, delete, and restore documents and folders. |
| KB-004 | P0 | Preserve Markdown as portable UTF-8 files and maintain readable directory structures. |
| KB-005 | P0 | Detect external filesystem changes and reconcile them with the local index. |
| KB-006 | P0 | Assign a stable, globally unique document ID and vault ID; retain identity across renames and moves. |
| KB-007 | P0 | Maintain creation/modification timestamps, title, path, content hash, and document type. |
| KB-008 | P0 | Support multiple vaults and independent vault-level settings. |
| KB-009 | P1 | Support simultaneous browsing/searching of multiple vaults. |
| KB-010 | P1 | Support aliases and alternate document titles. |
| KB-011 | P1 | Maintain revision history and recover earlier revisions. |
| KB-012 | P1 | Support archiving without excluding content from searches unless filtered. |
| KB-013 | P1 | Allow indexing externally referenced files without copying them; handle missing references. |
| KB-014 | P1 | Preserve document IDs and user metadata through export, backup, and migration where feasible. |

### 3.2 Markdown authoring and rendering

| ID | Priority | Requirement |
|---|---|---|
| MD-001 | P0 | Create and edit Markdown using a native desktop editor. |
| MD-002 | P0 | Support CommonMark and GitHub Flavored Markdown including tables, checklists, fenced code, and links. |
| MD-003 | P0 | Support source editing, rendered reading, and live preview. |
| MD-004 | P0 | Offer autosave, undo/redo, and reliable crash recovery. |
| MD-005 | P0 | Support images, file attachments, and local relative paths. |
| MD-006 | P0 | Recognize `[[Wiki Links]]`, aliases, heading links, and embedded document syntax. |
| MD-007 | P0 | Preserve link integrity when documents are renamed or moved. |
| MD-008 | P0 | Support YAML frontmatter without discarding unknown fields. |
| MD-009 | P1 | Support syntax highlighting for code blocks. |
| MD-010 | P1 | Support LaTeX math rendering and Mermaid diagrams. |
| MD-011 | P1 | Support templates, daily notes, document outlines, split panes, and multiple tabs. |
| MD-012 | P1 | Support Obsidian-style callouts and common Markdown extensions. |
| MD-013 | P1 | Provide efficient table editing and keyboard shortcuts. |
| MD-014 | P2 | Offer a rich-text editing mode that round-trips safely to valid Markdown. |

### 3.3 Import and capture

| ID | Priority | Requirement |
|---|---|---|
| IM-001 | P0 | Import individual Markdown files and folders recursively, preserving relative structure. |
| IM-002 | P0 | Import plain text and pasted text. |
| IM-003 | P0 | Detect likely duplicate content and offer non-destructive resolution. |
| IM-004 | P0 | Preserve imported tags, metadata, links, and attachments where possible. |
| IM-005 | P0 | Automatically index newly imported content. |
| IM-006 | P0 | Paste an email from a conventional client into a knowledge document. |
| IM-007 | P1 | Import existing Obsidian vaults without requiring proprietary conversion. |
| IM-008 | P1 | Import PDF, HTML, and Word documents with extracted searchable text. |
| IM-009 | P1 | Import `.eml` messages, including headers and attachments. |
| IM-010 | P1 | Support drag-and-drop, clipboard capture, and watched folders. |
| IM-011 | P1 | Import `.mbox` archives and retain message/thread metadata when available. |
| IM-012 | P2 | Support OCR for scans and images, web clipping, EPUB, and iOS Share Sheet capture. |

**Email scope:** Email capture and retrieval are required. A full email client, IMAP/SMTP account synchronization, and sending/replying are **out of scope** for the initial product.

### 3.4 Tags and metadata

| ID | Priority | Requirement |
|---|---|---|
| TAG-001 | P0 | Manually add and remove multiple tags on a document. |
| TAG-002 | P0 | Show tags with every document and browse all documents assigned to a tag. |
| TAG-003 | P0 | Support AND, OR, and NOT tag-based search/filter logic. |
| TAG-004 | P0 | Support inline `#tags`, YAML frontmatter tags, and indexed metadata tags. |
| TAG-005 | P0 | Automatically suggest or apply tags based on content using configurable LLMs. |
| TAG-006 | P0 | Preserve manual tags; AI must not remove or overwrite them automatically. |
| TAG-007 | P0 | Store tag provenance: manual, AI-confirmed, or AI-generated. |
| TAG-008 | P0 | Prefer existing canonical tags and aliases over near-duplicate AI tags. |
| TAG-009 | P0 | Maintain AI-generated tags separately from original source files unless the user explicitly requests a writeback. |
| TAG-010 | P1 | Support hierarchical tags, aliases, global rename, merge, autocomplete, and bulk editing. |
| TAG-011 | P1 | Track confidence, model, timestamp, and user approval for AI tag assignments. |
| TAG-012 | P1 | Support user rejection/locking rules to prevent unwanted reclassification. |
| TAG-013 | P1 | Reevaluate tags after substantial changes and support batch reclassification. |
| TAG-014 | P1 | Support arbitrary typed metadata fields, including date, text, number, Boolean, and lists. |
| TAG-015 | P1 | Provide sortable/filterable table views and saved queries over metadata. |
| TAG-016 | P2 | Suggest tag consolidation and identify emerging topics or naming inconsistencies. |

### 3.5 AI providers, policy, and generated knowledge

| ID | Priority | Requirement |
|---|---|---|
| AI-001 | P0 | Support OpenRouter, configurable direct API endpoints, and local LLM endpoints. |
| AI-002 | P0 | Support Ollama and OpenAI-compatible local model servers; provide extensible adapters for other providers. |
| AI-003 | P0 | Allow independent model selection for tagging, embeddings, summaries, and question answering. |
| AI-004 | P0 | Keep core reading/editing/search usable when LLMs are unavailable. |
| AI-005 | P0 | Pass all inference requests through a privacy-aware LLM policy engine. |
| AI-006 | P0 | Choose eligible providers based on document sensitivity, task, connectivity, and user policy. |
| AI-007 | P0 | Default unknown/imported sensitivity conservatively; never automatically lower a privacy restriction. |
| AI-008 | P0 | Never send protected content to an ineligible remote provider, including on fallback. |
| AI-009 | P0 | AI may add/change automatically derived tags and relationships but must not autonomously edit source document content. |
| AI-010 | P0 | Allow automatic classification without user review, with undo/audit history. |
| AI-011 | P1 | Generate separate summaries and synthesis notes, with visible AI provenance and citations to sources. |
| AI-012 | P1 | Display generated notes alongside ordinary documents and allow users to filter them out. |
| AI-013 | P1 | Preserve user edits to derived notes; regeneration must create a new revision or separate note. |
| AI-014 | P1 | Allow users to promote AI-derived notes to user-maintained documents. |
| AI-015 | P1 | Track model/provider/task, inference timestamp, and configurable cost/usage data. |
| AI-016 | P1 | Support configurable prompts, provider fallback under policy constraints, and per-vault rules. |
| AI-017 | P1 | Treat all imported contents and plugin-supplied text as untrusted data, not executable instructions. |
| AI-018 | P2 | Provide user-configurable autonomous workflows that create derived notes, never rewrite source content. |

### 3.6 Search and retrieval

| ID | Priority | Requirement |
|---|---|---|
| SRCH-001 | P0 | Full-text search over titles and document contents. |
| SRCH-002 | P0 | Search by tags, path, vault, date, document type, and metadata. |
| SRCH-003 | P0 | Support exact phrases, Boolean operators, and match highlighting/snippets. |
| SRCH-004 | P0 | Search across all accessible vaults in one global query. |
| SRCH-005 | P0 | Show originating vault and document identity for every search result. |
| SRCH-006 | P1 | Support fuzzy search, saved searches, filtering, and ranking. |
| SRCH-007 | P1 | Generate local or policy-eligible remote embeddings for semantic search. |
| SRCH-008 | P1 | Index document passages/sections as well as whole documents. |
| SRCH-009 | P1 | Combine full-text ranking, tag matches, and vector similarity in hybrid search. |
| SRCH-010 | P1 | Support document-grounded conversational Q&A with clickable source citations. |
| SRCH-011 | P1 | Support follow-up questions, vault scope restrictions, and source-sensitive retrieval. |
| SRCH-012 | P1 | Clearly distinguish supported source claims from generated inference or uncertainty. |
| SRCH-013 | P1 | Incrementally update indexes and embeddings when documents change. |
| SRCH-014 | P2 | Support cross-document comparison, contradiction detection, and knowledge-gap suggestions. |

### 3.7 Knowledge graph

| ID | Priority | Requirement |
|---|---|---|
| GRAPH-001 | P0 | Present an interactive graph with zoom, pan, selection, and document opening. |
| GRAPH-002 | P0 | Show explicit Markdown links and backlinks. |
| GRAPH-003 | P0 | Show shared-tag relationships, including a tag-centric graph view. |
| GRAPH-004 | P0 | Filter by vault, tag, document, and relationship type. |
| GRAPH-005 | P0 | Provide local-neighborhood and global graph views. |
| GRAPH-006 | P1 | Show tag nodes and document–tag links (bipartite mode). |
| GRAPH-007 | P1 | Discover and **persist** semantic relationships even without explicit links or shared tags. |
| GRAPH-008 | P1 | Support cross-vault manual and inferred relationships. |
| GRAPH-009 | P1 | Give every relationship a stable ID, type, origin, and optional confidence score. |
| GRAPH-010 | P1 | Permit users to create, confirm, delete, and suppress individual relationships. |
| GRAPH-011 | P1 | Prevent automatically recreating relationships rejected by the user unless requested. |
| GRAPH-012 | P1 | Reevaluate inferred relationships when documents change without silently discarding confirmed links. |
| GRAPH-013 | P1 | Distinguish inferred, explicit, manual, and tag-based relationships visually. |
| GRAPH-014 | P1 | Scale large graphs using clustering, filtering, and progressive rendering. |
| GRAPH-015 | P1 | Respect vault privacy boundaries when presenting cross-vault nodes or relationship metadata. |
| GRAPH-016 | P2 | Provide timeline layouts, isolated-node views, bridge-node discovery, and graph export. |

**Graph interpretation:** A shared tag can produce a relationship in the visualization without storing every possible pairwise edge. Persistent inferred and user-confirmed relationships must be stored individually.

### 3.8 Synchronization and remote storage

| ID | Priority | Requirement |
|---|---|---|
| SYNC-001 | P0 | Operate entirely locally, without accounts or network access. |
| SYNC-002 | P1 | Keep an independent SQLite database on each device. |
| SYNC-003 | P1 | Use CloudKit to synchronize vault data among a user's macOS and iOS devices. |
| SYNC-004 | P1 | Provide a self-hosted sync service backed by PostgreSQL for supported desktop platforms and, where feasible, iOS. |
| SYNC-005 | P1 | Synchronize document changes, tags, relationships, generated notes, and required metadata. |
| SYNC-006 | P1 | Support offline changes followed by incremental synchronization. |
| SYNC-007 | P1 | Use one authoritative synchronization backend per vault at a time. |
| SYNC-008 | P1 | Support controlled migration between CloudKit and self-hosted backends. |
| SYNC-009 | P1 | Identify conflicts and never silently discard concurrent user edits. |
| SYNC-010 | P1 | Use stable IDs, operation IDs, revisions, and deletion tombstones for idempotent replication. |
| SYNC-011 | P1 | Provide sync status, retries, recovery, and progress/error reporting. |
| SYNC-012 | P1 | Ensure unavailable vaults do not destroy their cross-vault graph edges. |
| SYNC-013 | P1 | Authenticate remote clients and use TLS for remote communication. |
| SYNC-014 | P1 | Do not share a concurrently writable SQLite file through iCloud Drive as the sync mechanism. |
| SYNC-015 | P2 | Offer optional selective synchronization, remote attachment storage, and end-to-end encryption. |

**Storage distinction:** Markdown source files are authoritative for their content; the database is authoritative for metadata that exists only in the application (for example, suppressed AI relationships). Derived search indexes must be rebuildable.

### 3.9 Plugin system and Obsidian compatibility

| ID | Priority | Requirement |
|---|---|---|
| PLUG-001 | P1 | Support manual installation, enable/disable, and removal of plugin packages. |
| PLUG-002 | P1 | Provide an independent JavaScript runtime interface for compatible Obsidian plugins. |
| PLUG-003 | P1 | Reimplement documented vault/file, metadata, event, command, and lifecycle behavior without copying proprietary implementation code. |
| PLUG-004 | P1 | Implement a test suite for independently defined expected API behavior. |
| PLUG-005 | P1 | Support plugin file operations through the safe document/vault abstraction. |
| PLUG-006 | P1 | Provide a compatibility report listing available and unsupported APIs. |
| PLUG-007 | P1 | Require clear capabilities and permissions for file, network, secret, and vault access. |
| PLUG-008 | P1 | Prevent plugin failures from crashing or corrupting the application. |
| PLUG-009 | P1 | Prioritize desktop platform compatibility first. |
| PLUG-010 | P1 | Support the application's own documented plugin interface alongside the compatibility layer. |
| PLUG-011 | P2 | Add editor integration, Markdown processors, and custom view compatibility. |
| PLUG-012 | P2 | Investigate browser-DOM and Node-dependent plugins separately; do not introduce Electron. |
| PLUG-013 | P2 | Evaluate iOS plugin compatibility only after desktop foundations are stable. |

**Scope limitation:** “Obsidian plugin compatibility” is a target, not a guarantee that every existing plugin works unmodified. Some plugins depend on DOM, Node.js, Electron, undocumented APIs, or platform capabilities that may not be safely or practically emulated. Plugin authors' licenses must be respected.

### 3.10 User experience and platform support

| ID | Priority | Requirement |
|---|---|---|
| UI-001 | P0 | Provide a GPUI desktop application on macOS, Windows, and Linux. |
| UI-002 | P0 | Provide document explorer, editor, search, tag browser, and properties panel. |
| UI-003 | P0 | Provide efficient keyboard navigation, shortcuts, and quick document switching. |
| UI-004 | P0 | Support dark and light appearance. |
| UI-005 | P1 | Provide graph, AI assistant, related notes, and backlinks views. |
| UI-006 | P1 | Support tabs, split panes, command palette, and saved workspaces. |
| UI-007 | P1 | Provide a native-feeling iOS application with offline access and syncing. |
| UI-008 | P1 | Share domain logic in Rust across platforms, isolating platform-specific UI. |
| UI-009 | P1 | Make key interfaces accessible through keyboard and platform accessibility features. |
| UI-010 | P1 | Show provenance indicators on generated notes and inferred graph edges. |
| UI-011 | P2 | Support customizable layouts, additional windows, dashboards, and iOS Share Sheet capture. |

## 4. Data model requirements

The initial conceptual entities are:

- **Vault:** ID, name, root/source location, sync backend, policy settings.
- **Document:** Stable ID, vault ID, path, title, type, content hash, timestamps, source/derived status.
- **DocumentAlias:** Alternate human-readable names.
- **Tag:** Stable ID, canonical name, hierarchy, aliases.
- **DocumentTag:** Document, tag, provenance, confidence, approval/lock state.
- **Relationship:** Stable edge ID, source and target IDs, type, provenance, confidence, confirmation status.
- **RejectedRelationship:** Suppression record preventing undesired re-creation.
- **DocumentRevision:** Version metadata and recoverable change history.
- **DocumentEmbedding/Chunk:** Document/section reference, embedding model version, vector/index key.
- **GeneratedNoteProvenance:** Generating model, sources, timestamp, revision, prompt/task reference.
- **SyncOperation:** Operation ID, entity ID, device, revision, logical timestamp, backend, state.
- **Policy:** Vault/document sensitivity, eligible providers, processing permissions.
- **PluginInstallation:** Manifest, version, enabled state, permissions, compatibility diagnostics.

### 4.1 Data integrity rules

1. Renaming or moving a document must not invalidate its stable ID or graph links.
2. Cross-vault relationships must reference stable IDs rather than paths alone.
3. Indexes can be rebuilt from source and durable metadata.
4. Manual/confirmed tags and links must not be overwritten by automatic classification.
5. AI-generated notes must retain source references and provenance.
6. User edits to generated notes must never be silently overwritten by regeneration.
7. Deleted or suppressed relationships must not unexpectedly reappear during reindexing or sync.
8. Vault access controls and sensitivity restrictions apply to graph/search previews and AI retrieval.

## 5. Security, privacy, and trust

| ID | Priority | Requirement |
|---|---|---|
| SEC-001 | P0 | Store remote API keys and tokens using native credential storage, not plaintext application settings. |
| SEC-002 | P0 | Do not transmit document content to a remote AI provider unless allowed by explicit or configured policy. |
| SEC-003 | P0 | Treat imported email, documents, web content, and plugin outputs as untrusted input. |
| SEC-004 | P0 | Keep local-only operation available. |
| SEC-005 | P1 | Support vault-level sensitivity policies and per-document restrictions. |
| SEC-006 | P1 | Log model/provider use and consequential automatic metadata changes for user review. |
| SEC-007 | P1 | Encrypt transport to self-hosted services and authenticate clients. |
| SEC-008 | P1 | Provide backup, export, restore, and index rebuild. |
| SEC-009 | P1 | Prevent plugins from bypassing source-protection and AI privacy rules without appropriate explicit permission. |
| SEC-010 | P2 | Support optional encrypted-at-rest vault metadata and end-to-end encrypted synchronization. |

## 6. Nonfunctional targets

These targets are proposed and should be validated on reference hardware with representative data sets.

| ID | Priority | Target |
|---|---|---|
| NFR-001 | P0 | No loss of committed Markdown data after ordinary application crashes. |
| NFR-002 | P0 | UI editing remains responsive while importing, embedding, classifying, or syncing. |
| NFR-003 | P0 | Typical indexed vault opens in about 2 seconds on reference desktop hardware. |
| NFR-004 | P0 | Common indexed keyword searches complete in about 200 ms for a 10,000-document vault. |
| NFR-005 | P1 | Support at least 100,000 documents across vaults, with progressive graph rendering. |
| NFR-006 | P1 | Import/index/embedding jobs are resumable and provide progress and failure reports. |
| NFR-007 | P1 | Restore indexed functionality after a database index rebuild without loss of authoritative content/metadata. |
| NFR-008 | P1 | Support automated unit, integration, sync conflict, and compatibility regression tests. |

## 7. Recommended technical structure (nonbinding)

| Concern | Suggested implementation |
|---|---|
| Shared core | Rust crates |
| Desktop UI | GPUI |
| Mobile UI | GPUI feasibility prototype; thin native iOS host fallback |
| Local relational/search | SQLite and FTS5 |
| Remote database | PostgreSQL, optional pgvector |
| Local vectors | Pluggable vector index/SQLite extension |
| Markdown | comrak or pulldown-cmark |
| Code highlighting | Tree-sitter |
| Runtime/network | Tokio, reqwest |
| Self-hosted service | Axum |
| Model adapters | OpenRouter, OpenAI-compatible APIs, Ollama |
| Apple sync | CloudKit adapter |
| Plugin runtime | Embedded JavaScript engine behind compatibility layer |
| Graph renderer | Native GPUI-based visualization |

Suggested workspace:

```text
knowledge-app/
├── crates/
│   ├── core/
│   ├── model/
│   ├── markdown/
│   ├── storage/
│   ├── indexing/
│   ├── search/
│   ├── embeddings/
│   ├── classification/
│   ├── llm-policy/
│   ├── graph/
│   ├── import/
│   ├── sync/
│   ├── plugin-compat/
│   └── ui-gpui/
├── apps/
│   ├── desktop/
│   ├── ios/
│   └── sync-server/
└── tests/
```

## 8. Verification and acceptance scenarios

The following are end-to-end acceptance scenarios. Detailed test cases should be derived for each requirement ID.

| ID | Scenario | Acceptance condition |
|---|---|---|
| AC-001 | Open an existing directory containing Markdown files. | Documents render and edit in place; files remain standard Markdown. |
| AC-002 | Import an Obsidian vault. | Files, links, attachments, and supported frontmatter are preserved. |
| AC-003 | Rename a linked document. | Stable identity and internal relationships remain valid. |
| AC-004 | Manually add/remove a tag. | Tag changes are immediately reflected in browse, search, and graph views. |
| AC-005 | Run AI auto-tagging on an imported note. | Metadata tags appear with provenance; original Markdown body and frontmatter are unchanged without user-authorized writeback. |
| AC-006 | Reject an AI tag or inferred relationship. | The rejection persists across reindexing and synchronization. |
| AC-007 | Search for a topic with different wording than the documents use. | Relevant semantic results are returned, subject to indexed content and configured model. |
| AC-008 | Ask a question spanning multiple documents. | The response cites accessible source passages and distinguishes uncertain synthesis. |
| AC-009 | Create two vaults and link documents across them. | Cross-vault graph and global search work; removing a temporarily unavailable vault does not destroy link metadata. |
| AC-010 | Generate an AI summary note and then edit it. | Generated provenance remains visible; later regeneration does not overwrite the user's edits. |
| AC-011 | Paste an email into a vault. | Its text becomes searchable, taggable, and graph-connectable without email account integration. |
| AC-012 | Edit a synced document while offline on two devices. | On reconnection, the system detects a conflict and does not silently discard either edit. |
| AC-013 | Sync macOS and iOS via CloudKit. | Each device retains its own local storage; documents and required metadata converge after sync. |
| AC-014 | Migrate one vault from CloudKit to self-hosted sync. | Data, IDs, and relationships remain intact, with only one authoritative backend active. |
| AC-015 | Install a compatible Obsidian plugin manually. | Supported APIs operate through the independent runtime; unsupported calls are reported clearly. |
| AC-016 | Set a vault to local-only AI processing. | No content from that vault is transmitted to external LLM providers, including during fallback. |
| AC-017 | Disconnect all networks. | Source documents, manual tags, full-text search, existing graph data, and local model functions (if configured) remain available. |

## 9. Delivery phases

| Phase | Scope | Deliverable |
|---|---|---|
| 0 — Feasibility | GPUI/iOS investigation; Obsidian plugin runtime prototype; CloudKit design validation. | Risk-reduction prototypes and architecture decisions. |
| 1 — Native foundation | GPUI editor, file/vault handling, Markdown, SQLite, basic search. | Reliable offline Markdown knowledge manager. |
| 2 — Classification | Manual/AI tags, provider adapters, policy engine, metadata. | Searchable, automatically organized notes. |
| 3 — Knowledge graph | Backlinks, tags, persistent inferred and cross-vault links, native graph UI. | Connected multi-vault knowledge base. |
| 4 — AI retrieval | Embeddings, hybrid search, grounded Q&A, derived notes. | Semantic personal knowledge retrieval. |
| 5 — Synchronization | CloudKit, self-hosted PostgreSQL service, conflicts, migration. | Cross-device vault synchronization. |
| 6 — Plugin compatibility | Manual installer, JS runtime, core Obsidian-compatible APIs. | Desktop plugin compatibility foundation. |
| 7 — Mobile | iOS experience and offline/cloud sync. | Mobile knowledge access. |
| 8 — Expansion | Advanced plugins, additional importers, knowledge discovery. | Extended ecosystem. |

**Development note:** Phase numbers are implementation milestones, not strict dependencies. Security, durable IDs, sync-ready change tracking, and source-content protection must be designed in from the first phase.

## 10. Explicit exclusions and boundaries

- No Electron-based application shell.
- No requirement to build a full email client, send messages, or manage mailboxes.
- No autonomous AI rewriting of imported or user-authored source content.
- No reliance on a remote service for ordinary offline editing.
- No promise of universal Obsidian plugin compatibility; compatibility is progressive and testable.
- No direct exposure of PostgreSQL to untrusted clients; use an authenticated service API.
- No concurrently writable SQLite database synchronized as a single file through iCloud Drive.

## 11. Open engineering questions (not blocking product requirements)

1. Which iOS UI implementation will best satisfy native performance and maintainability if upstream GPUI does not support iOS adequately?
2. Which embedded JavaScript engine best balances performance, security, and compatibility with Obsidian plugins?
3. Which Markdown frontmatter convention should hold stable IDs without unnecessarily modifying existing source files? A database sidecar is an alternative.
4. What conflict-resolution strategy should be used for concurrent Markdown edits: revision-based manual merge, three-way merge, or an optional CRDT-based editor?
5. Which embedding index should be used on each platform and how should model/version changes trigger reindexing?
6. What vault data and indexes should be synced versus computed locally, especially when local and remote models differ?
7. Which graph layout algorithms are best suited for large, interactive native GPUI views?
8. How should cross-vault relationships behave when one vault uses CloudKit and another uses self-hosted synchronization?

---

**Document status:** This specification consolidates the user's confirmed functional scope and architectural choices. The requirement IDs provide the basis for a traceability matrix, implementation backlog, and automated acceptance tests.
