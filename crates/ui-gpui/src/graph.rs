//! Force-directed layout for the knowledge graph (GRAPH-001).
//!
//! Pure math — no gpui types — so the simulation can be tested headlessly:
//! determinism, settling, and the invariant that connected components pull
//! together while unrelated nodes spread out. `GraphView` drives one of these
//! a few steps per frame and draws the result.

use marius_core::GraphData;

/// A 2-D point in layout space (abstract units, not pixels; the view applies
/// pan and zoom).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

impl std::ops::Add for Vec2 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
}

impl std::ops::Sub for Vec2 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
}

impl std::ops::Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k)
    }
}

/// Fruchterman–Reingold-style simulation over the nodes of a [`GraphData`].
pub struct Layout {
    pub positions: Vec<Vec2>,
    velocities: Vec<Vec2>,
    /// Nodes the user is holding still (dragging). Forces skip them.
    pinned: Vec<bool>,
    edges: Vec<(usize, usize)>,
    /// Ideal edge length; also sets the repulsion scale.
    spring: f32,
    temperature: f32,
    steps: u32,
}

/// Below this per-step movement the picture has stopped visibly changing.
const SETTLED: f32 = 0.15;
/// Give up annealing after this many steps even if still jittering.
const MAX_STEPS: u32 = 1200;

impl Layout {
    /// Deterministic initial placement: a golden-angle spiral sized to the
    /// node count, so the same graph always starts (and therefore settles)
    /// the same way — no RNG, which keeps tests exact.
    pub fn new(data: &GraphData) -> Self {
        let n = data.nodes.len();
        let spring = 60.0;
        let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
        let positions = (0..n)
            .map(|i| {
                let r = spring * 0.6 * ((i + 1) as f32).sqrt();
                let a = golden * i as f32;
                Vec2::new(r * a.cos(), r * a.sin())
            })
            .collect();
        Self {
            positions,
            velocities: vec![Vec2::default(); n],
            pinned: vec![false; n],
            edges: data.edges.iter().map(|e| (e.from, e.to)).collect(),
            spring,
            temperature: spring * 2.0,
            steps: 0,
        }
    }

    pub fn is_settled(&self) -> bool {
        self.steps >= MAX_STEPS || (self.steps > 0 && self.temperature < SETTLED)
    }

    /// Pin or release a node (while the user drags it).
    pub fn set_pinned(&mut self, index: usize, pinned: bool) {
        if let Some(slot) = self.pinned.get_mut(index) {
            *slot = pinned;
        }
    }

    /// Move a node by hand and wake the simulation so neighbours follow.
    pub fn place(&mut self, index: usize, at: Vec2) {
        if let Some(p) = self.positions.get_mut(index) {
            *p = at;
        }
        self.reheat();
    }

    /// Wake a settled layout (topology unchanged, but something moved).
    /// A full re-anneal: a hand-placed node may be far from where it belongs.
    pub fn reheat(&mut self) {
        self.temperature = self.temperature.max(self.spring);
        self.steps = 0;
    }

    /// One simulation step; returns the largest displacement, which doubles
    /// as the settling signal.
    pub fn step(&mut self) -> f32 {
        let n = self.positions.len();
        if n == 0 {
            self.steps = MAX_STEPS;
            return 0.0;
        }
        let k = self.spring;
        let mut forces = vec![Vec2::default(); n];

        // Repulsion between every pair. O(n²), fine for the capped node
        // counts GraphOptions::max_nodes allows (GRAPH-014 keeps n small).
        for i in 0..n {
            for j in (i + 1)..n {
                let mut d = self.positions[i] - self.positions[j];
                let mut dist = d.length();
                if dist >= k * 6.0 {
                    // Beyond a few spring lengths only gravity acts, or
                    // orphans would be pushed to the horizon and the fitted
                    // view would shrink everything else to dots.
                    continue;
                }
                if dist < 0.01 {
                    // Coincident points (the spiral never produces them, but a
                    // drag can): nudge apart deterministically.
                    d = Vec2::new(0.1 * ((i + 1) as f32), 0.1);
                    dist = d.length();
                }
                // Classic k²/d repulsion, faded linearly to zero at the
                // cutoff (no cliff to oscillate across) and clamped so the
                // tightly-packed start cannot explode outward.
                let fade = 1.0 - dist / (k * 6.0);
                let magnitude = ((k * k) / dist * fade).min(k);
                let f = d * (magnitude / dist);
                forces[i] = forces[i] + f;
                forces[j] = forces[j] - f;
            }
        }

        // Attraction along edges.
        for &(a, b) in &self.edges {
            if a >= n || b >= n {
                continue;
            }
            let d = self.positions[b] - self.positions[a];
            let dist = d.length().max(0.01);
            let pull = dist / k;
            let f = d * (pull / dist);
            forces[a] = forces[a] + f * dist * 0.02;
            forces[b] = forces[b] - f * dist * 0.02;
        }

        // Gentle gravity toward the origin keeps disconnected pieces on
        // screen instead of flying apart forever.
        for (force, position) in forces.iter_mut().zip(&self.positions) {
            *force = *force - *position * 0.02;
        }

        // Displace, clamped by the cooling temperature.
        let mut max_moved: f32 = 0.0;
        for (i, &force) in forces.iter().enumerate() {
            if self.pinned[i] {
                self.velocities[i] = Vec2::default();
                continue;
            }
            let v = (self.velocities[i] + force * 0.4) * 0.6;
            self.velocities[i] = v;
            let speed = v.length();
            let step = if speed > self.temperature {
                v * (self.temperature / speed)
            } else {
                v
            };
            self.positions[i] = self.positions[i] + step;
            max_moved = max_moved.max(step.length());
        }

        self.temperature = (self.temperature * 0.97).max(0.01);
        // Once nothing moves meaningfully, freeze: `is_settled` keys off the
        // temperature, so force it down when motion has already stopped.
        if max_moved < SETTLED {
            self.temperature = 0.0;
        }
        self.steps += 1;
        max_moved
    }

    /// Run to rest. The view never calls this (it steps per frame); tests do.
    pub fn settle(&mut self) {
        while !self.is_settled() {
            self.step();
        }
    }

    /// Axis-aligned bounds of the laid-out points, for fit-to-view.
    pub fn bounds(&self) -> Option<(Vec2, Vec2)> {
        let first = *self.positions.first()?;
        let mut min = first;
        let mut max = first;
        for p in &self.positions {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        Some((min, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marius_core::{EdgeStyle, GraphEdge, GraphNode};
    use marius_model::{DocumentId, RelationshipKind};

    fn data(n: usize, edges: &[(usize, usize)]) -> GraphData {
        GraphData {
            nodes: (0..n)
                .map(|i| GraphNode {
                    id: DocumentId::new(),
                    title: format!("n{i}"),
                    path: format!("n{i}.md"),
                    degree: 0,
                    distance: None,
                })
                .collect(),
            edges: edges
                .iter()
                .map(|&(from, to)| GraphEdge {
                    from,
                    to,
                    kind: RelationshipKind::Link,
                    style: EdgeStyle::Explicit,
                    weight: 1,
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn layout_is_deterministic() {
        let d = data(12, &[(0, 1), (1, 2), (2, 3), (0, 4), (5, 6)]);
        let mut a = Layout::new(&d);
        let mut b = Layout::new(&d);
        a.settle();
        b.settle();
        assert_eq!(a.positions, b.positions);
    }

    #[test]
    fn simulation_settles() {
        let mut layout = Layout::new(&data(30, &[(0, 1), (1, 2), (3, 4)]));
        layout.settle();
        assert!(layout.is_settled());
        // And stays put: another step moves almost nothing.
        assert!(layout.step() < 1.0);
    }

    #[test]
    fn linked_nodes_end_up_closer_than_strangers() {
        // One triangle, one far-flung loner.
        let mut layout = Layout::new(&data(4, &[(0, 1), (1, 2), (2, 0)]));
        layout.settle();
        let d = |a: usize, b: usize| (layout.positions[a] - layout.positions[b]).length();
        let triangle = [d(0, 1), d(1, 2), d(2, 0)];
        for side in triangle {
            assert!(
                side < d(0, 3) && side < d(1, 3),
                "linked pair {side} should sit closer than the loner"
            );
        }
    }

    #[test]
    fn nodes_never_collapse_onto_each_other() {
        let mut layout = Layout::new(&data(20, &[]));
        layout.settle();
        for i in 0..20 {
            for j in (i + 1)..20 {
                assert!(
                    (layout.positions[i] - layout.positions[j]).length() > 1.0,
                    "{i} and {j} overlap"
                );
            }
        }
    }

    #[test]
    fn pinned_nodes_hold_still_and_placing_reheats() {
        let d = data(6, &[(0, 1), (1, 2), (2, 3)]);
        let mut layout = Layout::new(&d);
        layout.settle();
        assert!(layout.is_settled());

        let held = layout.positions[0];
        layout.set_pinned(0, true);
        layout.place(3, Vec2::new(500.0, 500.0));
        assert!(!layout.is_settled(), "placing a node wakes the simulation");
        layout.settle();
        assert_eq!(layout.positions[0], held, "pinned node did not move");
        // Released, the dragged node is pulled back along its edge.
        assert!(
            layout.positions[3].length() < 500.0,
            "released node drifted back toward its neighbours, got {}",
            layout.positions[3].length()
        );
    }

    #[test]
    fn orphans_stay_within_sight_of_the_pack() {
        // Gravity vs. cut-off repulsion: a node with no edges must not be
        // pushed to the horizon, or the fitted view shrinks everything else.
        let mut layout = Layout::new(&data(8, &[(0, 1), (1, 2), (2, 0)]));
        layout.settle();
        let (min, max) = layout.bounds().unwrap();
        let span = (max - min).length();
        assert!(span < 60.0 * 6.0 * 2.5, "layout spans {span}");
    }

    #[test]
    fn empty_graph_is_trivially_settled() {
        let mut layout = Layout::new(&data(0, &[]));
        assert_eq!(layout.step(), 0.0);
        assert!(layout.is_settled());
        assert!(layout.bounds().is_none());
    }

    #[test]
    fn bounds_cover_every_point() {
        let mut layout = Layout::new(&data(9, &[(0, 1), (2, 3)]));
        layout.settle();
        let (min, max) = layout.bounds().unwrap();
        for p in &layout.positions {
            assert!(p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y);
        }
    }
}
