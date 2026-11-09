//! The knowledge-graph view (GRAPH-001): an interactive, force-directed
//! picture of one vault.
//!
//! All graph *data* comes from `Marius::graph` and all *math* from
//! [`crate::graph::Layout`], both headlessly tested. This file is the thin
//! part: a canvas that draws nodes and edges, pan/zoom/drag gestures, and a
//! small toolbar for the local/global and co-tag filters (GRAPH-004/005).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, Bounds, Context, EventEmitter, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Rgba,
    ScrollWheelEvent, SharedString, TextRun, Window,
};
use marius_core::{EdgeStyle, GraphData, GraphOptions, Marius};
use marius_model::{DocumentId, VaultId};

use crate::graph::{Layout, Vec2};
use crate::theme::Theme;

/// What the workspace needs to hear.
pub enum GraphEvent {
    /// The user clicked a node (GRAPH-001: selection opens the document).
    Open(DocumentId),
}

/// Scope of the picture (GRAPH-005).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Local,
}

enum Gesture {
    None,
    /// Dragging empty space moves the camera.
    Pan {
        last: Point<Pixels>,
    },
    /// Dragging a node moves the node (pinned while held). `moved` tells a
    /// click (open the document) from a drag (rearrange the picture).
    Node {
        index: usize,
        moved: bool,
    },
}

pub struct GraphView {
    app: Rc<RefCell<Marius>>,
    vault: VaultId,
    theme: Theme,

    data: GraphData,
    layout: Layout,

    scope: Scope,
    depth: u32,
    shared_tags: bool,
    /// The document the local view centres on; also drawn emphasised in the
    /// global view.
    focus: Option<DocumentId>,

    /// Camera: world point `w` lands at `centre + (w + pan) * zoom`.
    pan: Vec2,
    zoom: f32,
    /// Keep refitting the camera to the settling layout until the user takes
    /// over with a pan or zoom of their own.
    auto_fit: bool,

    hovered: Option<usize>,
    gesture: Gesture,
    /// Canvas bounds from the last frame, for screen↔world conversion in
    /// event handlers.
    bounds: Bounds<Pixels>,
}

impl EventEmitter<GraphEvent> for GraphView {}

const NODE_MIN: f32 = 4.0;
const LABEL_ZOOM: f32 = 0.85;

fn with_alpha(mut colour: Rgba, a: f32) -> Rgba {
    colour.a = a;
    colour
}

impl GraphView {
    pub fn new(app: Rc<RefCell<Marius>>, vault: VaultId, theme: Theme) -> Self {
        let data = GraphData::default();
        let layout = Layout::new(&data);
        Self {
            app,
            vault,
            theme,
            data,
            layout,
            scope: Scope::Local,
            depth: 2,
            shared_tags: true,
            focus: None,
            pan: Vec2::default(),
            zoom: 1.0,
            auto_fit: true,
            hovered: None,
            gesture: Gesture::None,
            bounds: Bounds::default(),
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Rebuild the picture around `focus` (the open document, usually).
    pub fn show(&mut self, focus: Option<DocumentId>, cx: &mut Context<Self>) {
        self.focus = focus;
        if focus.is_none() {
            self.scope = Scope::Global;
        }
        self.reload(cx);
    }

    fn options(&self) -> GraphOptions {
        GraphOptions {
            focus: match self.scope {
                Scope::Local => self.focus,
                Scope::Global => None,
            },
            depth: self.depth,
            shared_tags: self.shared_tags,
            ..GraphOptions::default()
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let result = self.app.borrow().graph(self.vault, &self.options());
        match result {
            Ok(data) => {
                self.layout = Layout::new(&data);
                self.data = data;
                self.hovered = None;
                self.gesture = Gesture::None;
                self.auto_fit = true;
            }
            Err(error) => {
                // Leave the old picture up; the status line is the
                // workspace's, so report through the empty state instead.
                self.data = GraphData::default();
                self.layout = Layout::new(&self.data);
                tracing_report(&error);
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------ camera

    fn centre(&self) -> Point<Pixels> {
        self.bounds.center()
    }

    fn to_screen(&self, w: Vec2) -> Point<Pixels> {
        let c = self.centre();
        point(
            c.x + px((w.x + self.pan.x) * self.zoom),
            c.y + px((w.y + self.pan.y) * self.zoom),
        )
    }

    fn to_world(&self, s: Point<Pixels>) -> Vec2 {
        let c = self.centre();
        Vec2::new(
            f32::from(s.x - c.x) / self.zoom - self.pan.x,
            f32::from(s.y - c.y) / self.zoom - self.pan.y,
        )
    }

    fn node_radius(&self, index: usize) -> f32 {
        let degree = self.data.nodes[index].degree as f32;
        (NODE_MIN + degree.sqrt() * 1.6).min(14.0) * self.zoom.clamp(0.5, 1.6)
    }

    fn node_at(&self, position: Point<Pixels>) -> Option<usize> {
        // Smallest hit-slop that still catches low-degree nodes.
        (0..self.layout.positions.len())
            .rev() // drawn last = on top
            .find(|&i| {
                let s = self.to_screen(self.layout.positions[i]);
                let r = self.node_radius(i).max(6.0) + 2.0;
                let dx = f32::from(position.x - s.x);
                let dy = f32::from(position.y - s.y);
                dx * dx + dy * dy <= r * r
            })
    }

    /// Fit the whole layout into the canvas with a margin.
    fn fit(&mut self) {
        let Some((min, max)) = self.layout.bounds() else {
            return;
        };
        let span = max - min;
        let b = self.bounds;
        if b.size.width <= px(0.) || b.size.height <= px(0.) {
            return;
        }
        let margin = 60.0;
        let sx = (f32::from(b.size.width) - margin) / span.x.max(1.0);
        let sy = (f32::from(b.size.height) - margin) / span.y.max(1.0);
        self.zoom = sx.min(sy).clamp(0.2, 1.5);
        let mid = Vec2::new((min.x + max.x) * 0.5, (min.y + max.y) * 0.5);
        self.pan = Vec2::new(-mid.x, -mid.y);
    }

    // ----------------------------------------------------------- gestures

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if let Some(index) = self.node_at(event.position) {
            self.layout.set_pinned(index, true);
            self.gesture = Gesture::Node {
                index,
                moved: false,
            };
        } else {
            self.gesture = Gesture::Pan {
                last: event.position,
            };
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &mut self.gesture {
            Gesture::Pan { last } => {
                let dx = f32::from(event.position.x - last.x) / self.zoom;
                let dy = f32::from(event.position.y - last.y) / self.zoom;
                *last = event.position;
                self.pan = self.pan + Vec2::new(dx, dy);
                self.auto_fit = false;
                cx.notify();
            }
            Gesture::Node { index, moved } => {
                let index = *index;
                *moved = true;
                let at = self.to_world(event.position);
                self.layout.place(index, at);
                self.auto_fit = false;
                cx.notify();
            }
            Gesture::None => {
                let hovered = self.node_at(event.position);
                if hovered != self.hovered {
                    self.hovered = hovered;
                    cx.notify();
                }
            }
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Gesture::Node { index, moved } = self.gesture {
            self.layout.set_pinned(index, false);
            if !moved {
                // A click, not a drag: open the document (GRAPH-001).
                let _ = event;
                cx.emit(GraphEvent::Open(self.data.nodes[index].id));
            }
        }
        self.gesture = Gesture::None;
        cx.notify();
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dy = match event.delta {
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / 120.0,
            gpui::ScrollDelta::Lines(l) => l.y / 3.0,
        };
        let factor = (dy * 0.9).exp().clamp(0.5, 2.0);
        let new_zoom = (self.zoom * factor).clamp(0.1, 4.0);
        // Keep the world point under the cursor stationary.
        let anchor = self.to_world(event.position);
        self.zoom = new_zoom;
        let c = self.centre();
        self.pan = Vec2::new(
            f32::from(event.position.x - c.x) / self.zoom - anchor.x,
            f32::from(event.position.y - c.y) / self.zoom - anchor.y,
        );
        self.auto_fit = false;
        cx.notify();
    }

    // ------------------------------------------------------------ toolbar

    fn toolbar_button(
        &self,
        label: &'static str,
        active: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id(SharedString::from(label))
            .px(px(8.))
            .py(px(3.))
            .rounded(px(4.))
            .text_size(px(11.))
            .when(active, |d| d.bg(theme.selected).text_color(theme.text))
            .when(!active, |d| d.text_color(theme.muted))
            .hover(|d| d.bg(theme.hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .child(label)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let have_focus = self.focus.is_some();
        let summary = format!(
            "{} notes · {} edges{}",
            self.data.nodes.len(),
            self.data.edges.len(),
            if self.data.truncated {
                " · largest shown"
            } else {
                ""
            }
        );
        div()
            .absolute()
            .top(px(8.))
            .left(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(with_alpha(theme.elevated, 0.92))
            .border_1()
            .border_color(theme.border)
            .child(self.toolbar_button(
                "global",
                self.scope == Scope::Global,
                |this, cx| {
                    this.scope = Scope::Global;
                    this.reload(cx);
                },
                cx,
            ))
            .when(have_focus, |bar| {
                bar.child(self.toolbar_button(
                    "local",
                    self.scope == Scope::Local,
                    |this, cx| {
                        this.scope = Scope::Local;
                        this.reload(cx);
                    },
                    cx,
                ))
            })
            .when(have_focus && self.scope == Scope::Local, |bar| {
                bar.child(self.toolbar_button(
                    "−",
                    false,
                    |this, cx| {
                        this.depth = this.depth.saturating_sub(1).max(1);
                        this.reload(cx);
                    },
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .child(format!("{} hops", self.depth)),
                )
                .child(self.toolbar_button(
                    "+",
                    false,
                    |this, cx| {
                        this.depth = (this.depth + 1).min(6);
                        this.reload(cx);
                    },
                    cx,
                ))
            })
            .child(self.toolbar_button(
                "tags",
                self.shared_tags,
                |this, cx| {
                    this.shared_tags = !this.shared_tags;
                    this.reload(cx);
                },
                cx,
            ))
            .child(self.toolbar_button(
                "fit",
                false,
                |this, cx| {
                    this.auto_fit = true;
                    this.fit();
                    cx.notify();
                },
                cx,
            ))
            .child(
                div()
                    .pl(px(6.))
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(summary),
            )
    }

    // ------------------------------------------------------------ canvas

    fn paint_graph(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.theme;

        // Edges, one stroked path per style so the three origins stay
        // distinguishable (GRAPH-013).
        let styles = [
            (EdgeStyle::SharedTag, with_alpha(theme.muted, 0.18), 1.0),
            (EdgeStyle::Inferred, with_alpha(theme.accent, 0.4), 1.0),
            (EdgeStyle::Explicit, with_alpha(theme.muted, 0.55), 1.4),
        ];
        for (style, colour, width) in styles {
            let mut builder = PathBuilder::stroke(px(width));
            let mut any = false;
            for edge in self.data.edges.iter().filter(|e| e.style == style) {
                let from = self.to_screen(self.layout.positions[edge.from]);
                let to = self.to_screen(self.layout.positions[edge.to]);
                builder.move_to(from);
                builder.line_to(to);
                any = true;
            }
            if any {
                if let Ok(path) = builder.build() {
                    window.paint_path(path, colour);
                }
            }
        }

        // Nodes over edges; focus and hover emphasised.
        for index in 0..self.data.nodes.len() {
            let node = &self.data.nodes[index];
            let at = self.to_screen(self.layout.positions[index]);
            let r = self.node_radius(index);
            let is_focus = self.focus == Some(node.id);
            let is_hover = self.hovered == Some(index);
            if is_hover || is_focus {
                let ring = r + 3.0;
                window.paint_quad(
                    fill(
                        Bounds::new(
                            point(at.x - px(ring), at.y - px(ring)),
                            size(px(ring * 2.0), px(ring * 2.0)),
                        ),
                        with_alpha(theme.accent, 0.3),
                    )
                    .corner_radii(px(ring)),
                );
            }
            let colour = if is_focus {
                theme.accent
            } else if is_hover {
                with_alpha(theme.accent, 0.9)
            } else {
                with_alpha(theme.text, 0.65)
            };
            window.paint_quad(
                fill(
                    Bounds::new(
                        point(at.x - px(r), at.y - px(r)),
                        size(px(r * 2.0), px(r * 2.0)),
                    ),
                    colour,
                )
                .corner_radii(px(r)),
            );
        }

        // Labels: everything when zoomed in enough, otherwise just the
        // hovered and focused nodes.
        let style = window.text_style();
        let font_size = px(11.0);
        for index in 0..self.data.nodes.len() {
            let node = &self.data.nodes[index];
            let is_hover = self.hovered == Some(index);
            let is_focus = self.focus == Some(node.id);
            if !(is_hover || is_focus || self.zoom >= LABEL_ZOOM) {
                continue;
            }
            let text: SharedString = node.title.clone().into();
            let run = TextRun {
                len: text.len(),
                font: style.font(),
                color: if is_hover || is_focus {
                    theme.text.into()
                } else {
                    with_alpha(theme.muted, 0.9).into()
                },
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(text, font_size, &[run], None);
            let at = self.to_screen(self.layout.positions[index]);
            let r = self.node_radius(index);
            let origin = point(at.x - line.width / 2., at.y + px(r + 3.0));
            if origin.y < bounds.origin.y || origin.y > bounds.bottom_left().y {
                continue;
            }
            let _ = line.paint(origin, px(14.0), window, cx);
        }
    }
}

fn tracing_report(error: &dyn std::fmt::Display) {
    eprintln!("graph: {error}");
}

impl Render for GraphView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drive the simulation a few steps per frame until it settles; the
        // camera follows until the user takes over.
        if !self.layout.is_settled() {
            for _ in 0..3 {
                self.layout.step();
            }
            if self.auto_fit {
                self.fit();
            }
            window.request_animation_frame();
        }

        let theme = self.theme;
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let empty = self.data.nodes.is_empty();

        div()
            .size_full()
            .relative()
            .bg(theme.background)
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |view, _| view.bounds = bounds);
                    },
                    move |bounds, _prepaint, window, cx| {
                        let view = paint_entity.clone();
                        view.update(cx, |view, cx| view.paint_graph(bounds, window, cx));
                    },
                )
                .size_full(),
            )
            .when(empty, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left(px(8.))
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child("nothing to draw — index the vault or widen the filters"),
                )
            })
            .child(self.render_toolbar(cx))
    }
}
