//! The main window: sidebar, search, editor, status bar.
//!
//! Requirements: UI-001, UI-002 (document explorer, editor, search, tag
//! browser, properties), UI-003 (keyboard navigation and quick switching),
//! UI-004 (dark and light), UI-010 (provenance indicators), MD-001, MD-004
//! (autosave on switching documents), SRCH-001/003 (live search with
//! snippets).
//!
//! The window talks to `marius-core` through the same service API the CLI
//! uses — nothing here opens the database directly — so everything the UI can
//! do is scriptable, and everything scripted shows up in the UI.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use gpui::{
    actions, div, prelude::*, px, uniform_list, App, Context, Entity, FocusHandle, Focusable,
    KeyBinding, SharedString, Window,
};
use marius_core::{ExternalReport, Marius, SearchOptions};
use marius_indexing::watch::VaultWatcher;
use marius_model::{DocumentId, Provenance, Vault, VaultPath};

use crate::editor::{Editor, EditorEvent};
use crate::graph_view::{GraphEvent, GraphView};
use crate::highlight::LinkResolver;
use crate::preview::{Preview, PreviewContext, PreviewEvent};
use crate::settings::SettingsView;
use crate::theme::{Appearance, Theme};
use crate::tree::{self, TreeRow};

actions!(
    workspace,
    [
        FocusSearch,
        ToggleAppearance,
        NewNote,
        ReadingPageDown,
        ReadingPageUp,
        OpenSelected,
        ReindexVault,
        ToggleReading,
        ToggleSplit,
        ToggleGraph,
        SuggestTags,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        OpenPalette,
        ToggleBacklinks,
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        OpenSettings,
        RenameNote
    ]
);

/// Workspace-level bindings; the application registers them with the
/// editor's.
pub fn key_bindings() -> Vec<KeyBinding> {
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let key = |keys: String| -> &'static str { Box::leak(keys.into_boxed_str()) };
    vec![
        // Quick switching and search are the same surface (UI-003): one box,
        // reachable from anywhere.
        KeyBinding::new(key(format!("{primary}-p")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-k")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-shift-f")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-shift-l")), ToggleAppearance, None),
        KeyBinding::new(key(format!("{primary}-n")), NewNote, None),
        KeyBinding::new(key(format!("{primary}-r")), ReindexVault, None),
        // Edit <-> rendered reading, the toggle every Markdown app shares.
        KeyBinding::new(key(format!("{primary}-e")), ToggleReading, None),
        // Side-by-side preview (MD-003).
        KeyBinding::new(key(format!("{primary}-shift-e")), ToggleSplit, None),
        // Ask the configured model for tags (TAG-005).
        KeyBinding::new(key(format!("{primary}-t")), SuggestTags, None),
        // Font zoom, the platform convention: ⌘+/⌘−/⌘0 on macOS,
        // Ctrl elsewhere. Plus is shift-equals on most layouts, so both
        // faces of that key are bound.
        KeyBinding::new(key(format!("{primary}-=")), ZoomIn, None),
        KeyBinding::new(key(format!("{primary}-+")), ZoomIn, None),
        KeyBinding::new(key(format!("{primary}--")), ZoomOut, None),
        KeyBinding::new(key(format!("{primary}-0")), ZoomReset, None),
        // The command palette (UI-006): every action, one keystroke away.
        KeyBinding::new(key(format!("{primary}-shift-p")), OpenPalette, None),
        // Backlinks and related notes for the open document (UI-005).
        KeyBinding::new(key(format!("{primary}-b")), ToggleBacklinks, None),
        // Settings (⌘, — the platform convention).
        KeyBinding::new(key(format!("{primary}-,")), OpenSettings, None),
        // Rename (and move) the open note; F2 is the convention
        // Windows fingers expect, and it is free on macOS too.
        KeyBinding::new("f2", RenameNote, None),
        // Context-free PageDown/PageUp as a belt-and-braces fallback:
        // when the editor is focused its own context shadows these, and
        // the workspace handler pages the reading view only in reading
        // mode. (Space and arrows stay strictly pane-scoped — see
        // preview::keybindings — because they are also typing keys.)
        KeyBinding::new("pagedown", ReadingPageDown, None),
        KeyBinding::new("pageup", ReadingPageUp, None),
        // Tabs (UI-006).
        KeyBinding::new(key(format!("{primary}-shift-t")), NewTab, None),
        KeyBinding::new(key(format!("{primary}-w")), CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        // The knowledge graph (GRAPH-001).
        KeyBinding::new(key(format!("{primary}-g")), ToggleGraph, None),
    ]
}

/// One open tab (UI-006): its own editor entity, so switching tabs
/// costs nothing and unsaved buffers survive the switch.
struct Tab {
    editor: Entity<Editor>,
    document: Option<DocumentId>,
    title: SharedString,
}

/// One palette entry: a label and what running it means.
#[derive(Clone)]
struct PaletteItem {
    label: SharedString,
    action: PaletteAction,
}

#[derive(Clone)]
enum PaletteAction {
    NewNote,
    NewTab,
    CloseTab,
    DailyNote,
    CaptureClipboard,
    RenameNote,
    NewFromTemplate(String),
    Reindex,
    ToggleReading,
    ToggleSplit,
    ToggleGraph,
    SuggestTags,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleAppearance,
    OpenSettings,
    Save,
    /// A command an enabled plugin registered (PLUG-010), run on a
    /// worker thread with its own connection.
    Plugin {
        plugin: String,
        command: String,
    },
}

/// One row in the result list.
#[derive(Clone)]
struct Row {
    id: DocumentId,
    title: SharedString,
    path: SharedString,
    snippet: Option<SharedString>,
}

/// Resolves `[[wiki links]]` against the open vault, caching per document
/// version so highlighting does not query SQLite on every frame.
struct VaultResolver {
    app: Rc<RefCell<Marius>>,
    vault: marius_model::VaultId,
    cache: RefCell<HashMap<String, bool>>,
}

impl LinkResolver for VaultResolver {
    fn resolves(&self, target: &str) -> bool {
        if let Some(&hit) = self.cache.borrow().get(target) {
            return hit;
        }
        let resolved = {
            let app = self.app.borrow();
            marius_storage::documents::resolve_link(app.database().conn(), self.vault, target, &[])
                .ok()
                .flatten()
                .is_some()
        };
        self.cache.borrow_mut().insert(target.to_owned(), resolved);
        resolved
    }
}

/// Wires the reading view to the vault: wiki resolution and attachment
/// paths, both relative to the open document (MD-005, MD-006).
struct VaultPreviewContext {
    app: Rc<RefCell<Marius>>,
    vault: marius_model::VaultId,
    vault_root: std::path::PathBuf,
    /// The open document's path, for resolving relative destinations.
    base: RefCell<Option<VaultPath>>,
}

impl PreviewContext for VaultPreviewContext {
    fn wiki_resolves(&self, target: &str) -> bool {
        let app = self.app.borrow();
        marius_storage::documents::resolve_relative(
            app.database().conn(),
            self.vault,
            self.base.borrow().as_ref(),
            target,
            &[],
        )
        .ok()
        .flatten()
        .is_some()
    }

    fn attachment_path(&self, destination: &str) -> Option<std::path::PathBuf> {
        // Try relative to the open document's folder, then the vault root —
        // the same order link resolution uses.
        if let Some(base) = self.base.borrow().as_ref() {
            if let Some(folder) = base.parent() {
                if let Ok(path) = VaultPath::new(format!("{folder}/{destination}")) {
                    let absolute = path.to_fs(&self.vault_root);
                    if absolute.exists() {
                        return Some(absolute);
                    }
                }
            }
        }
        let path = VaultPath::new(destination).ok()?;
        Some(path.to_fs(&self.vault_root))
    }
}

/// `Preview` wants a `Box<dyn PreviewContext>`, the workspace wants to keep
/// updating the base path: an `Rc` shared by both, with a forwarding shim.
struct ForwardingContext(Rc<VaultPreviewContext>);

impl PreviewContext for ForwardingContext {
    fn wiki_resolves(&self, target: &str) -> bool {
        self.0.wiki_resolves(target)
    }
    fn attachment_path(&self, destination: &str) -> Option<std::path::PathBuf> {
        self.0.attachment_path(destination)
    }
}

/// A document being dragged out of the explorer (KB-003 move).
#[derive(Clone)]
struct DraggedDocument {
    id: DocumentId,
    title: SharedString,
}

/// A whole folder being dragged (KB-003): all its documents move, links
/// rewritten.
#[derive(Clone)]
struct DraggedFolder {
    path: SharedString,
}

/// The chip that follows the pointer during a drag.
struct DragGhost {
    title: SharedString,
    theme: Theme,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(self.theme.elevated)
            .border_1()
            .border_color(self.theme.accent)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .child(self.title.clone())
    }
}

/// Which pane the main area shows (MD-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Edit,
    Reading,
    /// Editor and rendered preview side by side, the preview following
    /// every keystroke (MD-003's third mode).
    Split,
    /// Model configuration and vault policy (AI-016, SEC-005).
    Settings,
    /// The knowledge graph (GRAPH-001), local or global (GRAPH-005).
    Graph,
}

/// The root view.
pub struct Workspace {
    app: Rc<RefCell<Marius>>,
    vault: Vault,
    theme: Theme,

    search: Entity<Editor>,
    /// The ACTIVE tab's editor — a mirror of `tabs[active_tab].editor`,
    /// kept as its own field so the rest of the workspace reads one
    /// place.
    editor: Entity<Editor>,
    tabs: Vec<Tab>,
    active_tab: usize,
    preview: Entity<Preview>,
    preview_base: Rc<VaultPreviewContext>,
    graph: Entity<GraphView>,
    settings: Entity<SettingsView>,
    mode: ViewMode,

    rows: Vec<Row>,
    /// Explorer rows when no search is active (UI-002): the vault's real
    /// folder structure, built by `tree::rows`.
    tree: Vec<TreeRow>,
    collapsed: BTreeSet<String>,
    selected_row: usize,
    open_document: Option<DocumentId>,
    open_path: SharedString,
    open_title: SharedString,
    /// Last search text actually applied, to skip redundant queries.
    applied_query: String,
    status: SharedString,
    tag_counts: Vec<(SharedString, u64)>,
    /// The open document's tags with real provenance and review state,
    /// for the properties strip and its approve/reject loop (UI-002,
    /// UI-010, TAG-007). One query per open, not per frame.
    open_tags: Vec<(
        SharedString,
        Provenance,
        marius_model::ReviewState,
        marius_model::TagSource,
    )>,
    /// Provenance banner for a generated note (AI-011): who made it,
    /// from what, and whether a person has edited it since.
    open_provenance: Option<marius_model::GeneratedNoteProvenance>,
    /// A background run (⌘T suggestions, a palette plugin command) on
    /// its worker thread with its own connection — WAL serves both —
    /// because neither an LLM nor a plugin may freeze the interface.
    /// The payload is the finished status line.
    suggestion: Option<std::sync::mpsc::Receiver<std::result::Result<String, String>>>,
    /// A semantic/hybrid search on its worker thread (`?`/`??` + Enter):
    /// the query embedding is a model call, so it never runs live.
    semantic: Option<std::sync::mpsc::Receiver<std::result::Result<Vec<Row>, String>>>,
    /// Content font scale (⌘+/⌘−/⌘0). Applies to the editor, the
    /// preview and the split; the chrome stays put.
    zoom: f32,
    /// The command palette (UI-006): its input, what it offers, and the
    /// current filtered selection. `None` items = closed.
    palette_input: Entity<Editor>,
    palette_items: Option<Vec<PaletteItem>>,
    palette_selected: usize,
    /// The rename prompt (F2): a single input prefilled with the open
    /// note's path; Enter renames (and moves, when the path changes
    /// folder), Escape dismisses.
    rename_input: Entity<Editor>,
    rename_open: bool,
    /// The backlinks/related side panel (⌘B, UI-005): refreshed when
    /// the open document changes, not per frame.
    /// Template staged by the palette for the next ⌘N (MD-011).
    pending_template: Option<String>,
    backlinks_open: bool,
    neighbourhood: Option<marius_core::Neighbourhood>,
    /// Shared-tag kinship resolved to titles, for the panel.
    kin_titles: Vec<(DocumentId, SharedString, u64)>,
    /// Debounced filesystem watcher (IM-010); `None` when the platform
    /// refused one, in which case ⌘R stays the manual fallback.
    watcher: Option<VaultWatcher>,
    focus_handle: FocusHandle,
}

impl Workspace {
    pub fn new(
        app: Rc<RefCell<Marius>>,
        vault: Vault,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = Theme::new(appearance);
        let resolver = Box::new(VaultResolver {
            app: app.clone(),
            vault: vault.id,
            cache: RefCell::new(HashMap::new()),
        });
        let editor = cx.new(|cx| Editor::new(cx, theme, resolver));
        let search = cx.new(|cx| {
            Editor::single_line(
                cx,
                theme,
                format!("Search… ({PRIMARY}K) · ?semantic · ??hybrid"),
            )
        });
        let palette_input = cx.new(|cx| Editor::single_line(cx, theme, "Type a command…"));
        let rename_input =
            cx.new(|cx| Editor::single_line(cx, theme, "new/path/and name.md — Enter renames"));
        let preview_base = Rc::new(VaultPreviewContext {
            app: app.clone(),
            vault: vault.id,
            vault_root: vault.root.clone(),
            base: RefCell::new(None),
        });
        let preview =
            cx.new(|cx| Preview::new(theme, Box::new(ForwardingContext(preview_base.clone())), cx));
        cx.subscribe_in(&preview, window, Self::on_preview_event)
            .detach();
        let graph = cx.new(|_| GraphView::new(app.clone(), vault.id, theme));
        let settings = cx.new(|cx| SettingsView::new(app.clone(), vault.id, theme, cx));
        cx.subscribe_in(&graph, window, Self::on_graph_event)
            .detach();

        cx.subscribe_in(&search, window, Self::on_search_event)
            .detach();
        cx.subscribe_in(&palette_input, window, Self::on_palette_event)
            .detach();
        cx.subscribe_in(&rename_input, window, Self::on_rename_event)
            .detach();
        cx.subscribe_in(&editor, window, Self::on_editor_event)
            .detach();
        window.focus(&search.focus_handle(cx));

        // Follow edits other programs make (IM-010). Polled rather than
        // event-driven into the UI thread: the watcher thread parks events
        // in a channel, and a timer drains it — no cross-thread wakeups to
        // reason about, and 600ms is far below anyone's reindex reflex.
        let watcher = VaultWatcher::new(&vault.root, std::time::Duration::from_millis(400))
            .map_err(|error| eprintln!("watch: {error} — falling back to manual reindex (⌘R)"))
            .ok();
        // The timer also collects ⌘T suggestion results, so it runs even
        // when the platform refused a watcher.
        {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(600))
                        .await;
                    let Ok(()) = this.update(cx, |ws: &mut Workspace, cx| ws.poll_watcher(cx))
                    else {
                        break; // the workspace is gone; stop polling
                    };
                }
            })
            .detach();
        }

        let mut this = Self {
            app,
            vault,
            theme,
            search,
            editor: editor.clone(),
            tabs: vec![Tab {
                editor,
                document: None,
                title: "untitled".into(),
            }],
            active_tab: 0,
            preview,
            preview_base,
            graph,
            settings,
            mode: ViewMode::Edit,
            rows: Vec::new(),
            tree: Vec::new(),
            collapsed: BTreeSet::new(),
            selected_row: 0,
            open_document: None,
            open_path: "no document".into(),
            open_title: "Marius".into(),
            applied_query: String::from("\u{0}never"),
            status: "ready".into(),
            tag_counts: Vec::new(),
            open_tags: Vec::new(),
            open_provenance: None,
            suggestion: None,
            semantic: None,
            zoom: 1.0,
            palette_input,
            palette_items: None,
            palette_selected: 0,
            rename_input,
            rename_open: false,
            pending_template: None,
            backlinks_open: false,
            neighbourhood: None,
            kin_titles: Vec::new(),
            watcher,
            focus_handle: cx.focus_handle(),
        };
        this.refresh_results(cx);
        this.refresh_tags();

        // Resume the saved workspace (UI-006): the tabs, the active one
        // and the zoom from last time, when they belong to this vault.
        let saved = this.app.borrow().config().ui_state.clone();
        let restored = match saved {
            Some(state) if state.vault == this.vault.name && !state.tabs.is_empty() => {
                this.zoom = (state.zoom_percent as f32 / 100.0).clamp(0.6, 2.4);
                let zoom = this.zoom;
                this.preview
                    .update(cx, |preview, cx| preview.set_zoom(zoom, cx));
                let mut opened = 0usize;
                for (index, path) in state.tabs.iter().enumerate() {
                    let id = VaultPath::new(path.clone()).ok().and_then(|path| {
                        this.app
                            .borrow()
                            .document_at(this.vault.id, &path)
                            .ok()
                            .flatten()
                            .map(|d| d.id)
                    });
                    let Some(id) = id else { continue };
                    if opened > 0 {
                        this.new_tab(window, cx);
                    }
                    this.open(id, cx);
                    opened += 1;
                    let _ = index;
                }
                if opened > 0 {
                    let active = state.active_tab.min(this.tabs.len() - 1);
                    this.activate_tab(active, window, cx);
                }
                opened > 0
            }
            _ => false,
        };
        if !restored {
            if let Some(first) = this.rows.first().map(|r| r.id) {
                this.open(first, cx);
            }
        }
        this
    }

    /// Complete a drag: move the document, which also rewrites the links
    /// that point at it (MD-007) — the same path `marius move` takes, so the
    /// gesture and the command cannot disagree.
    fn drop_document(&mut self, id: DocumentId, folder: Option<VaultPath>, cx: &mut Context<Self>) {
        let result = self.app.borrow_mut().move_document(id, folder.as_ref());
        match result {
            Ok(report) if report.from == report.to => {
                self.status = "already there".into();
            }
            Ok(report) => {
                self.status = if report.links_rewritten > 0 {
                    format!(
                        "moved to {} · {} link(s) rewritten in {} file(s)",
                        report.to,
                        report.links_rewritten,
                        report.files_rewritten.len()
                    )
                    .into()
                } else {
                    format!("moved to {}", report.to).into()
                };
                if self.open_document == Some(id) {
                    self.open_path = report.to.to_string().into();
                }
                self.rebuild_tree(cx);
            }
            Err(error) => self.status = format!("move: {error}").into(),
        }
        cx.notify();
    }

    /// Complete a folder drag through the same service as `marius move`.
    fn drop_folder(&mut self, from: &str, new_parent: Option<VaultPath>, cx: &mut Context<Self>) {
        let Ok(from_path) = VaultPath::new(from) else {
            return;
        };
        let vault = self.vault.clone();
        let result = self
            .app
            .borrow_mut()
            .move_folder(&vault, &from_path, new_parent.as_ref());
        match result {
            Ok(report) if report.from == report.to => {
                self.status = "already there".into();
            }
            Ok(report) => {
                self.status = format!(
                    "moved {} document(s) to {}/ · {} link(s) rewritten",
                    report.documents_moved, report.to, report.links_rewritten
                )
                .into();
                // The open document may have travelled with the folder.
                if let Some(id) = self.open_document {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        self.open_path = document.path.to_string().into();
                        *self.preview_base.base.borrow_mut() = Some(document.path);
                    }
                }
                self.rebuild_tree(cx);
            }
            Err(error) => self.status = format!("move: {error}").into(),
        }
        cx.notify();
    }

    fn browsing(&self) -> bool {
        self.applied_query.is_empty()
    }

    fn visible_len(&self) -> usize {
        if self.browsing() {
            self.tree.len()
        } else {
            self.rows.len()
        }
    }

    // ------------------------------------------------------------- queries

    /// Re-run the list: recency with no query, real search with one.
    fn refresh_results(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_owned();
        if query == self.applied_query {
            return;
        }
        self.applied_query = query.clone();

        let app = self.app.borrow();
        self.rows.clear();

        self.tree.clear();
        if query.is_empty() {
            // Browse mode: the explorer tree, not a flat list (UI-002).
            let filter = marius_storage::documents::Filter {
                vaults: vec![self.vault.id],
                exclude_archived: false,
                limit: Some(10_000),
                ..Default::default()
            };
            if let Ok(documents) = marius_storage::documents::list(app.database().conn(), &filter) {
                let triples: Vec<_> = documents
                    .into_iter()
                    .filter(|d| d.kind != marius_model::DocumentKind::Attachment)
                    .map(|d| (d.id, d.path.clone(), d.title.clone()))
                    .collect();
                self.tree = tree::rows(&triples, &self.collapsed);
            }
        } else if let Some(stripped) = query.strip_prefix('?') {
            // Semantic (`?`) and hybrid (`??`) run on Enter, not per
            // keystroke: the query embedding is a model call (SRCH-007).
            let mode = if stripped.starts_with('?') {
                "hybrid"
            } else {
                "semantic"
            };
            self.status = format!("{mode} search: press Enter to run").into();
        } else {
            match app.search(
                &query,
                SearchOptions {
                    limit: 100,
                    ..Default::default()
                },
            ) {
                Ok(results) => {
                    for hit in results.hits {
                        self.rows.push(Row {
                            id: hit.document.id,
                            title: hit.document.title.clone().into(),
                            path: hit.document.path.to_string().into(),
                            snippet: hit.snippet.map(|s| s.replace('\n', " ").into()),
                        });
                    }
                    self.status = format!("{} match(es)", self.rows.len()).into();
                }
                Err(error) => {
                    // A half-typed query is often momentarily invalid; show
                    // why instead of showing nothing silently.
                    self.status = format!("search: {error}").into();
                }
            }
        }
        self.selected_row = 0;
        cx.notify();
    }

    fn refresh_open_tags(&mut self) {
        self.open_tags.clear();
        self.open_provenance = None;
        let Some(id) = self.open_document else { return };
        let app = self.app.borrow();
        if let Ok(tags) = app.document_tags(id) {
            for (tag, assignment) in tags {
                self.open_tags.push((
                    tag.canonical_name.to_string().into(),
                    assignment.provenance,
                    assignment.review,
                    assignment.source,
                ));
            }
        }
        self.open_provenance = app.note_provenance(id).ok().flatten();
        drop(app);
        self.refresh_neighbourhood();
    }

    /// The open document's connections, for the ⌘B panel (UI-005).
    fn refresh_neighbourhood(&mut self) {
        self.neighbourhood = None;
        self.kin_titles.clear();
        if !self.backlinks_open {
            return;
        }
        let Some(id) = self.open_document else { return };
        let app = self.app.borrow();
        let Ok(neighbourhood) = app.neighbourhood(id) else {
            return;
        };
        for kin in &neighbourhood.shared_tags {
            if let Ok(document) = app.document(kin.document.document) {
                self.kin_titles
                    .push((document.id, document.title.clone().into(), kin.shared_tags));
            }
        }
        self.neighbourhood = Some(neighbourhood);
    }

    /// Rebuild the explorer rows after a collapse toggle, without touching
    /// the applied query.
    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        self.applied_query = String::from("\u{0}never");
        self.refresh_results(cx);
    }

    fn refresh_tags(&mut self) {
        let app = self.app.borrow();
        self.tag_counts = app
            .tag_counts(Some(self.vault.id))
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(tag, count)| (SharedString::from(tag.canonical_name.to_string()), count))
            .take(14)
            .collect();
    }

    // -------------------------------------------------------------- opening

    /// Open a document in the editor, autosaving the previous one (MD-004).
    /// Drain the filesystem watcher and fold external edits in (IM-010).
    fn poll_watcher(&mut self, cx: &mut Context<Self>) {
        self.poll_suggestion(cx);
        self.poll_semantic(cx);
        let Some(changes) = self.watcher.as_ref().and_then(|w| w.try_changes()) else {
            return;
        };
        let vault = self.vault.clone();
        let report = self
            .app
            .borrow_mut()
            .apply_external_changes(&vault, &changes);
        match report {
            Ok(report) => {
                self.status = match report {
                    ExternalReport::Indexed(n) => format!("synced {n} external change(s)").into(),
                    ExternalReport::Reindexed(r) => format!(
                        "reconciled external changes: {} added, {} modified, {} moved, {} removed",
                        r.added, r.modified, r.moved, r.removed
                    )
                    .into(),
                };
            }
            Err(error) => {
                self.status = format!("watch: {error}").into();
                cx.notify();
                return;
            }
        }

        // The picture changed under us: refresh what is derived from it.
        self.rebuild_tree(cx);
        self.refresh_tags();
        // Force the result list to re-query whatever it currently shows.
        self.applied_query = String::from("\u{0}refresh");
        self.refresh_results(cx);

        // Reload the open document only when the buffer holds nothing of
        // the user's: their unsaved edits always win over the disk.
        if let Some(id) = self.open_document {
            let open_path_changed = changes
                .paths
                .iter()
                .any(|p| p.as_str() == self.open_path.as_ref())
                || changes.rescan;
            if open_path_changed && !self.editor.read(cx).is_dirty() {
                let text = {
                    let app = self.app.borrow();
                    app.document(id).and_then(|d| app.read(&d))
                };
                if let Ok(text) = text {
                    self.editor
                        .update(cx, |editor, cx| editor.set_text(&text, cx));
                    if matches!(self.mode, ViewMode::Reading | ViewMode::Split) {
                        let source = self.editor.read(cx).text().to_owned();
                        self.preview
                            .update(cx, |preview, cx| preview.set_source(&source, cx));
                    }
                }
                self.refresh_open_tags();
            }
        }
        cx.notify();
    }

    /// `?query` / `??query` + Enter: semantic or hybrid retrieval on a
    /// worker thread, results delivered by the poll timer (SRCH-007/009).
    fn run_semantic(&mut self, stripped: String, cx: &mut Context<Self>) {
        if self.semantic.is_some() {
            self.status = "a semantic search is already running".into();
            cx.notify();
            return;
        }
        let (hybrid, query) = match stripped.strip_prefix('?') {
            Some(rest) => (true, rest.trim().to_owned()),
            None => (false, stripped.trim().to_owned()),
        };
        if query.is_empty() {
            return;
        }
        let data_dir = self.app.borrow().data_dir().to_path_buf();
        let vault = self.vault.id;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> std::result::Result<Vec<Row>, String> {
                let mut app = Marius::open(&data_dir).map_err(|e| e.to_string())?;
                let results = if hybrid {
                    app.hybrid_search(&query, vault, 50)
                } else {
                    app.semantic_search(&query, vault, 50)
                }
                .map_err(|e| e.to_string())?;
                if let Some(refusal) = results.refusal {
                    return Err(refusal);
                }
                Ok(results
                    .hits
                    .into_iter()
                    .map(|hit| {
                        let snippet: String = hit
                            .chunk
                            .text
                            .chars()
                            .take(160)
                            .collect::<String>()
                            .replace('\n', " ");
                        Row {
                            id: hit.document.id,
                            title: hit.document.title.clone().into(),
                            path: hit.document.path.to_string().into(),
                            snippet: (!snippet.is_empty()).then(|| snippet.into()),
                        }
                    })
                    .collect())
            })();
            let _ = sender.send(result);
        });
        self.semantic = Some(receiver);
        self.status = if hybrid {
            "hybrid search running…".into()
        } else {
            "semantic search running…".into()
        };
        cx.notify();
    }

    fn poll_semantic(&mut self, cx: &mut Context<Self>) {
        let Some(receiver) = &self.semantic else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(rows)) => {
                self.status = format!("{} semantic match(es)", rows.len()).into();
                self.rows = rows;
                self.selected_row = 0;
                self.semantic = None;
                cx.notify();
            }
            Ok(Err(reason)) => {
                self.status = format!("semantic search: {reason}").into();
                self.semantic = None;
                cx.notify();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.semantic = None;
            }
        }
    }

    /// Collect a finished ⌘T run, if one is cooking.
    fn poll_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(receiver) = &self.suggestion else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(line)) => {
                self.status = line.into();
                self.suggestion = None;
                self.refresh_open_tags();
                self.refresh_tags();
                self.rebuild_tree(cx);
                cx.notify();
            }
            Ok(Err(reason)) => {
                self.status = reason.into();
                self.suggestion = None;
                cx.notify();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "the suggestion worker died".into();
                self.suggestion = None;
                cx.notify();
            }
        }
    }

    /// Scroll the reading view by a viewport fraction.
    fn reading_scroll(&mut self, pages: f32, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.scroll_pages(pages, cx));
    }

    fn open(&mut self, id: DocumentId, cx: &mut Context<Self>) {
        if self.open_document == Some(id) {
            // Say so instead of doing nothing: a silent early return made
            // a real bug (a click that appeared to be ignored) impossible
            // to tell apart from a click that never landed.
            self.status = format!("already showing {}", self.open_path).into();
            cx.notify();
            return;
        }
        self.autosave(cx);
        self.preview
            .update(cx, |preview, cx| preview.scroll_to_top(cx));

        let text = {
            let app = self.app.borrow();
            app.document(id).and_then(|d| app.read(&d))
        };
        match text {
            Ok(text) => {
                self.editor
                    .update(cx, |editor, cx| editor.set_text(&text, cx));
                self.open_document = Some(id);
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.document = Some(id);
                }
                {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        self.open_path = document.path.to_string().into();
                        self.open_title = document.title.clone().into();
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.title = document.title.clone().into();
                        }
                    }
                }
                if let Some(row) = self.rows.iter().position(|r| r.id == id) {
                    self.selected_row = row;
                }
                self.refresh_open_tags();
                {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        *self.preview_base.base.borrow_mut() = Some(document.path.clone());
                    }
                }
                if matches!(self.mode, ViewMode::Reading | ViewMode::Split) {
                    let text = self.editor.read(cx).text().to_owned();
                    self.preview
                        .update(cx, |preview, cx| preview.set_source(&text, cx));
                }
                self.persist_ui_state();
            }
            Err(error) => self.status = format!("open: {error}").into(),
        }
        cx.notify();
    }

    /// Save if dirty. Called on explicit save, on switch, and on blur.
    fn autosave(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_document else { return };
        if !self.editor.read(cx).is_dirty() {
            return;
        }
        let text = self.editor.read(cx).text().to_owned();
        let result = self.app.borrow_mut().save_document(id, &text);
        match result {
            Ok(_) => {
                self.editor.update(cx, |editor, cx| editor.mark_saved(cx));
                self.status = "saved".into();
                self.refresh_tags();
                self.refresh_open_tags();
                // Content changed, so a live query's results may have too.
                self.applied_query = String::from("\u{0}never");
                self.refresh_results(cx);
            }
            Err(error) => self.status = format!("save FAILED: {error}").into(),
        }
        cx.notify();
    }

    fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The search box doubles as the name field: type a title, press ⌘N.
        let typed = self.search.read(cx).text().trim().to_owned();
        // `research/ideas/First thought` creates the folders too — which is
        // also the only way a folder comes to exist: a Markdown vault has no
        // empty directories, so a folder is born with its first note
        // (KB-003). Each segment is sanitised separately; sanitising the
        // whole string would turn the separators into hyphens.
        let name = typed
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(marius_model::sanitize)
            .collect::<Vec<_>>()
            .join("/");
        let name = if name.is_empty() {
            format!("Untitled {}", self.visible_len() + 1)
        } else {
            name
        };
        let Ok(path) = VaultPath::new(format!("{name}.md")) else {
            self.status = format!("cannot create {name:?}").into();
            return;
        };
        // The heading is the note's name, not its address: a note filed under
        // `research/ideas/` is still titled by its leaf.
        let title = name.rsplit('/').next().unwrap_or(&name);
        let vault = self.vault.clone();
        let created = match self.pending_template.take() {
            // Palette-staged template (MD-011): its body, placeholders
            // expanded, instead of the bare heading.
            Some(template) => self
                .app
                .borrow_mut()
                .new_from_template(&vault, &template, &path),
            None => {
                let body = format!("# {title}\n\n");
                self.app.borrow_mut().create_document(&vault, &path, &body)
            }
        };
        match created {
            Ok(document) => {
                self.search.update(cx, |s, cx| s.set_text("", cx));
                self.applied_query = String::from("\u{0}never");
                self.refresh_results(cx);
                self.open(document.id, cx);
                // Creating a note means wanting to write in it.
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                self.status = format!("created {path}").into();
            }
            Err(error) => self.status = format!("create: {error}").into(),
        }
        cx.notify();
    }

    fn reindex(&mut self, cx: &mut Context<Self>) {
        self.autosave(cx);
        let vault = self.vault.clone();
        let report = self.app.borrow_mut().index(&vault);
        self.status = match report {
            Ok(report) => format!("indexed: {}", report.summary()).into(),
            Err(error) => format!("index: {error}").into(),
        };
        self.applied_query = String::from("\u{0}never");
        self.refresh_results(cx);
        self.refresh_tags();
        cx.notify();
    }

    // --------------------------------------------------------------- events

    fn on_search_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Edited => self.refresh_results(cx),
            // Enter opens the selected result *and hands it the keyboard* —
            // search, arrive, type; no mouse in between (UI-003).
            EditorEvent::Submitted => {
                let query = self.search.read(cx).text().trim().to_owned();
                if let Some(stripped) = query.strip_prefix('?') {
                    self.run_semantic(stripped.to_owned(), cx);
                    return;
                }
                if self.browsing() {
                    match self.tree.get(self.selected_row).cloned() {
                        Some(TreeRow::Document { id, .. }) => {
                            self.open(id, cx);
                            let handle = self.editor.read(cx).focus_handle(cx);
                            window.focus(&handle);
                        }
                        Some(TreeRow::Folder { path, .. }) => {
                            tree::toggle(&mut self.collapsed, &path);
                            self.rebuild_tree(cx);
                        }
                        None => {}
                    }
                } else if let Some(row) = self.rows.get(self.selected_row).map(|r| r.id) {
                    self.open(row, cx);
                    let handle = self.editor.read(cx).focus_handle(cx);
                    window.focus(&handle);
                }
            }
            EditorEvent::NavigateDown => {
                if self.selected_row + 1 < self.visible_len() {
                    self.selected_row += 1;
                    cx.notify();
                }
            }
            EditorEvent::NavigateUp => {
                self.selected_row = self.selected_row.saturating_sub(1);
                cx.notify();
            }
            EditorEvent::Cancelled => {
                // First escape clears the query; a second, with nothing left
                // to clear, returns to the document.
                if self.search.read(cx).text().is_empty() {
                    let handle = self.editor.read(cx).focus_handle(cx);
                    window.focus(&handle);
                } else {
                    self.search.update(cx, |s, cx| s.set_text("", cx));
                    self.refresh_results(cx);
                }
            }
            _ => {}
        }
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SaveRequested => self.autosave(cx),
            EditorEvent::Edited => {
                // The split preview follows every keystroke (MD-003);
                // parsing a note is cheap next to repainting it.
                if self.mode == ViewMode::Split {
                    let text = self.editor.read(cx).text().to_owned();
                    self.preview
                        .update(cx, |preview, cx| preview.set_source(&text, cx));
                }
                cx.notify(); // dirty dot, line:col
            }
            // Escape in the document goes back to the list (UI-003's loop:
            // ⌘K down, Escape up). The old query is selected, not kept as a
            // live cursor: typing should replace it, not append to it.
            EditorEvent::Cancelled => {
                let handle = self.search.read(cx).focus_handle(cx);
                window.focus(&handle);
                self.search
                    .update(cx, |search, cx| search.select_all_text(cx));
            }
            _ => {}
        }
    }

    fn on_toggle_reading(
        &mut self,
        _: &ToggleReading,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = match self.mode {
            ViewMode::Edit => {
                // Render from the buffer, not the disk: unsaved edits are
                // exactly what the writer wants to proof-read.
                let text = self.editor.read(cx).text().to_owned();
                self.preview
                    .update(cx, |preview, cx| preview.set_source(&text, cx));
                // The editor leaves the tree; keys must land somewhere
                // real or PageDown scrolls nothing.
                let handle = self.preview.read(cx).focus_handle();
                window.focus(&handle);
                ViewMode::Reading
            }
            ViewMode::Reading | ViewMode::Split | ViewMode::Graph | ViewMode::Settings => {
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Edit
            }
        };
        cx.notify();
    }

    /// Files dropped from the OS (IM-010): each through the importer,
    /// the first resulting note opened.
    fn import_dropped(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let vault = self.vault.clone();
        let mut imported = 0usize;
        let mut first: Option<DocumentId> = None;
        let mut failures: Vec<String> = Vec::new();
        for path in paths {
            match self.app.borrow_mut().import_file(&vault, &path, None) {
                Ok(report) => {
                    imported += report.imported.len();
                    if first.is_none() {
                        first = report.imported.first().map(|d| d.id);
                    }
                }
                Err(error) => failures.push(format!(
                    "{}: {error}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )),
            }
        }
        self.status = if failures.is_empty() {
            format!("imported {imported} note(s)").into()
        } else {
            format!("imported {imported}; {}", failures.join("; ")).into()
        };
        self.rebuild_tree(cx);
        self.refresh_tags();
        if let Some(id) = first {
            self.open(id, cx);
        }
        cx.notify();
    }

    /// Palette: a new note from whatever text is on the clipboard
    /// (IM-002/IM-010's capture half).
    fn capture_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.status = "the clipboard holds no text".into();
            cx.notify();
            return;
        };
        // First line (or first words) names the note.
        let title: String = text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("Clipping")
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(60)
            .collect();
        let name = marius_model::sanitize(&format!("{title}.md"));
        let vault = self.vault.clone();
        let mut path = VaultPath::new(name.clone()).ok();
        let mut counter = 1usize;
        while let Some(candidate) = &path {
            let exists = self
                .app
                .borrow()
                .document_at(vault.id, candidate)
                .ok()
                .flatten()
                .is_some();
            if !exists {
                break;
            }
            counter += 1;
            let stem = name.trim_end_matches(".md");
            path = VaultPath::new(format!("{stem} (clipped {counter}).md")).ok();
        }
        let Some(path) = path else {
            self.status = "cannot name the clipping".into();
            cx.notify();
            return;
        };
        let body = if text.trim_start().starts_with('#') {
            text.clone()
        } else {
            format!("# {title}\n\n{text}\n")
        };
        let created = self.app.borrow_mut().create_document(&vault, &path, &body);
        match created {
            Ok(document) => {
                self.rebuild_tree(cx);
                self.open(document.id, cx);
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                self.status = format!("clipped to {}", document.path).into();
            }
            Err(error) => self.status = format!("clip: {error}").into(),
        }
        cx.notify();
    }

    /// F2: rename or move the open note, links rewritten either way
    /// (MD-007, AC-003). One input, prefilled with the current path:
    /// change the name to rename, change the folders to move, or both.
    fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_document.is_none() {
            self.status = "no document to rename".into();
            cx.notify();
            return;
        }
        let current = self.open_path.to_string();
        self.rename_open = true;
        self.rename_input
            .update(cx, |input, cx| input.set_text(&current, cx));
        window.focus(&self.rename_input.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn close_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_open = false;
        let handle = self.editor.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn apply_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.open_document else { return };
        let typed = self.rename_input.read(cx).text().trim().to_owned();
        self.close_rename(window, cx);
        if typed.is_empty() || typed == self.open_path.as_ref() {
            return;
        }

        // Folders first (a move), then the leaf (a rename); either half
        // alone is fine, and both rewrite inbound links.
        let (folder, leaf) = match typed.rsplit_once('/') {
            Some((parent, leaf)) => (Some(parent.to_owned()), leaf.to_owned()),
            None => (None, typed.clone()),
        };
        let current_folder = self
            .open_path
            .as_ref()
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_owned());
        let current_leaf = self
            .open_path
            .as_ref()
            .rsplit_once('/')
            .map(|(_, leaf)| leaf.to_owned())
            .unwrap_or_else(|| self.open_path.to_string());

        let mut links = 0usize;
        let mut outcome: Result<(), String> = Ok(());
        if folder != current_folder {
            let parent = match &folder {
                Some(parent) => match VaultPath::new(parent.clone()) {
                    Ok(path) => Some(path),
                    Err(error) => {
                        self.status = format!("rename: {error}").into();
                        cx.notify();
                        return;
                    }
                },
                None => None,
            };
            match self.app.borrow_mut().move_document(id, parent.as_ref()) {
                Ok(report) => links += report.links_rewritten,
                Err(error) => outcome = Err(error.to_string()),
            }
        }
        if outcome.is_ok() && leaf != current_leaf {
            match self.app.borrow_mut().rename_document(id, &leaf) {
                Ok(report) => links += report.links_rewritten,
                Err(error) => outcome = Err(error.to_string()),
            }
        }

        match outcome {
            Ok(()) => {
                // Refresh everything the path feeds.
                if let Ok(document) = self.app.borrow().document(id) {
                    self.open_path = document.path.to_string().into();
                    self.open_title = document.title.clone().into();
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        tab.title = document.title.clone().into();
                    }
                    *self.preview_base.base.borrow_mut() = Some(document.path);
                }
                self.status = if links > 0 {
                    format!("renamed · {links} inbound link(s) rewritten").into()
                } else {
                    "renamed".into()
                };
                self.rebuild_tree(cx);
                self.refresh_open_tags();
                self.persist_ui_state();
            }
            Err(error) => self.status = format!("rename: {error}").into(),
        }
        cx.notify();
    }

    fn on_rename_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Cancelled => self.close_rename(window, cx),
            EditorEvent::Submitted => self.apply_rename(window, cx),
            _ => {}
        }
    }

    /// ⌘⇧P: open the palette with everything runnable right now —
    /// built-in actions plus every enabled plugin's commands (UI-006).
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![
            ("New note", PaletteAction::NewNote),
            ("New tab", PaletteAction::NewTab),
            ("Close tab", PaletteAction::CloseTab),
            ("Open today's daily note", PaletteAction::DailyNote),
            ("New note from clipboard", PaletteAction::CaptureClipboard),
            ("Rename or move this note (F2)", PaletteAction::RenameNote),
            ("Save document", PaletteAction::Save),
            ("Suggest tags for this note", PaletteAction::SuggestTags),
            ("Toggle reading view", PaletteAction::ToggleReading),
            ("Toggle split preview", PaletteAction::ToggleSplit),
            ("Toggle knowledge graph", PaletteAction::ToggleGraph),
            ("Reindex vault", PaletteAction::Reindex),
            ("Zoom in", PaletteAction::ZoomIn),
            ("Zoom out", PaletteAction::ZoomOut),
            ("Zoom: reset", PaletteAction::ZoomReset),
            ("Toggle light/dark", PaletteAction::ToggleAppearance),
            ("Open settings", PaletteAction::OpenSettings),
        ]
        .into_iter()
        .map(|(label, action)| PaletteItem {
            label: label.into(),
            action,
        })
        .collect::<Vec<_>>();

        // One entry per template in the vault's templates folder.
        for template in self.app.borrow().templates(&self.vault) {
            items.push(PaletteItem {
                label: format!("New from template: {template}").into(),
                action: PaletteAction::NewFromTemplate(template),
            });
        }

        // Plugin commands: enabled plugins only, loaded without
        // capabilities — enumeration must not grant anything.
        let plugins = self.app.borrow().plugins().unwrap_or_default();
        for installation in plugins.into_iter().filter(|p| p.enabled) {
            let commands =
                marius_core::plugins::plugin_commands(self.app.clone(), &installation.manifest.id)
                    .unwrap_or_default();
            for command in commands {
                items.push(PaletteItem {
                    label: format!("{}: {command}", installation.manifest.name).into(),
                    action: PaletteAction::Plugin {
                        plugin: installation.manifest.id.clone(),
                        command,
                    },
                });
            }
        }

        self.palette_items = Some(items);
        self.palette_selected = 0;
        self.palette_input
            .update(cx, |input, cx| input.set_text("", cx));
        window.focus(&self.palette_input.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_items = None;
        let handle = self.editor.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn palette_matches(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let Some(items) = &self.palette_items else {
            return Vec::new();
        };
        let query = self.palette_input.read(cx).text().trim().to_owned();
        crate::palette::rank(&query, items, |item| item.label.as_ref())
            .into_iter()
            .map(|(_, item)| item.clone())
            .take(12)
            .collect()
    }

    fn on_palette_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Edited => {
                self.palette_selected = 0;
                cx.notify();
            }
            EditorEvent::Cancelled => self.close_palette(window, cx),
            EditorEvent::NavigateDown => {
                let count = self.palette_matches(cx).len();
                if count > 0 {
                    self.palette_selected = (self.palette_selected + 1).min(count - 1);
                }
                cx.notify();
            }
            EditorEvent::NavigateUp => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                cx.notify();
            }
            EditorEvent::Submitted => {
                let matches = self.palette_matches(cx);
                if let Some(item) = matches.get(self.palette_selected).cloned() {
                    self.close_palette(window, cx);
                    self.run_palette_action(item.action, window, cx);
                } else {
                    self.close_palette(window, cx);
                }
            }
            _ => {}
        }
    }

    fn run_palette_action(
        &mut self,
        action: PaletteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PaletteAction::NewNote => self.new_note(window, cx),
            PaletteAction::NewTab => self.new_tab(window, cx),
            PaletteAction::CloseTab => self.close_tab(self.active_tab, window, cx),
            PaletteAction::CaptureClipboard => self.capture_clipboard(window, cx),
            PaletteAction::RenameNote => self.open_rename(window, cx),
            PaletteAction::DailyNote => {
                let vault = self.vault.clone();
                let result = self.app.borrow_mut().daily_note(&vault);
                match result {
                    Ok(note) => {
                        self.rebuild_tree(cx);
                        self.open(note.id, cx);
                        let handle = self.editor.read(cx).focus_handle(cx);
                        window.focus(&handle);
                    }
                    Err(error) => self.status = format!("daily: {error}").into(),
                }
                cx.notify();
            }
            PaletteAction::NewFromTemplate(template) => {
                // The template seeds the note the ⌘N box then names:
                // reuse the search-box flow with the template body staged.
                self.pending_template = Some(template);
                self.new_note(window, cx);
            }
            PaletteAction::Save => self.autosave(cx),
            PaletteAction::SuggestTags => self.suggest_tags(cx),
            PaletteAction::ToggleReading => self.on_toggle_reading(&ToggleReading, window, cx),
            PaletteAction::ToggleSplit => self.on_toggle_split(&ToggleSplit, window, cx),
            PaletteAction::ToggleGraph => self.on_toggle_graph(&ToggleGraph, window, cx),
            PaletteAction::Reindex => self.reindex(cx),
            PaletteAction::ZoomIn => self.set_zoom(self.zoom + 0.1, cx),
            PaletteAction::ZoomOut => self.set_zoom(self.zoom - 0.1, cx),
            PaletteAction::ZoomReset => self.set_zoom(1.0, cx),
            PaletteAction::ToggleAppearance => {
                self.on_toggle_appearance(&ToggleAppearance, window, cx)
            }
            PaletteAction::OpenSettings => {
                self.mode = ViewMode::Settings;
                cx.notify();
            }
            PaletteAction::Plugin { plugin, command } => {
                // A plugin command may block on anything: its own thread,
                // its own connection; its log lands in the status line via
                // the poll timer, and file changes arrive via the watcher.
                let data_dir = self.app.borrow().data_dir().to_path_buf();
                let vault = self.vault.id;
                let (sender, receiver) = std::sync::mpsc::channel();
                let label = format!("{plugin}: {command}");
                std::thread::spawn(move || {
                    let result = (|| -> std::result::Result<String, String> {
                        let app = Marius::open(&data_dir).map_err(|e| e.to_string())?;
                        let shared = std::rc::Rc::new(std::cell::RefCell::new(app));
                        let (output, mut log) = marius_core::plugins::run_plugin_command(
                            shared, vault, &plugin, &command,
                        )
                        .map_err(|e| e.to_string())?;
                        if !output.is_empty() {
                            log.push(output);
                        }
                        Ok(if log.is_empty() {
                            format!("{command} finished")
                        } else {
                            log.join(" · ")
                        })
                    })();
                    let _ = sender.send(result);
                });
                self.suggestion = Some(receiver);
                self.status = format!("running {label}…").into();
                cx.notify();
            }
        }
    }

    /// A fresh tab with its own editor, activated immediately (UI-006).
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.theme;
        let resolver = Box::new(VaultResolver {
            app: self.app.clone(),
            vault: self.vault.id,
            cache: RefCell::new(HashMap::new()),
        });
        let editor = cx.new(|cx| Editor::new(cx, theme, resolver));
        cx.subscribe_in(&editor, window, Self::on_editor_event)
            .detach();
        self.tabs.push(Tab {
            editor,
            document: None,
            title: "untitled".into(),
        });
        let index = self.tabs.len() - 1;
        self.activate_tab(index, window, cx);
    }

    fn activate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        self.active_tab = index;
        self.editor = tab.editor.clone();
        self.open_document = tab.document;
        // Re-derive the active-document chrome from the tab.
        match tab.document {
            Some(id) => {
                let app = self.app.borrow();
                if let Ok(document) = app.document(id) {
                    self.open_path = document.path.to_string().into();
                    self.open_title = document.title.clone().into();
                    *self.preview_base.base.borrow_mut() = Some(document.path);
                }
            }
            None => {
                self.open_path = "no document".into();
                self.open_title = "Marius".into();
            }
        }
        self.refresh_open_tags();
        if matches!(self.mode, ViewMode::Reading | ViewMode::Split) {
            let text = self.editor.read(cx).text().to_owned();
            self.preview
                .update(cx, |preview, cx| preview.set_source(&text, cx));
        }
        let handle = self.editor.read(cx).focus_handle(cx);
        window.focus(&handle);
        self.persist_ui_state();
        cx.notify();
    }

    /// Close a tab, saving its unsaved work first; the last tab empties
    /// instead of closing — a workspace always has somewhere to type.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            if let Some(tab) = self.tabs.first() {
                let editor = tab.editor.clone();
                self.save_tab(&editor, self.tabs[0].document, cx);
                editor.update(cx, |editor, cx| editor.set_text("", cx));
            }
            self.tabs[0].document = None;
            self.tabs[0].title = "untitled".into();
            self.activate_tab(0, window, cx);
            return;
        }
        let tab = self.tabs.remove(index);
        self.save_tab(&tab.editor, tab.document, cx);
        let next = index.min(self.tabs.len() - 1);
        self.activate_tab(next, window, cx);
    }

    /// Autosave one tab's buffer, active or not.
    fn save_tab(
        &mut self,
        editor: &Entity<Editor>,
        document: Option<DocumentId>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = document else { return };
        if !editor.read(cx).is_dirty() {
            return;
        }
        let text = editor.read(cx).text().to_owned();
        if let Err(error) = self.app.borrow_mut().save_document(id, &text) {
            self.status = format!("save: {error}").into();
        } else {
            editor.update(cx, |editor, cx| editor.mark_saved(cx));
        }
    }

    /// Remember the open tabs, the active one and the zoom, per vault,
    /// so the next launch resumes where this one stood (UI-006's saved
    /// workspace).
    fn persist_ui_state(&self) {
        let mut app = self.app.borrow_mut();
        let mut config = app.config().clone();
        let paths: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.document)
            .filter_map(|id| app.document(id).ok())
            .map(|d| d.path.to_string())
            .collect();
        config.ui_state = Some(marius_core::config::UiState {
            vault: self.vault.name.clone(),
            tabs: paths,
            active_tab: self.active_tab,
            zoom_percent: (self.zoom * 100.0).round() as u32,
        });
        let _ = app.set_config(config);
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let zoom = (zoom * 10.0).round() / 10.0;
        self.zoom = zoom.clamp(0.6, 2.4);
        let zoom = self.zoom;
        self.preview
            .update(cx, |preview, cx| preview.set_zoom(zoom, cx));
        self.status = if (zoom - 1.0).abs() < f32::EPSILON {
            "zoom 100%".into()
        } else {
            format!("zoom {:.0}%", zoom * 100.0).into()
        };
        self.persist_ui_state();
        cx.notify();
    }

    /// ⌘T: tag suggestions for the open note, off the UI thread.
    ///
    /// The worker opens its own `Marius` on the same data directory —
    /// SQLite's WAL serves both connections — because a local model can
    /// take long seconds and the interface must not freeze for them.
    /// `poll_watcher`'s timer collects the result.
    fn suggest_tags(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_document else {
            self.status = "no document to tag".into();
            cx.notify();
            return;
        };
        if self.suggestion.is_some() {
            self.status = "a suggestion run is already underway".into();
            cx.notify();
            return;
        }
        self.autosave(cx);
        let data_dir = self.app.borrow().data_dir().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> std::result::Result<String, String> {
                let mut app = Marius::open(&data_dir).map_err(|e| e.to_string())?;
                let result = app.suggest_tags(id, true).map_err(|e| e.to_string())?;
                if result.suggestions.is_empty() {
                    return Err(result.reason);
                }
                let applied: Vec<&str> = result.applied.iter().map(|t| t.as_str()).collect();
                Ok(if applied.is_empty() {
                    "the model proposed nothing new".to_owned()
                } else {
                    format!("proposed: {} — review with ✓/✗ above", applied.join(", "))
                })
            })();
            let _ = sender.send(result);
        });
        self.suggestion = Some(receiver);
        self.status = "asking the model for tags…".into();
        cx.notify();
    }

    fn on_toggle_split(&mut self, _: &ToggleSplit, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            ViewMode::Split => {
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Edit
            }
            _ => {
                let text = self.editor.read(cx).text().to_owned();
                self.preview
                    .update(cx, |preview, cx| preview.set_source(&text, cx));
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Split
            }
        };
        cx.notify();
    }

    fn on_toggle_graph(&mut self, _: &ToggleGraph, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            ViewMode::Graph => {
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Edit
            }
            _ => {
                let focus = self.open_document;
                self.graph.update(cx, |graph, cx| graph.show(focus, cx));
                ViewMode::Graph
            }
        };
        cx.notify();
    }

    fn on_graph_event(
        &mut self,
        _: &Entity<GraphView>,
        event: &GraphEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GraphEvent::Open(id) => {
                // Clicking a node opens the document (GRAPH-001).
                self.mode = ViewMode::Edit;
                self.open(*id, cx);
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                cx.notify();
            }
        }
    }

    fn on_preview_event(
        &mut self,
        _: &Entity<Preview>,
        event: &PreviewEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match event {
            PreviewEvent::OpenWiki(target) | PreviewEvent::OpenRelative(target) => target.clone(),
        };
        let resolved = {
            let app = self.app.borrow();
            marius_storage::documents::resolve_relative(
                app.database().conn(),
                self.vault.id,
                self.preview_base.base.borrow().as_ref(),
                &target,
                &[],
            )
            .ok()
            .flatten()
        };
        match resolved {
            Some((found, _)) => {
                // An attachment (a PDF, an image) is not editable text:
                // hand it to the system viewer. Notes open in place.
                let attachment = {
                    let app = self.app.borrow();
                    marius_storage::documents::get(app.database().conn(), found.document)
                        .ok()
                        .flatten()
                        .filter(|d| d.kind == marius_model::DocumentKind::Attachment)
                };
                if let Some(doc) = attachment {
                    let file = doc.path.to_fs(&self.vault.root);
                    self.status = match open_with_system_viewer(&file) {
                        Ok(()) => format!("opened {} in the system viewer", doc.path).into(),
                        Err(e) => format!("cannot open {}: {e}", doc.path).into(),
                    };
                } else {
                    // Browsing stays in reading mode; that is what links are for.
                    self.open(found.document, cx);
                }
            }
            None => self.status = format!("{target:?} does not resolve").into(),
        }
    }

    fn on_toggle_appearance(
        &mut self,
        _: &ToggleAppearance,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.theme = Theme::new(self.theme.appearance.toggled());
        let theme = self.theme;
        self.editor
            .update(cx, |editor, cx| editor.set_theme(theme, cx));
        self.search
            .update(cx, |search, cx| search.set_theme(theme, cx));
        self.preview
            .update(cx, |preview, cx| preview.set_theme(theme, cx));
        self.graph.update(cx, |graph, _| graph.set_theme(theme));
        self.settings
            .update(cx, |settings, cx| settings.set_theme(theme, cx));
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    /// One row of the folder tree (UI-002). A folder shows a chevron and its
    /// recursive document count; clicking it folds or unfolds. Depth becomes
    /// indentation, which is the whole of what makes it read as a tree.
    fn render_tree_row(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let Some(row) = self.tree.get(ix).cloned() else {
            return div().into_any_element();
        };
        let selected = ix == self.selected_row;
        let indent = px(12. + row.depth() as f32 * 14.);
        match row {
            TreeRow::Folder {
                path,
                name,
                collapsed,
                documents,
                ..
            } => div()
                .id(ix)
                .pl(indent)
                .pr(px(12.))
                .py(px(4.))
                .cursor_pointer()
                .bg(if selected {
                    theme.selected
                } else {
                    theme.surface
                })
                .hover(|style| style.bg(theme.hover))
                .on_drag(
                    DraggedFolder {
                        path: path.clone().into(),
                    },
                    move |dragged, _position, _window, cx| {
                        let title: SharedString = format!("📁 {}", dragged.path).into();
                        cx.new(|_| DragGhost { title, theme })
                    },
                )
                .drag_over::<DraggedDocument>(move |style, _dragged, _window, _cx| {
                    style
                        .bg(theme.selected)
                        .border_1()
                        .border_color(theme.accent)
                })
                .drag_over::<DraggedFolder>(move |style, _dragged, _window, _cx| {
                    style
                        .bg(theme.selected)
                        .border_1()
                        .border_color(theme.accent)
                })
                .on_drop(cx.listener({
                    let target = path.clone();
                    move |this: &mut Self, dragged: &DraggedDocument, _window, cx| {
                        match VaultPath::new(&target) {
                            Ok(folder) => this.drop_document(dragged.id, Some(folder), cx),
                            Err(error) => {
                                this.status = format!("move: {error}").into();
                            }
                        }
                    }
                }))
                .on_drop(cx.listener({
                    let target = path.clone();
                    move |this: &mut Self, dragged: &DraggedFolder, _window, cx| {
                        match VaultPath::new(&target) {
                            Ok(parent) => this.drop_folder(dragged.path.as_ref(), Some(parent), cx),
                            Err(error) => {
                                this.status = format!("move: {error}").into();
                            }
                        }
                    }
                }))
                .flex()
                .flex_row()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(theme.muted)
                        .w(px(12.))
                        .child(if collapsed { "▸" } else { "▾" }),
                )
                .child(div().text_color(theme.text).child(name))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .child(format!("{documents}")),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this: &mut Self, _e, _w, cx| {
                        this.selected_row = ix;
                        tree::toggle(&mut this.collapsed, &path);
                        this.rebuild_tree(cx);
                    }),
                )
                .into_any_element(),
            TreeRow::Document { id, title, .. } => {
                let open = self.open_document == Some(id);
                let payload = DraggedDocument {
                    id,
                    title: title.clone().into(),
                };
                div()
                    .id(ix)
                    .pl(indent + px(18.))
                    .pr(px(12.))
                    .py(px(4.))
                    .cursor_pointer()
                    .bg(if selected || open {
                        theme.selected
                    } else {
                        theme.surface
                    })
                    .hover(|style| style.bg(theme.hover))
                    .text_color(theme.text)
                    .child(title)
                    .on_drag(payload, move |dragged, _position, _window, cx| {
                        let ghost = DragGhost {
                            title: dragged.title.clone(),
                            theme,
                        };
                        cx.new(|_| ghost)
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            this.selected_row = ix;
                            this.open(id, cx);
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    /// One flat search result, with its snippet (SRCH-003).
    fn render_search_row(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let is_selected = ix == self.selected_row || self.open_document == Some(row.id);
        let mut item = div()
            .id(ix)
            .px(px(12.))
            .py(px(6.))
            .cursor_pointer()
            .child(div().text_color(theme.text).child(row.title.clone()))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(row.path.clone()),
            );
        if let Some(snippet) = &row.snippet {
            item = item.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(snippet.clone()),
            );
        }
        let id = row.id;
        item.bg(if is_selected {
            theme.selected
        } else {
            theme.surface
        })
        .hover(|style| style.bg(theme.hover))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut Self, _e, _w, cx| {
                this.open(id, cx);
            }),
        )
        .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let rows = self.rows.clone();
        let searching = !self.applied_query.is_empty() && self.applied_query != "\u{0}never";

        let count = if searching {
            rows.len()
        } else {
            self.tree.len()
        };
        let list = uniform_list(
            "documents",
            count,
            cx.processor(
                move |this: &mut Self, range: std::ops::Range<usize>, _w, cx| {
                    range
                        .map(|ix| {
                            if this.applied_query.is_empty() {
                                this.render_tree_row(ix, cx)
                            } else {
                                this.render_search_row(ix, cx)
                            }
                        })
                        .collect()
                },
            ),
        )
        .flex_grow();

        let mut tag_chips = div().flex().flex_wrap().gap(px(4.)).p(px(8.));
        for (tag, count) in &self.tag_counts {
            let tag_for_click = tag.clone();
            tag_chips = tag_chips.child(
                div()
                    .id(SharedString::from(format!("tag-{tag}")))
                    .px(px(6.))
                    .py(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.tag_manual)
                    .bg(theme.elevated)
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .child(format!("#{tag} {count}"))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            let query = format!("tag:{tag_for_click}");
                            this.search.update(cx, |s, cx| s.set_text(&query, cx));
                            this.refresh_results(cx);
                        }),
                    ),
            );
        }

        div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .bg(theme.surface)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div().p(px(8.)).child(
                    div()
                        .h(px(32.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.border)
                        .child(self.search.clone()),
                ),
            )
            .child(
                div()
                    .id("vault-root-drop")
                    .px(px(12.))
                    .py(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .drag_over::<DraggedDocument>(move |style, _dragged, _window, _cx| {
                        style.bg(theme.selected)
                    })
                    .drag_over::<DraggedFolder>(move |style, _dragged, _window, _cx| {
                        style.bg(theme.selected)
                    })
                    .on_drop(cx.listener(
                        |this: &mut Self, dragged: &DraggedDocument, _window, cx| {
                            this.drop_document(dragged.id, None, cx);
                        },
                    ))
                    .on_drop(cx.listener(
                        |this: &mut Self, dragged: &DraggedFolder, _window, cx| {
                            this.drop_folder(dragged.path.as_ref(), None, cx);
                        },
                    ))
                    .child(if searching {
                        format!("{} result(s)", rows.len())
                    } else {
                        let documents = self
                            .tree
                            .iter()
                            .filter(|row| matches!(row, TreeRow::Document { .. }))
                            .count();
                        format!("{documents} document(s) · {PRIMARY}N folder/name creates folders")
                    }),
            )
            .child(list)
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(tag_chips),
            )
    }

    /// The open document's identity and tags, colour-coded by provenance:
    /// green is yours, amber is a model's unreviewed proposal, blue is a
    /// proposal you accepted (UI-010).
    /// The tab strip (UI-006): title, dirty dot, close; + for a fresh
    /// tab. One tab renders no strip — a single document needs no chrome.
    fn render_tabs(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let theme = self.theme;
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .px(px(6.))
            .gap(px(2.));
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active_tab;
            let dirty = tab.editor.read(cx).is_dirty();
            let label: SharedString =
                format!("{}{}", tab.title, if dirty { " ●" } else { "" }).into();
            bar =
                bar.child(
                    div()
                        .id(("tab", index))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .px(px(10.))
                        .py(px(4.))
                        .text_size(px(12.))
                        .rounded_t(px(5.))
                        .when(active, |d| {
                            d.bg(theme.surface)
                                .text_color(theme.text)
                                .border_t_2()
                                .border_color(theme.accent)
                        })
                        .when(!active, |d| d.text_color(theme.muted))
                        .hover(|d| d.bg(theme.hover))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.activate_tab(index, window, cx)
                        }))
                        .child(label)
                        .child(
                            div()
                                .id(("tabx", index))
                                .text_color(theme.muted)
                                .hover(|d| d.text_color(theme.text))
                                .child("×")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(index, window, cx)
                                })),
                        ),
                );
        }
        bar = bar.child(
            div()
                .id("tab-plus")
                .px(px(8.))
                .py(px(2.))
                .text_color(theme.muted)
                .hover(|d| d.text_color(theme.text))
                .cursor_pointer()
                .child("+")
                .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
        );
        Some(bar.into_any_element())
    }

    fn render_properties(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let title = self.open_title.clone();

        let mut chips = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(4.));
        for (index, (tag, provenance, review, source)) in self.open_tags.iter().enumerate() {
            let marker = match provenance {
                Provenance::AiGenerated => "~",
                Provenance::AiConfirmed => "+",
                _ => "",
            };
            let mut chip = div()
                .id(("tagchip", index))
                .flex()
                .flex_row()
                .items_center()
                .px(px(6.))
                .py(px(1.))
                .text_size(px(11.))
                .rounded(px(9.))
                .bg(theme.elevated)
                .text_color(theme.tag_colour(*provenance))
                .child(format!("{marker}#{tag}"));

            // A machine proposal awaiting review gets its decision right
            // on the chip (TAG-007, UI-010): accept promotes, reject is
            // durable against reclassification.
            let reviewable = *provenance == Provenance::AiGenerated
                && *review == marius_model::ReviewState::Unreviewed;
            // A tag held only in the database (an accepted AI tag, or one
            // added by hand through the CLI) can be taken back: minds
            // change (TAG-012). An inline or frontmatter tag lives in the
            // note's own text — delete it there, or reindexing would just
            // restore it.
            let removable = !reviewable && *source == marius_model::TagSource::Index;
            if reviewable {
                let accept: SharedString = tag.clone();
                let reject: SharedString = tag.clone();
                chip = chip
                    .child(
                        div()
                            .id(("tagyes", index))
                            .pl(px(5.))
                            .text_color(theme.tag_confirmed)
                            .hover(|d| d.text_color(theme.text))
                            .cursor_pointer()
                            .child("✓")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.review_tag(&accept, true, cx);
                            })),
                    )
                    .child(
                        div()
                            .id(("tagno", index))
                            .pl(px(3.))
                            .text_color(theme.muted)
                            .hover(|d| d.text_color(theme.text))
                            .cursor_pointer()
                            .child("✗")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.review_tag(&reject, false, cx);
                            })),
                    );
            }
            if removable {
                let reject: SharedString = tag.clone();
                chip = chip.child(
                    div()
                        .id(("tagdrop", index))
                        .pl(px(5.))
                        .text_color(theme.muted)
                        .hover(|d| d.text_color(theme.text))
                        .cursor_pointer()
                        .child("✗")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.review_tag(&reject, false, cx);
                        })),
                );
            }
            chips = chips.child(chip);
        }

        let mut strip = div().flex().flex_col();
        strip = strip.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px(px(16.))
                .py(px(7.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .child(div().font_weight(gpui::FontWeight::BOLD).child(title))
                .child(chips),
        );

        // The generated-note banner (AI-011, UI-010): a derived note says
        // so, names its maker and sources, and says whether regeneration
        // may still touch it.
        if let Some(provenance) = &self.open_provenance {
            let sources: Vec<String> = provenance.sources.iter().map(|s| s.label.clone()).collect();
            let state = if provenance.user_edited {
                "edited by hand since — regeneration writes a sibling"
            } else {
                "untouched — regeneration replaces in place"
            };
            strip = strip.child(
                div()
                    .px(px(16.))
                    .py(px(4.))
                    .bg(theme.elevated)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(format!(
                        "⚙ {} · from {} · {}",
                        provenance.attribution(),
                        sources.join(", "),
                        state
                    )),
            );
        }
        strip
    }

    /// Settle one proposed tag from the chip (TAG-007).
    fn review_tag(&mut self, tag: &str, accept: bool, cx: &mut Context<Self>) {
        let Some(id) = self.open_document else { return };
        let outcome = {
            let mut app = self.app.borrow_mut();
            if accept {
                app.approve_tag(id, tag).map(|_| "approved")
            } else {
                app.reject_tag(id, tag, None)
                    .map(|_| "rejected — it will not be re-proposed")
            }
        };
        match outcome {
            Ok(verdict) => self.status = format!("#{tag} {verdict}").into(),
            Err(error) => self.status = format!("review: {error}").into(),
        }
        self.refresh_open_tags();
        self.refresh_tags();
        cx.notify();
    }

    /// The ⌘B side panel (UI-005): backlinks, outgoing links, co-tag
    /// kin and unresolved targets for the open document, each row a
    /// click away.
    fn render_backlinks(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.backlinks_open {
            return None;
        }
        let theme = self.theme;
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(250.))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .overflow_hidden()
            .text_size(px(12.));

        let heading = |label: &str| {
            div()
                .px(px(12.))
                .pt(px(10.))
                .pb(px(2.))
                .text_size(px(10.))
                .text_color(theme.muted)
                .child(label.to_uppercase())
        };
        fn doc_row(
            theme: &Theme,
            index: usize,
            id: DocumentId,
            label: SharedString,
            cx: &mut Context<Workspace>,
        ) -> gpui::Stateful<gpui::Div> {
            let hover_bg = theme.hover;
            div()
                .id(("bl-row", index))
                .px(px(12.))
                .py(px(3.))
                .hover(move |d| d.bg(hover_bg))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.open(id, cx)))
        }
        let mut row_index = 0usize;

        let Some(neighbourhood) = &self.neighbourhood else {
            panel = panel.child(heading("related")).child(
                div()
                    .px(px(12.))
                    .py(px(4.))
                    .text_color(theme.muted)
                    .child("no document open"),
            );
            return Some(panel.into_any_element());
        };

        let resolve =
            |global: &marius_model::GlobalDocumentId| -> Option<(DocumentId, SharedString)> {
                let app = self.app.borrow();
                app.document(global.document)
                    .ok()
                    .map(|d| (d.id, d.title.into()))
            };

        // Backlinks: what points here (GRAPH-002's other half).
        let incoming: Vec<_> = neighbourhood
            .incoming
            .iter()
            .filter_map(|edge| resolve(&edge.endpoints.source))
            .collect();
        panel = panel.child(heading(&format!("backlinks ({})", incoming.len())));
        if incoming.is_empty() {
            panel = panel.child(
                div()
                    .px(px(12.))
                    .py(px(3.))
                    .text_color(theme.muted)
                    .child("nothing links here yet"),
            );
        }
        for (id, label) in incoming {
            row_index += 1;
            panel = panel.child(doc_row(&theme, row_index, id, label, cx));
        }

        // Outgoing links.
        let outgoing: Vec<_> = neighbourhood
            .outgoing
            .iter()
            .filter_map(|edge| resolve(&edge.endpoints.target))
            .collect();
        if !outgoing.is_empty() {
            panel = panel.child(heading(&format!("links to ({})", outgoing.len())));
            for (id, label) in outgoing {
                row_index += 1;
                panel = panel.child(doc_row(&theme, row_index, id, label, cx));
            }
        }

        // Related by shared tags (GRAPH-003).
        if !self.kin_titles.is_empty() {
            panel = panel.child(heading("related by tags"));
            for (id, title, shared) in self.kin_titles.clone() {
                row_index += 1;
                let label: SharedString = format!("{title}  ·{shared}").into();
                panel = panel.child(doc_row(&theme, row_index, id, label, cx));
            }
        }

        // Links that point at nothing yet (MD-007).
        if !neighbourhood.unresolved.is_empty() {
            panel = panel.child(heading("unresolved"));
            for target in &neighbourhood.unresolved {
                panel = panel.child(
                    div()
                        .px(px(12.))
                        .py(px(3.))
                        .text_color(theme.broken_link)
                        .child(format!("[[{target}]]")),
                );
            }
        }

        Some(panel.into_any_element())
    }

    /// The F2 rename prompt, when open.
    fn render_rename(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.rename_open {
            return None;
        }
        let theme = self.theme;
        Some(
            div()
                .id("rename-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_click(cx.listener(|this, _, window, cx| this.close_rename(window, cx)))
                .child(
                    div()
                        .id("rename-panel")
                        .occlude()
                        .mx_auto()
                        .mt(px(110.))
                        .w(px(560.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated)
                        .shadow_lg()
                        .child(
                            div()
                                .px(px(12.))
                                .pt(px(8.))
                                .text_size(px(11.))
                                .text_color(theme.muted)
                                .child(
                                    "Rename or move — change the name, the folders, or both;                                      inbound links are rewritten",
                                ),
                        )
                        .child(div().p(px(8.)).child(self.rename_input.clone())),
                )
                .into_any_element(),
        )
    }

    /// The palette overlay (UI-006), when open: input on top, ranked
    /// matches below, Enter or click to run, Escape to dismiss.
    fn render_palette(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        self.palette_items.as_ref()?;
        let theme = self.theme;
        let matches = self.palette_matches(cx);

        let mut list = div().flex().flex_col();
        for (index, item) in matches.iter().enumerate() {
            let action = item.action.clone();
            let selected = index == self.palette_selected;
            list = list.child(
                div()
                    .id(("palette-row", index))
                    .px(px(12.))
                    .py(px(5.))
                    .text_size(px(13.))
                    .when(selected, |d| d.bg(theme.selected))
                    .hover(|d| d.bg(theme.hover))
                    .cursor_pointer()
                    .child(item.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_palette(window, cx);
                        this.run_palette_action(action.clone(), window, cx);
                    })),
            );
        }
        if matches.is_empty() {
            list = list.child(
                div()
                    .px(px(12.))
                    .py(px(5.))
                    .text_size(px(13.))
                    .text_color(theme.muted)
                    .child("no matching command"),
            );
        }

        Some(
            div()
                .id("palette-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                .child(
                    div()
                        .id("palette-panel")
                        .occlude()
                        .mx_auto()
                        .mt(px(110.))
                        .w(px(560.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated)
                        .shadow_lg()
                        .child(
                            div()
                                .p(px(8.))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(self.palette_input.clone()),
                        )
                        .child(list),
                )
                .into_any_element(),
        )
    }

    fn render_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let (line, column) = self.editor.read(cx).cursor_line_column();
        let dirty = self.editor.read(cx).is_dirty();
        let open_path = self.open_path.clone();

        div()
            .flex()
            .flex_row()
            .justify_between()
            .px(px(12.))
            .py(px(5.))
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.muted)
            .child(div().flex().flex_row().gap(px(10.)).child(format!(
                "{}{}",
                open_path,
                if dirty { "  ●" } else { "" }
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .min_w(px(0.))
                    .child(format!("ln {}, col {}", line + 1, column + 1))
                    .child(
                        // The status gets its own hoverable segment: the
                        // footer truncates a long error, and hovering it
                        // shows the whole message in a tooltip.
                        div()
                            .id("status-message")
                            .max_w(px(560.))
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} · {}",
                                self.vault.name, self.status
                            )))
                            .when(!self.status.is_empty(), |d| {
                                d.tooltip(crate::tip::TextTip::build(
                                    self.status.clone(),
                                    theme,
                                ))
                            }),
                    )
                    .child(SharedString::from(format!(
                        "{PRIMARY}K search · {PRIMARY_SHIFT}P commands · {PRIMARY}E {} · {PRIMARY_SHIFT}E split · {PRIMARY}G graph · {PRIMARY}T tags · {PRIMARY_SHIFT}L {}",
                        match self.mode {
                            ViewMode::Edit => "read",
                            ViewMode::Reading | ViewMode::Split | ViewMode::Graph | ViewMode::Settings => "edit",
                        },
                        self.theme.appearance.toggled().label(),
                    ))),
            )
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search.read(cx).focus_handle(cx);
                window.focus(&handle);
                this.search.update(cx, |search, cx| {
                    search.set_text("", cx);
                });
                this.refresh_results(cx);
            }))
            .on_action(cx.listener(Self::on_toggle_appearance))
            .on_action(cx.listener(Self::on_toggle_reading))
            .on_action(cx.listener(Self::on_toggle_split))
            .on_action(cx.listener(|this, _: &SuggestTags, _w, cx| this.suggest_tags(cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _w, cx| this.set_zoom(this.zoom + 0.1, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _w, cx| this.set_zoom(this.zoom - 0.1, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _w, cx| this.set_zoom(1.0, cx)))
            .on_action(
                cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleBacklinks, _w, cx| {
                this.backlinks_open = !this.backlinks_open;
                this.refresh_neighbourhood();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.mode = match this.mode {
                    ViewMode::Settings => {
                        let handle = this.editor.read(cx).focus_handle(cx);
                        window.focus(&handle);
                        ViewMode::Edit
                    }
                    _ => ViewMode::Settings,
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RenameNote, window, cx| this.open_rename(window, cx)))
            .on_action(cx.listener(|this, _: &ReadingPageDown, _w, cx| {
                if this.mode == ViewMode::Reading {
                    this.reading_scroll(0.9, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ReadingPageUp, _w, cx| {
                if this.mode == ViewMode::Reading {
                    this.reading_scroll(-0.9, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                this.close_tab(this.active_tab, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                let next = (this.active_tab + 1) % this.tabs.len().max(1);
                this.activate_tab(next, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                let count = this.tabs.len().max(1);
                let previous = (this.active_tab + count - 1) % count;
                this.activate_tab(previous, window, cx);
            }))
            .on_action(cx.listener(Self::on_toggle_graph))
            .on_action(cx.listener(|this, _: &NewNote, window, cx| this.new_note(window, cx)))
            .on_action(cx.listener(|this, _: &ReindexVault, _w, cx| this.reindex(cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(14.))
            // Files dragged in from the OS import in place (IM-010).
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.import_dropped(paths.paths().to_vec(), cx)
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow()
                            .h_full()
                            .min_w(px(0.))
                            .children(self.render_tabs(cx))
                            .child(self.render_properties(cx))
                            .child(
                                div()
                                    .flex_grow()
                                    .min_h(px(0.))
                                    .text_size(px(14.0 * self.zoom))
                                    .child(match self.mode {
                                        ViewMode::Edit => self.editor.clone().into_any_element(),
                                        ViewMode::Reading => {
                                            self.preview.clone().into_any_element()
                                        }
                                        ViewMode::Split => div()
                                            .flex()
                                            .flex_row()
                                            .size_full()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .h_full()
                                                    .border_r_1()
                                                    .border_color(theme.border)
                                                    .child(self.editor.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .h_full()
                                                    .child(self.preview.clone()),
                                            )
                                            .into_any_element(),
                                        ViewMode::Graph => self.graph.clone().into_any_element(),
                                        ViewMode::Settings => {
                                            self.settings.clone().into_any_element()
                                        }
                                    }),
                            ),
                    )
                    .children(self.render_backlinks(cx)),
            )
            .child(self.render_status(cx))
            .children(self.render_palette(cx))
            .children(self.render_rename(cx))
    }
}

/// What on-screen hints print for the keymap's `{primary}` modifier:
/// the footer must name the key the fingers actually press.
#[cfg(target_os = "macos")]
const PRIMARY: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const PRIMARY: &str = "Ctrl+";
#[cfg(target_os = "macos")]
const PRIMARY_SHIFT: &str = "⌘⇧";
#[cfg(not(target_os = "macos"))]
const PRIMARY_SHIFT: &str = "Ctrl+Shift+";

/// Hand a file to the platform's default application — the only sane way
/// to "open" a PDF or an image from a notes app.
fn open_with_system_viewer(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(path).status()?;
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .status()?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let status = std::process::Command::new("xdg-open").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "the system viewer refused it ({status})"
        )))
    }
}
