//! The knowledge graph as data (GRAPH-001, 003, 005, 013, 014), through the
//! same service API the UI draws from.

use std::fs;

use marius_core::{EdgeStyle, GraphOptions, Marius};
use marius_model::RelationshipKind;

struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let mut app = Marius::open(data.path()).unwrap();
        let vault = app.open_vault("Graph", files.path()).unwrap();
        Self {
            app,
            vault,
            _data: data,
            files,
        }
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.files.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn index(&mut self) {
        let vault = self.vault.clone();
        self.app.index(&vault).unwrap();
    }

    fn graph(&self, options: &GraphOptions) -> marius_core::GraphData {
        self.app.graph(self.vault.id, options).unwrap()
    }

    fn node(&self, data: &marius_core::GraphData, title: &str) -> usize {
        data.nodes
            .iter()
            .position(|n| n.title == title)
            .unwrap_or_else(|| panic!("no node titled {title:?}"))
    }
}

/// A small, connected vault: hub links to two spokes, one spoke links back,
/// an orphan floats free, and two notes share a tag without linking.
fn seeded() -> Harness {
    let mut h = Harness::new();
    h.write("hub.md", "# Hub\n\nSee [[spoke-a]] and [[spoke-b]].\n");
    h.write("spoke-a.md", "# Spoke A\n\nBack to [[hub]]. #beacon\n");
    h.write("spoke-b.md", "# Spoke B\n\nQuiet. #beacon\n");
    h.write("orphan.md", "# Orphan\n\nNo links, no tags.\n");
    h.index();
    h
}

#[test]
fn global_graph_has_every_document_and_both_edge_sources() {
    // GRAPH-001/002/003: explicit links and computed co-tag kinship, with
    // orphans still visible.
    let h = seeded();
    let g = h.graph(&GraphOptions::default());

    assert_eq!(g.nodes.len(), 4);
    assert!(!g.truncated);

    let hub = h.node(&g, "Hub");
    let a = h.node(&g, "Spoke A");
    let b = h.node(&g, "Spoke B");
    let orphan = h.node(&g, "Orphan");

    let links: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.kind == RelationshipKind::Link)
        .collect();
    assert_eq!(links.len(), 3); // hub→a, hub→b, a→hub
    assert!(links.iter().all(|e| e.style == EdgeStyle::Explicit));

    // The co-tag pair is computed, faint, weighted — and deduplicated against
    // real links (a and b share "beacon" and have no link between them).
    let kinship: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.kind == RelationshipKind::SharedTag)
        .collect();
    assert_eq!(kinship.len(), 1);
    assert_eq!(kinship[0].style, EdgeStyle::SharedTag);
    assert_eq!(kinship[0].weight, 1);
    let pair = [kinship[0].from, kinship[0].to];
    assert!(pair.contains(&a) && pair.contains(&b));

    // Degree feeds node sizing: the hub has three link endpoints.
    assert_eq!(g.nodes[hub].degree, 3);
    assert_eq!(g.nodes[orphan].degree, 0);
    assert!(g.nodes.iter().all(|n| n.distance.is_none()));
}

#[test]
fn local_graph_is_a_bounded_neighbourhood() {
    // GRAPH-005: depth 1 around Spoke B keeps the hub; the orphan and the
    // co-tagged spoke A at distance 2 drop out at that radius.
    let h = seeded();
    let full = h.graph(&GraphOptions::default());
    let b_id = full.nodes[h.node(&full, "Spoke B")].id;

    let local = h.graph(&GraphOptions {
        focus: Some(b_id),
        depth: 1,
        ..GraphOptions::default()
    });
    let mut titles: Vec<_> = local.nodes.iter().map(|n| n.title.as_str()).collect();
    titles.sort();
    assert_eq!(titles, ["Hub", "Spoke A", "Spoke B"]); // A is 1 hop via the co-tag edge
    assert_eq!(local.nodes[h.node(&local, "Spoke B")].distance, Some(0));
    assert_eq!(local.nodes[h.node(&local, "Hub")].distance, Some(1));

    // Depth 2 pulls in everything connected; the orphan never appears.
    let wider = h.graph(&GraphOptions {
        focus: Some(b_id),
        depth: 2,
        ..GraphOptions::default()
    });
    assert_eq!(wider.nodes.len(), 3);
    assert!(wider.nodes.iter().all(|n| n.title != "Orphan"));
}

#[test]
fn filters_narrow_kinds_and_shared_tags_can_be_switched_off() {
    // GRAPH-004: the view can be narrowed by relationship kind.
    let h = seeded();
    let g = h.graph(&GraphOptions {
        shared_tags: false,
        ..GraphOptions::default()
    });
    assert!(g
        .edges
        .iter()
        .all(|e| e.kind != RelationshipKind::SharedTag));

    let only_tags = h.graph(&GraphOptions {
        kinds: vec![RelationshipKind::SharedTag],
        ..GraphOptions::default()
    });
    assert!(only_tags
        .edges
        .iter()
        .all(|e| e.kind == RelationshipKind::SharedTag));
    assert_eq!(only_tags.edges.len(), 1);
}

#[test]
fn the_node_ceiling_keeps_the_best_connected_and_reports_truncation() {
    // GRAPH-014: more documents than the ceiling → the hub survives, the
    // graph admits it is partial.
    let h = seeded();
    let g = h.graph(&GraphOptions {
        max_nodes: 2,
        ..GraphOptions::default()
    });
    assert!(g.truncated);
    assert_eq!(g.nodes.len(), 2);
    assert!(g.nodes.iter().any(|n| n.title == "Hub"));
    // Edge indices were remapped, not left dangling.
    for e in &g.edges {
        assert!(e.from < g.nodes.len() && e.to < g.nodes.len());
    }

    // The focused document survives truncation even with degree zero.
    let full = h.graph(&GraphOptions::default());
    let orphan_id = full.nodes[h.node(&full, "Orphan")].id;
    let g = h.graph(&GraphOptions {
        focus: Some(orphan_id),
        depth: 99,
        ..GraphOptions::default()
    });
    // Local view of an orphan is just the orphan.
    assert_eq!(g.nodes.len(), 1);
    assert_eq!(g.nodes[0].title, "Orphan");
}

#[test]
fn archived_documents_leave_the_graph_unless_asked_for() {
    let mut h = seeded();
    let full = h.graph(&GraphOptions::default());
    let b_id = full.nodes[h.node(&full, "Spoke B")].id;
    h.app.set_archived(b_id, true).unwrap();

    let g = h.graph(&GraphOptions::default());
    assert_eq!(g.nodes.len(), 3);
    assert!(g.nodes.iter().all(|n| n.title != "Spoke B"));
    // Edges to the hidden node went with it.
    assert!(g
        .edges
        .iter()
        .all(|e| e.from < g.nodes.len() && e.to < g.nodes.len()));

    let with = h.graph(&GraphOptions {
        include_archived: true,
        ..GraphOptions::default()
    });
    assert_eq!(with.nodes.len(), 4);
}
