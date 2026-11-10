//! The Markdown editor: a GPUI element over the tested [`crate::buffer`].
//!
//! Requirements: MD-001 (native editor), MD-004 (undo/redo), MD-009
//! (syntax highlighting), UI-003 (keyboard-first editing).
//!
//! ## Division of labour
//!
//! Everything that can be wrong in an interesting way — edits, motions,
//! selection, undo grouping, list continuation — lives in [`crate::buffer`]
//! and [`crate::highlight`], where tests reach it. This file is deliberately
//! the boring half: it maps key and mouse events to buffer calls, shapes the
//! text with soft wrap, and paints what the buffer says. The IME contract
//! follows GPUI's own `input.rs` example, which is the reference
//! implementation of `EntityInputHandler`.
//!
//! ## Geometry
//!
//! `shape_text` returns one [`WrappedLine`] per logical line, each knowing
//! where it soft-wraps. Byte offset ⇄ pixel mapping goes through two steps:
//! logical line (by cumulative height), then row within the line (by wrap
//! boundaries). Vertical cursor movement is *visual* — up from the end of a
//! wrapped paragraph lands on the row above, not the previous paragraph —
//! which is the behaviour prose editing requires.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, ElementId, ElementInputHandler, Entity, EventEmitter, FocusHandle, Focusable,
    Font, FontWeight, GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, SharedString, Style, TextRun,
    UTF16Selection, Window, WrappedLine,
};

use crate::buffer::{Buffer, Motion};
use crate::highlight::{self, LinkResolver, Token};
use crate::theme::Theme;

actions!(
    editor,
    [
        Backspace,
        Delete,
        DeleteWordBack,
        Enter,
        Tab,
        ShiftTab,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        LineStart,
        LineEnd,
        SelectLineStart,
        SelectLineEnd,
        DocumentStart,
        DocumentEnd,
        PageUp,
        PageDown,
        SelectAll,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        Save,
        Escape,
    ]
);

/// The key bindings the editor needs; the application registers them once.
pub fn key_bindings() -> Vec<KeyBinding> {
    const CTX: Option<&str> = Some("Editor");
    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, CTX),
        KeyBinding::new("delete", Delete, CTX),
        KeyBinding::new("alt-backspace", DeleteWordBack, CTX),
        KeyBinding::new("enter", Enter, CTX),
        KeyBinding::new("tab", Tab, CTX),
        KeyBinding::new("shift-tab", ShiftTab, CTX),
        KeyBinding::new("left", MoveLeft, CTX),
        KeyBinding::new("right", MoveRight, CTX),
        KeyBinding::new("up", MoveUp, CTX),
        KeyBinding::new("down", MoveDown, CTX),
        KeyBinding::new("shift-left", SelectLeft, CTX),
        KeyBinding::new("shift-right", SelectRight, CTX),
        KeyBinding::new("shift-up", SelectUp, CTX),
        KeyBinding::new("shift-down", SelectDown, CTX),
        KeyBinding::new("pageup", PageUp, CTX),
        KeyBinding::new("pagedown", PageDown, CTX),
        KeyBinding::new("escape", Escape, CTX),
    ];
    // Platform-conventional chords (UI-003): cmd on macOS, ctrl elsewhere.
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let word = if cfg!(target_os = "macos") {
        "alt"
    } else {
        "ctrl"
    };
    let key = |keys: String| -> &'static str { Box::leak(keys.into_boxed_str()) };
    bindings.extend([
        KeyBinding::new(key(format!("{word}-left")), WordLeft, CTX),
        KeyBinding::new(key(format!("{word}-right")), WordRight, CTX),
        KeyBinding::new(key(format!("{word}-shift-left")), SelectWordLeft, CTX),
        KeyBinding::new(key(format!("{word}-shift-right")), SelectWordRight, CTX),
        KeyBinding::new("home", LineStart, CTX),
        KeyBinding::new("end", LineEnd, CTX),
        KeyBinding::new("shift-home", SelectLineStart, CTX),
        KeyBinding::new("shift-end", SelectLineEnd, CTX),
        KeyBinding::new(key(format!("{primary}-left")), LineStart, CTX),
        KeyBinding::new(key(format!("{primary}-right")), LineEnd, CTX),
        KeyBinding::new(key(format!("{primary}-shift-left")), SelectLineStart, CTX),
        KeyBinding::new(key(format!("{primary}-shift-right")), SelectLineEnd, CTX),
        KeyBinding::new(key(format!("{primary}-up")), DocumentStart, CTX),
        KeyBinding::new(key(format!("{primary}-down")), DocumentEnd, CTX),
        KeyBinding::new(key(format!("{primary}-a")), SelectAll, CTX),
        KeyBinding::new(key(format!("{primary}-z")), Undo, CTX),
        KeyBinding::new(key(format!("{primary}-shift-z")), Redo, CTX),
        KeyBinding::new(key(format!("{primary}-c")), Copy, CTX),
        KeyBinding::new(key(format!("{primary}-x")), Cut, CTX),
        KeyBinding::new(key(format!("{primary}-v")), Paste, CTX),
        KeyBinding::new(key(format!("{primary}-s")), Save, CTX),
    ]);
    bindings
}

/// What the editor tells the application.
#[derive(Debug, Clone)]
pub enum EditorEvent {
    /// The content changed; the dirty marker may need repainting.
    Edited,
    /// The user asked to save (MD-004's explicit half; autosave is the app's).
    SaveRequested,
    /// Return was pressed in a single-line field.
    Submitted,
    /// Up/down in a single-line field: the list below should move instead.
    NavigateUp,
    NavigateDown,
    /// Escape was pressed.
    Cancelled,
}

/// The shaped geometry of the last frame, kept for mouse mapping and IME.
pub struct EditorLayout {
    lines: Vec<WrappedLine>,
    /// Byte offset of each logical line's start.
    line_starts: Vec<usize>,
    line_height: Pixels,
    /// Where the text begins on screen, scroll already applied.
    origin: Point<Pixels>,
    visible: Bounds<Pixels>,
}

impl EditorLayout {
    /// Pixel position of a byte offset, relative to the text origin.
    fn position_of(&self, offset: usize) -> Point<Pixels> {
        let line_ix = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let mut y = px(0.);
        for line in &self.lines[..line_ix] {
            y += self.line_height * (line.wrap_boundaries().len() as f32 + 1.);
        }
        let line = &self.lines[line_ix];
        let within = (offset - self.line_starts[line_ix]).min(line.len());
        let (row, row_start) = row_of(line, within);
        let x = x_in_row(line, row_start, within);
        point(x, y + self.line_height * row as f32)
    }

    /// Byte offset closest to a pixel position relative to the text origin.
    fn offset_at(&self, position: Point<Pixels>) -> usize {
        if position.y < px(0.) {
            return 0;
        }
        let mut y = px(0.);
        for (ix, line) in self.lines.iter().enumerate() {
            let height = self.line_height * (line.wrap_boundaries().len() as f32 + 1.);
            if position.y < y + height {
                let local = point(position.x.max(px(0.)), position.y - y);
                let within = match line.closest_index_for_position(local, self.line_height) {
                    Ok(i) | Err(i) => i,
                };
                return self.line_starts[ix] + within.min(line.len());
            }
            y += height;
        }
        self.line_starts
            .last()
            .map(|start| start + self.lines.last().map_or(0, |l| l.len()))
            .unwrap_or(0)
    }

    fn total_height(&self) -> Pixels {
        let rows: usize = self
            .lines
            .iter()
            .map(|l| l.wrap_boundaries().len() + 1)
            .sum();
        self.line_height * rows as f32
    }
}

/// Which visual row of a wrapped line a byte offset falls in, and that row's
/// starting byte offset.
fn row_of(line: &WrappedLine, within: usize) -> (usize, usize) {
    let mut row = 0;
    let mut row_start = 0;
    for boundary in line.wrap_boundaries() {
        let run = &line.unwrapped_layout.runs[boundary.run_ix];
        let boundary_ix = run.glyphs[boundary.glyph_ix].index;
        if within < boundary_ix {
            break;
        }
        row += 1;
        row_start = boundary_ix;
    }
    (row, row_start)
}

/// X position of `within` measured from its row's start.
fn x_in_row(line: &WrappedLine, row_start: usize, within: usize) -> Pixels {
    line.unwrapped_layout.x_for_index(within) - line.unwrapped_layout.x_for_index(row_start)
}

/// The editor entity: the buffer plus everything a window needs around it.
pub struct Editor {
    buffer: Buffer,
    focus_handle: FocusHandle,
    theme: Theme,
    /// IME composition range, underlined while composing.
    marked_range: Option<Range<usize>>,
    scroll_y: Pixels,
    /// Remembered column for a run of vertical movements.
    goal_x: Option<Pixels>,
    /// Keep the cursor visible on the next frame.
    scroll_to_cursor: bool,
    selecting: bool,
    last_layout: Option<EditorLayout>,
    resolver: Box<dyn LinkResolver>,
    /// A one-line field: return submits instead of inserting, highlighting is
    /// off, padding is compact. The machinery — buffer, undo, IME, mouse —
    /// is identical, which is the point of having exactly one text component.
    single_line: bool,
    /// Shown dimmed when the buffer is empty.
    placeholder: SharedString,
    /// Blink phase: the cursor paints only while true. Reset to visible by
    /// every edit or movement, so the cursor never blinks away mid-typing.
    cursor_visible: bool,
    /// When the cursor last moved or the text last changed.
    last_activity: std::time::Instant,
    /// Offset painted last frame, to detect movement centrally.
    last_painted_cursor: Option<usize>,
}

impl EventEmitter<EditorEvent> for Editor {}

impl Editor {
    pub fn new(cx: &mut Context<Self>, theme: Theme, resolver: Box<dyn LinkResolver>) -> Self {
        let editor = Self {
            buffer: Buffer::new(""),
            focus_handle: cx.focus_handle(),
            theme,
            marked_range: None,
            scroll_y: px(0.),
            goal_x: None,
            scroll_to_cursor: false,
            selecting: false,
            last_layout: None,
            resolver,
            single_line: false,
            placeholder: SharedString::default(),
            cursor_visible: true,
            last_activity: std::time::Instant::now(),
            last_painted_cursor: None,
        };
        // The blink clock. One task per editor, alive as long as it is;
        // a settled cursor alternates every half second, and any activity
        // snaps it back on (handled in `wake_cursor`).
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(530))
                .await;
            let Ok(()) = this.update(cx, |editor: &mut Editor, cx| {
                let settled =
                    editor.last_activity.elapsed() >= std::time::Duration::from_millis(500);
                let next = if settled {
                    !editor.cursor_visible
                } else {
                    true
                };
                if next != editor.cursor_visible {
                    editor.cursor_visible = next;
                    cx.notify();
                }
            }) else {
                break;
            };
        })
        .detach();
        editor
    }

    /// A compact single-line field sharing all of the editor's machinery.
    pub fn single_line(
        cx: &mut Context<Self>,
        theme: Theme,
        placeholder: impl Into<SharedString>,
    ) -> Self {
        let mut editor = Self::new(cx, theme, Box::new(crate::highlight::AllLinksResolve));
        editor.single_line = true;
        editor.placeholder = placeholder.into();
        editor
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    /// Load a document, resetting history and scroll.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer = Buffer::new(text);
        self.marked_range = None;
        self.scroll_y = px(0.);
        self.goal_x = None;
        cx.notify();
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        self.buffer.mark_saved();
        cx.notify();
    }

    /// Select everything, so the next keystroke replaces the content —
    /// what a search field should do when focus returns to it.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.buffer.select_all();
        cx.notify();
    }

    /// Zero-based cursor position for the status bar.
    pub fn cursor_line_column(&self) -> (usize, usize) {
        self.buffer.line_and_column(self.buffer.cursor())
    }

    // Every mutation funnels through these two, so notification, scroll
    // follow and the dirty event cannot be forgotten at one of thirty sites.
    fn edited(&mut self, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.scroll_to_cursor = true;
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn moved(&mut self, cx: &mut Context<Self>) {
        self.scroll_to_cursor = true;
        cx.notify();
    }

    fn motion(&mut self, motion: Motion, select: bool, cx: &mut Context<Self>) {
        self.buffer.move_cursor(motion, select);
        self.goal_x = None;
        self.moved(cx);
    }

    /// Visual vertical movement: one row up or down, keeping the column.
    fn vertical(&mut self, rows: f32, select: bool, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(if rows < 0. {
                EditorEvent::NavigateUp
            } else {
                EditorEvent::NavigateDown
            });
            return;
        }
        let Some(layout) = &self.last_layout else {
            return;
        };
        let here = layout.position_of(self.buffer.cursor());
        let goal = *self.goal_x.get_or_insert(here.x);
        let target = point(goal, here.y + layout.line_height * rows);
        let offset = if target.y < px(0.) {
            0
        } else if target.y >= layout.total_height() {
            self.buffer.len()
        } else {
            layout.offset_at(target)
        };
        self.buffer.set_cursor(offset, select);
        self.moved(cx);
    }

    fn page_rows(&self) -> f32 {
        self.last_layout
            .as_ref()
            .map(|l| (l.visible.size.height / l.line_height).floor() - 1.)
            .unwrap_or(20.)
            .max(1.)
    }

    // ------------------------------------------------------- action handlers

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.backspace();
        self.edited(cx);
    }

    fn on_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.delete_forward();
        self.edited(cx);
    }

    fn on_delete_word(&mut self, _: &DeleteWordBack, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.delete_word_back();
        self.edited(cx);
    }

    fn on_enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(EditorEvent::Submitted);
            return;
        }
        self.buffer.insert_newline();
        self.edited(cx);
    }

    fn on_escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.has_selection() {
            let cursor = self.buffer.cursor();
            self.buffer.set_cursor(cursor, false);
            cx.notify();
        } else {
            cx.emit(EditorEvent::Cancelled);
        }
    }

    fn on_tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        // In a table row, Tab walks the cells (MD-013); elsewhere it is
        // two spaces, the Markdown list-indent unit.
        if self.buffer.table_next_cell() {
            self.edited(cx);
            return;
        }
        self.buffer.insert("  ");
        self.edited(cx);
    }

    fn on_shift_tab(&mut self, _: &ShiftTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.table_previous_cell() {
            cx.notify();
        }
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        cx.notify();
    }

    fn on_undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.undo();
        self.edited(cx);
    }

    fn on_redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.redo();
        self.edited(cx);
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buffer.selected_text().to_owned(),
            ));
        }
    }

    fn on_cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buffer.selected_text().to_owned(),
            ));
            self.buffer.backspace();
            self.edited(cx);
        }
    }

    fn on_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // One undo step for the whole paste, via the IME-style path.
            let range = self.buffer.selection();
            self.buffer.replace_range(range, &text);
            self.edited(cx);
        }
    }

    fn on_save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::SaveRequested);
    }

    // --------------------------------------------------------------- mouse

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let Some(layout) = &self.last_layout else {
            return;
        };
        let local = point(
            event.position.x - layout.origin.x,
            event.position.y - layout.origin.y,
        );
        let offset = layout.offset_at(local);
        match event.click_count {
            2 => self.buffer.select_word_at(offset),
            3 => self.buffer.select_line_at(offset),
            _ => {
                self.buffer.set_cursor(offset, event.modifiers.shift);
                self.selecting = true;
            }
        }
        self.goal_x = None;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let Some(layout) = &self.last_layout else {
            return;
        };
        let local = point(
            event.position.x - layout.origin.x,
            event.position.y - layout.origin.y,
        );
        let offset = layout.offset_at(local);
        self.buffer.set_cursor(offset, true);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = &self.last_layout else {
            return;
        };
        let delta = event.delta.pixel_delta(layout.line_height);
        let max = (layout.total_height() - layout.visible.size.height).max(px(0.));
        self.scroll_y = (self.scroll_y - delta.y).max(px(0.)).min(max);
        cx.notify();
    }

    // ---------------------------------------------------------------- utf16

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.buffer.text().chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.buffer.text().chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The IME contract, after GPUI's own reference example. This is what makes
/// dead keys, dictation and CJK composition work (MD-001 is not met without
/// them).
impl gpui::EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.buffer.text().get(range)?.to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.buffer.selection()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone());
        match range {
            // Ordinary typing arrives here with no range: the buffer's own
            // insert applies, with its undo coalescing.
            None => self.buffer.insert(new_text),
            Some(range) => self.buffer.replace_range(range, new_text),
        }
        self.marked_range = None;
        self.edited(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.buffer.selection());
        self.buffer.replace_range(range.clone(), new_text);
        self.marked_range = Some(range.start..range.start + new_text.len());
        if let Some(selected) = new_selected_range_utf16 {
            let selected = self.range_from_utf16(&selected);
            self.buffer
                .set_cursor(range.start + selected.end.min(new_text.len()), false);
        }
        self.edited(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position_of(range.start);
        Some(Bounds::new(
            point(
                element_bounds.origin.x + start.x,
                element_bounds.origin.y + start.y,
            ),
            size(px(2.), layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let local = point(position.x - layout.origin.x, position.y - layout.origin.y);
        Some(self.offset_to_utf16(layout.offset_at(local)))
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(if self.single_line {
                self.theme.elevated
            } else {
                self.theme.background
            })
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_word))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_escape))
            .on_action(
                cx.listener(|this, _: &MoveLeft, _w, cx| this.motion(Motion::Left, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveRight, _w, cx| this.motion(Motion::Right, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectLeft, _w, cx| this.motion(Motion::Left, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectRight, _w, cx| this.motion(Motion::Right, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &WordLeft, _w, cx| this.motion(Motion::WordLeft, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &WordRight, _w, cx| {
                    this.motion(Motion::WordRight, false, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SelectWordLeft, _w, cx| {
                this.motion(Motion::WordLeft, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordRight, _w, cx| {
                this.motion(Motion::WordRight, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &LineStart, _w, cx| {
                    this.motion(Motion::LineStart, false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &LineEnd, _w, cx| this.motion(Motion::LineEnd, false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLineStart, _w, cx| {
                this.motion(Motion::LineStart, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectLineEnd, _w, cx| {
                    this.motion(Motion::LineEnd, true, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &DocumentStart, _w, cx| {
                this.motion(Motion::DocumentStart, false, cx)
            }))
            .on_action(cx.listener(|this, _: &DocumentEnd, _w, cx| {
                this.motion(Motion::DocumentEnd, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _w, cx| this.vertical(-1., false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _w, cx| this.vertical(1., false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _w, cx| this.vertical(-1., true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _w, cx| this.vertical(1., true, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _w, cx| {
                let rows = this.page_rows();
                this.vertical(-rows, false, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _w, cx| {
                let rows = this.page_rows();
                this.vertical(rows, false, cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(EditorElement {
                editor: cx.entity().clone(),
            })
    }
}

/// The custom element that shapes and paints the buffer.
struct EditorElement {
    editor: Entity<Editor>,
}

/// Everything computed in prepaint and consumed by paint.
struct Frame {
    layout: EditorLayout,
    selection_quads: Vec<gpui::PaintQuad>,
    cursor_quad: Option<gpui::PaintQuad>,
}

/// Inner padding around the text.
const PADDING: f32 = 16.;

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Frame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Frame {
        let editor = self.editor.read(cx);
        let theme = editor.theme;
        let text: SharedString = editor.buffer.text().to_owned().into();
        let selection = editor.buffer.selection();
        let cursor_offset = editor.buffer.cursor();
        let marked = editor.marked_range.clone();
        let mut scroll_y = editor.scroll_y;
        let follow_cursor = editor.scroll_to_cursor;
        let focused = editor.focus_handle.is_focused(window);

        let single_line = editor.single_line;
        let padding = if single_line { px(8.) } else { px(PADDING) };
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        // A single-line field never wraps; it would turn into two rows.
        let wrap_width = if single_line {
            px(100_000.)
        } else {
            (bounds.size.width - padding * 2.).max(px(50.))
        };

        // Highlight tokens -> text runs across the whole document. A
        // single-line field shows the placeholder dimmed when empty, and is
        // never markdown-highlighted — a search query is not a document.
        let (text, runs): (SharedString, Vec<TextRun>) = if single_line {
            let (shown, colour) = if text.is_empty() {
                (editor.placeholder.clone(), theme.muted)
            } else {
                (text.clone(), theme.text)
            };
            let runs = vec![TextRun {
                len: shown.len(),
                font: style.font(),
                color: colour.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            (shown, runs)
        } else {
            let runs = text_runs(
                &text,
                &style.font(),
                &theme,
                editor.resolver.as_ref(),
                &marked,
            );
            (text, runs)
        };

        let lines = window
            .text_system()
            .shape_text(text.clone(), font_size, &runs, Some(wrap_width), None)
            .unwrap_or_default()
            .into_vec();

        // Byte offset of each logical line: shape_text yields one entry per
        // '\n'-separated line.
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut acc = 0usize;
        for line in &lines {
            line_starts.push(acc);
            acc += line.len() + 1;
        }

        let mut layout = EditorLayout {
            lines,
            line_starts,
            line_height,
            origin: bounds.origin, // corrected below once scroll is final
            visible: bounds,
        };

        // Keep the cursor on screen after an edit or motion.
        if follow_cursor {
            let pos = layout.position_of(cursor_offset);
            let top = scroll_y;
            let bottom = scroll_y + bounds.size.height - padding * 2.;
            if pos.y < top {
                scroll_y = pos.y;
            } else if pos.y + line_height > bottom {
                scroll_y = pos.y + line_height - (bounds.size.height - px(PADDING * 2.));
            }
        }
        let max_scroll = (layout.total_height() + padding * 2. - bounds.size.height).max(px(0.));
        scroll_y = scroll_y.max(px(0.)).min(max_scroll);

        let vertical_pad = if single_line {
            // Centre the one row vertically in whatever box holds the field.
            ((bounds.size.height - line_height) / 2.).max(px(0.))
        } else {
            padding
        };
        let origin = point(
            bounds.origin.x + padding,
            bounds.origin.y + vertical_pad - scroll_y,
        );
        layout.origin = origin;

        // Selection quads, one per visual row the selection touches.
        let mut selection_quads = Vec::new();
        if selection.start != selection.end {
            selection_quads = selection_rows(&layout, &selection)
                .into_iter()
                .map(|(row_origin, width)| {
                    fill(
                        Bounds::new(
                            point(origin.x + row_origin.x, origin.y + row_origin.y),
                            size(width.max(px(3.)), line_height),
                        ),
                        theme.selected,
                    )
                })
                .collect();
        }

        let cursor_quad = (focused && selection.is_empty() && editor.cursor_visible).then(|| {
            let pos = layout.position_of(cursor_offset);
            fill(
                Bounds::new(
                    point(origin.x + pos.x, origin.y + pos.y),
                    size(px(2.), line_height),
                ),
                theme.accent,
            )
        });

        // Persist the corrected scroll, and wake the cursor on any
        // movement or edit: comparing here catches every path — typing,
        // arrows, mouse, IME — without touching each of them.
        self.editor.update(cx, |editor, _| {
            editor.scroll_y = scroll_y;
            editor.scroll_to_cursor = false;
            if editor.last_painted_cursor != Some(cursor_offset) {
                editor.last_painted_cursor = Some(cursor_offset);
                editor.cursor_visible = true;
                editor.last_activity = std::time::Instant::now();
            }
        });

        Frame {
            layout,
            selection_quads,
            cursor_quad,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in frame.selection_quads.drain(..) {
                window.paint_quad(quad);
            }

            let origin = frame.layout.origin;
            let mut y = px(0.);
            for line in &frame.layout.lines {
                let height = frame.layout.line_height * (line.wrap_boundaries().len() as f32 + 1.);
                // Paint only what intersects the viewport; a long document
                // must not pay for its off-screen text.
                if origin.y + y + height >= bounds.origin.y
                    && origin.y + y <= bounds.origin.y + bounds.size.height
                {
                    line.paint(
                        point(origin.x, origin.y + y),
                        frame.layout.line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
                }
                y += height;
            }

            if let Some(cursor) = frame.cursor_quad.take() {
                window.paint_quad(cursor);
            }
        });

        let placeholder = EditorLayout {
            lines: Vec::new(),
            line_starts: Vec::new(),
            line_height: frame.layout.line_height,
            origin: frame.layout.origin,
            visible: frame.layout.visible,
        };
        let layout = std::mem::replace(&mut frame.layout, placeholder);
        self.editor.update(cx, |editor, _| {
            editor.last_layout = Some(layout);
        });
    }
}

/// Rows a selection covers: (row origin relative to text origin, width).
fn selection_rows(layout: &EditorLayout, selection: &Range<usize>) -> Vec<(Point<Pixels>, Pixels)> {
    let mut rows = Vec::new();
    let mut y = px(0.);
    for (ix, line) in layout.lines.iter().enumerate() {
        let line_start = layout.line_starts[ix];
        let line_end = line_start + line.len();
        let row_count = line.wrap_boundaries().len() + 1;
        let line_px_height = layout.line_height * row_count as f32;

        // Selection range clipped to this logical line (newline inclusive).
        let sel_start = selection.start.max(line_start);
        let sel_end = selection.end.min(line_end + 1);
        if sel_start < sel_end {
            // Row starts within this line.
            let mut row_starts = vec![0usize];
            for boundary in line.wrap_boundaries() {
                let run = &line.unwrapped_layout.runs[boundary.run_ix];
                row_starts.push(run.glyphs[boundary.glyph_ix].index);
            }
            row_starts.push(line.len() + 1);

            for row in 0..row_count {
                let row_begin = line_start + row_starts[row];
                let row_finish = line_start + row_starts[row + 1].min(line.len());
                let a = sel_start.max(row_begin);
                let b = sel_end.min(row_finish.max(row_begin));
                if a > b || (a == b && sel_end <= line_end) {
                    continue;
                }
                let within_a = (a - line_start).min(line.len());
                let within_b = (b - line_start).min(line.len());
                let x0 = x_in_row(line, row_starts[row], within_a);
                let x1 = x_in_row(line, row_starts[row], within_b);
                // A selection crossing the newline shows a stub past the end.
                let newline_stub = if sel_end > line_end && row == row_count - 1 {
                    px(6.)
                } else {
                    px(0.)
                };
                rows.push((
                    point(x0, y + layout.line_height * row as f32),
                    (x1 - x0) + newline_stub,
                ));
            }
        }
        y += line_px_height;
        if line_end + 1 > selection.end {
            break;
        }
    }
    rows
}

/// Turn highlight tokens into shaped-text runs.
fn text_runs(
    text: &str,
    base_font: &Font,
    theme: &Theme,
    resolver: &dyn LinkResolver,
    marked: &Option<Range<usize>>,
) -> Vec<TextRun> {
    let mono_family: SharedString = if cfg!(target_os = "windows") {
        "Consolas".into()
    } else if cfg!(target_os = "macos") {
        "Menlo".into()
    } else {
        "monospace".into()
    };

    let mut runs: Vec<TextRun> = Vec::new();
    for (range, token) in highlight::runs(text, resolver) {
        let mut font = base_font.clone();
        let mut colour = theme.text;
        let mut underline = None;
        let mut background = None;
        match token {
            Token::Text => {}
            Token::Heading => {
                font.weight = FontWeight::BOLD;
            }
            Token::Emphasis => font.style = gpui::FontStyle::Italic,
            Token::Strong => font.weight = FontWeight::BOLD,
            Token::Strikethrough => colour = theme.muted,
            Token::Blockquote => colour = theme.muted,
            Token::Code => {
                font.family = mono_family.clone();
                colour = theme.code_text;
                background = Some(theme.code_background.into());
            }
            Token::Link => colour = theme.link,
            Token::WikiLink => {
                colour = theme.link;
                underline = Some(gpui::UnderlineStyle {
                    color: Some(theme.link.into()),
                    thickness: px(1.),
                    wavy: false,
                });
            }
            Token::BrokenWikiLink => {
                colour = theme.broken_link;
                underline = Some(gpui::UnderlineStyle {
                    color: Some(theme.broken_link.into()),
                    thickness: px(1.),
                    wavy: true,
                });
            }
            Token::Tag => colour = theme.tag_manual,
            Token::Frontmatter => colour = theme.muted,
        }
        runs.push(TextRun {
            len: range.len(),
            font,
            color: colour.into(),
            background_color: background,
            underline,
            strikethrough: None,
        });
    }
    if runs.is_empty() && !text.is_empty() {
        runs.push(TextRun {
            len: text.len(),
            font: base_font.clone(),
            color: theme.text.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }

    // The IME composition range is underlined over whatever colour it has.
    if let Some(marked) = marked {
        apply_underline(&mut runs, marked, theme);
    }
    runs
}

/// Split runs so `range` carries an underline, preserving everything else.
fn apply_underline(runs: &mut Vec<TextRun>, range: &Range<usize>, theme: &Theme) {
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len() + 2);
    let mut at = 0usize;
    for run in runs.drain(..) {
        let run_range = at..at + run.len;
        at = run_range.end;
        let overlap = range.start.max(run_range.start)..range.end.min(run_range.end);
        if overlap.start >= overlap.end {
            out.push(run);
            continue;
        }
        let pieces = [
            run_range.start..overlap.start,
            overlap.clone(),
            overlap.end..run_range.end,
        ];
        for piece in pieces {
            if piece.start >= piece.end {
                continue;
            }
            let mut part = run.clone();
            part.len = piece.end - piece.start;
            if piece == overlap {
                part.underline = Some(gpui::UnderlineStyle {
                    color: Some(theme.accent.into()),
                    thickness: px(1.),
                    wavy: false,
                });
            }
            out.push(part);
        }
    }
    *runs = out;
}
