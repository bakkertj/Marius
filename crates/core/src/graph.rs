//! Graph assembly for the knowledge-graph view (GRAPH-001…008, 013, 014).
//!
//! This builds the *data* for a graph — nodes, edges, and enough metadata to
//! style them — from the same tables the rest of the system maintains. Layout
//! and drawing belong to the UI; a CLI can serialise the same structure. That
//! split keeps the graph testable without a window.

use std::collections::{HashMap, HashSet, VecDeque};

use marius_model::{DocumentId, Provenance, RelationshipKind, ReviewState, VaultId};
use marius_storage::{documents, relationships, Connection};

use crate::app::Result;

/// What to include. The defaults draw the classic Obsidian-style global
/// graph: explicit links plus computed shared-tag kinship, archived
/// documents excluded.
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// `Some` renders the local graph around one document (GRAPH-005):
    /// only nodes reachable within `depth` hops are kept.
    pub focus: Option<DocumentId>,
    /// Hop radius for the local view. Ignored when `focus` is `None`.
    pub depth: u32,
    /// Stored relationship kinds to include; empty means all stored kinds.
    pub kinds: Vec<RelationshipKind>,
    /// Add computed co-tag edges (GRAPH-003). They are never stored, so they
    /// are recomputed here each time.
    pub shared_tags: bool,
    /// Keep archived documents in the picture (GRAPH-004 filtering).
    pub include_archived: bool,
    /// Hard ceiling on nodes (GRAPH-014). When the vault is bigger, the
    /// best-connected nodes win and `truncated` is set.
    pub max_nodes: usize,
    /// Ceiling on computed shared-tag edges, and on how broad a tag may be
    /// before its pairs are skipped.
    pub shared_tag_edge_limit: usize,
    pub shared_tag_breadth_cap: u32,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            focus: None,
            depth: 2,
            kinds: Vec::new(),
            shared_tags: true,
            include_archived: false,
            max_nodes: 600,
            shared_tag_edge_limit: 400,
            shared_tag_breadth_cap: 20,
        }
    }
}

/// One document in the graph.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: DocumentId,
    pub title: String,
    pub path: String,
    /// Edge count after filtering — the UI sizes nodes by it.
    pub degree: usize,
    /// Hop distance from the focus; `0` for the focus itself, `None` in the
    /// global view.
    pub distance: Option<u32>,
}

/// How an edge should read visually (GRAPH-013): explicit links, machine
/// inferences and co-tag kinship must be distinguishable at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeStyle {
    /// A link or embed written in the text, or an edge the user drew or
    /// confirmed. Solid.
    Explicit,
    /// Machine-proposed and not yet reviewed. Dashed, lighter.
    Inferred,
    /// Computed from shared tags, never stored. Faint.
    SharedTag,
}

/// One edge, by node index into [`GraphData::nodes`].
#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub from: usize,
    pub to: usize,
    pub kind: RelationshipKind,
    pub style: EdgeStyle,
    /// For shared-tag edges: how many tags the pair shares (edge weight).
    pub weight: u64,
}

/// A ready-to-draw graph.
#[derive(Debug, Clone, Default)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// True when `max_nodes` cut documents out of the picture.
    pub truncated: bool,
}

fn style_for(kind: RelationshipKind, provenance: Provenance, review: ReviewState) -> EdgeStyle {
    match kind {
        RelationshipKind::SharedTag => EdgeStyle::SharedTag,
        _ => {
            if provenance == Provenance::AiGenerated && review == ReviewState::Unreviewed {
                EdgeStyle::Inferred
            } else {
                EdgeStyle::Explicit
            }
        }
    }
}

/// Assemble the graph for one vault.
pub fn assemble(conn: &Connection, vault: VaultId, options: &GraphOptions) -> Result<GraphData> {
    // Documents first: nodes exist even with no edges (orphans matter in a
    // graph view — they are what you forgot to connect).
    let filter = documents::Filter {
        vaults: vec![vault],
        exclude_archived: !options.include_archived,
        ..Default::default()
    };
    let docs = documents::list(conn, &filter)?;
    let mut index: HashMap<DocumentId, usize> = HashMap::with_capacity(docs.len());
    let mut nodes: Vec<GraphNode> = Vec::with_capacity(docs.len());
    for doc in docs {
        index.insert(doc.id, nodes.len());
        nodes.push(GraphNode {
            id: doc.id,
            title: doc.title,
            path: doc.path.to_string(),
            degree: 0,
            distance: None,
        });
    }

    // Stored edges. Endpoints outside the vault (GRAPH-008) or pointing at a
    // filtered-out document are dropped with their edge.
    let mut edges: Vec<GraphEdge> = Vec::new();
    for rel in relationships::in_vaults(conn, &[vault], &options.kinds, None)? {
        if rel.review == ReviewState::Rejected {
            continue;
        }
        let (Some(&from), Some(&to)) = (
            index.get(&rel.endpoints.source.document),
            index.get(&rel.endpoints.target.document),
        ) else {
            continue;
        };
        if from == to {
            continue;
        }
        edges.push(GraphEdge {
            from,
            to,
            kind: rel.kind,
            style: style_for(rel.kind, rel.provenance, rel.review),
            weight: 1,
        });
    }

    // Computed co-tag kinship (GRAPH-003), deduplicated against stored edges:
    // a pair already joined by a real link does not need a faint second line.
    if options.shared_tags
        && (options.kinds.is_empty() || options.kinds.contains(&RelationshipKind::SharedTag))
    {
        let mut joined: HashSet<(usize, usize)> = edges
            .iter()
            .map(|e| (e.from.min(e.to), e.from.max(e.to)))
            .collect();
        for (a, b, shared) in relationships::co_tagged_in_vault(
            conn,
            vault,
            options.shared_tag_breadth_cap,
            options.shared_tag_edge_limit,
        )? {
            let (Some(&from), Some(&to)) = (index.get(&a), index.get(&b)) else {
                continue;
            };
            if !joined.insert((from.min(to), from.max(to))) {
                continue;
            }
            edges.push(GraphEdge {
                from,
                to,
                kind: RelationshipKind::SharedTag,
                style: EdgeStyle::SharedTag,
                weight: shared,
            });
        }
    }

    // Local view (GRAPH-005): breadth-first from the focus over the filtered
    // edges, trimming everything farther than `depth`.
    if let Some(focus) = options.focus {
        let Some(&start) = index.get(&focus) else {
            return Ok(GraphData::default());
        };
        let mut adjacency: HashMap<usize, Vec<usize>> = HashMap::new();
        for e in &edges {
            adjacency.entry(e.from).or_default().push(e.to);
            adjacency.entry(e.to).or_default().push(e.from);
        }
        let mut distance: HashMap<usize, u32> = HashMap::from([(start, 0)]);
        let mut queue = VecDeque::from([start]);
        while let Some(at) = queue.pop_front() {
            let d = distance[&at];
            if d == options.depth {
                continue;
            }
            for &next in adjacency.get(&at).into_iter().flatten() {
                if let std::collections::hash_map::Entry::Vacant(slot) = distance.entry(next) {
                    slot.insert(d + 1);
                    queue.push_back(next);
                }
            }
        }
        let kept = retain_nodes(&mut nodes, &mut edges, |i| distance.contains_key(&i));
        for (new_index, old_index) in kept.iter().enumerate() {
            nodes[new_index].distance = Some(distance[old_index]);
        }
    }

    // Degree, then the node ceiling (GRAPH-014): the most connected survive,
    // and the focus always does.
    tally_degrees(&mut nodes, &edges);
    let mut truncated = false;
    if nodes.len() > options.max_nodes {
        truncated = true;
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        order.sort_by(|&a, &b| {
            let focus_first = |i: usize| options.focus.is_some_and(|f| nodes[i].id == f);
            focus_first(b)
                .cmp(&focus_first(a))
                .then(nodes[b].degree.cmp(&nodes[a].degree))
                .then(nodes[a].path.cmp(&nodes[b].path))
        });
        let keep: HashSet<usize> = order.into_iter().take(options.max_nodes).collect();
        retain_nodes(&mut nodes, &mut edges, |i| keep.contains(&i));
        tally_degrees(&mut nodes, &edges);
    }

    Ok(GraphData {
        nodes,
        edges,
        truncated,
    })
}

/// Drop nodes failing `keep`, remap edge indices, return old indices kept
/// (in their new order).
fn retain_nodes(
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    keep: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut kept_old = Vec::new();
    let mut kept_nodes = Vec::new();
    for (old, node) in nodes.drain(..).enumerate() {
        if keep(old) {
            remap.insert(old, kept_nodes.len());
            kept_old.push(old);
            kept_nodes.push(node);
        }
    }
    *nodes = kept_nodes;
    edges.retain_mut(|e| match (remap.get(&e.from), remap.get(&e.to)) {
        (Some(&f), Some(&t)) => {
            e.from = f;
            e.to = t;
            true
        }
        _ => false,
    });
    kept_old
}

fn tally_degrees(nodes: &mut [GraphNode], edges: &[GraphEdge]) {
    for node in nodes.iter_mut() {
        node.degree = 0;
    }
    for e in edges {
        nodes[e.from].degree += 1;
        nodes[e.to].degree += 1;
    }
}
