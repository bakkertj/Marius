//! The editor's text model: content, cursor, selection and undo.
//!
//! Requirements: MD-001 (native Markdown editor), MD-004 (undo/redo), MD-013
//! (efficient keyboard editing).
//!
//! ## Why this is a separate, windowless module
//!
//! The GPUI layer cannot be driven by automated tests — nothing can type into
//! a window from a test harness — so correctness has to live where tests can
//! reach it. Every edit, motion, selection change and undo step is a method on
//! this plain struct, exhaustively tested below; the GPUI element above it
//! only maps key events to these methods and paints the result. When an editor
//! bug is found, the fix belongs here, with a test, not in the paint code.
//!
//! Offsets are byte offsets into UTF-8, always on a character boundary, and
//! cursor movement is by **grapheme cluster** — arrow keys must not split a
//! flag emoji or a combining accent.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// How far a horizontal motion reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// One grapheme left.
    Left,
    /// One grapheme right.
    Right,
    /// To the start of the previous word.
    WordLeft,
    /// To the end of the next word.
    WordRight,
    /// Start of the logical line.
    LineStart,
    /// End of the logical line.
    LineEnd,
    /// Start of the document.
    DocumentStart,
    /// End of the document.
    DocumentEnd,
}

/// What kind of edit last happened, for undo grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastEdit {
    Typing,
    Deleting,
    Other,
}

/// How an edit may merge into the previous undo entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coalesce {
    No,
    /// Typing directly after the previous insertion extends it.
    Append,
    /// Backspacing directly before the previous deletion extends it leftward.
    DeleteBack,
}

/// One reversible change: `deleted` was removed at `at`, `inserted` put in its
/// place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    at: usize,
    deleted: String,
    inserted: String,
    cursor_before: usize,
    anchor_before: Option<usize>,
}

/// A text buffer with a cursor, an optional selection, and undo history.
#[derive(Debug, Clone)]
pub struct Buffer {
    text: String,
    cursor: usize,
    /// Selection anchor. `Some` whenever a selection exists; the selection is
    /// the span between `anchor` and `cursor`, in either order.
    anchor: Option<usize>,
    undo: Vec<Change>,
    redo: Vec<Change>,
    last_edit: LastEdit,
    /// Bumped on every content change, so the element knows to re-shape and
    /// the app knows the buffer is dirty relative to `saved_version`.
    version: u64,
    saved_version: u64,
}

/// Undo history is capped so a long session cannot grow without bound.
const UNDO_LIMIT: usize = 1_000;

impl Buffer {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: 0,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: LastEdit::Other,
            version: 0,
            saved_version: 0,
        }
    }

    // ------------------------------------------------------------ reading

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// True when the content has changed since the last `mark_saved`.
    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    /// The selected range, empty at the cursor when nothing is selected.
    pub fn selection(&self) -> Range<usize> {
        match self.anchor {
            Some(anchor) if anchor <= self.cursor => anchor..self.cursor,
            Some(anchor) => self.cursor..anchor,
            None => self.cursor..self.cursor,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.is_some_and(|a| a != self.cursor)
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    /// Zero-based (line, column-in-bytes) of an offset.
    pub fn line_and_column(&self, offset: usize) -> (usize, usize) {
        let offset = self.clamp(offset);
        let before = &self.text[..offset];
        let line = before.matches('\n').count();
        let column = offset - before.rfind('\n').map_or(0, |i| i + 1);
        (line, column)
    }

    /// Byte range of the logical line containing `offset`, newline excluded.
    pub fn line_range(&self, offset: usize) -> Range<usize> {
        let offset = self.clamp(offset);
        let start = self.text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i);
        start..end
    }

    // ----------------------------------------------------------- movement

    /// Place the cursor, optionally extending the selection.
    ///
    /// The offset is clamped into the text and snapped to a character
    /// boundary, so a caller computing offsets from pixel positions cannot
    /// corrupt the buffer.
    pub fn set_cursor(&mut self, offset: usize, select: bool) {
        let offset = self.clamp(offset);
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = offset;
        self.last_edit = LastEdit::Other;
    }

    pub fn move_cursor(&mut self, motion: Motion, select: bool) {
        // A plain horizontal move with a selection active collapses to the
        // selection's edge, the way every editor behaves.
        if !select && self.has_selection() && matches!(motion, Motion::Left | Motion::Right) {
            let range = self.selection();
            let target = match motion {
                Motion::Left => range.start,
                _ => range.end,
            };
            self.set_cursor(target, false);
            return;
        }
        let target = self.target(motion, self.cursor);
        self.set_cursor(target, select);
    }

    fn target(&self, motion: Motion, from: usize) -> usize {
        match motion {
            Motion::Left => self.prev_grapheme(from),
            Motion::Right => self.next_grapheme(from),
            Motion::WordLeft => prev_word_boundary(&self.text, from),
            Motion::WordRight => next_word_boundary(&self.text, from),
            Motion::LineStart => self.line_range(from).start,
            Motion::LineEnd => self.line_range(from).end,
            Motion::DocumentStart => 0,
            Motion::DocumentEnd => self.text.len(),
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
        self.last_edit = LastEdit::Other;
    }

    /// Select the word at `offset` (a double-click).
    pub fn select_word_at(&mut self, offset: usize) {
        let offset = self.clamp(offset);
        let range = word_range(&self.text, offset);
        self.anchor = Some(range.start);
        self.cursor = range.end;
        self.last_edit = LastEdit::Other;
    }

    /// Select the logical line at `offset` (a triple-click), newline included
    /// so consecutive line selections abut.
    pub fn select_line_at(&mut self, offset: usize) {
        let mut range = self.line_range(offset);
        if range.end < self.text.len() {
            range.end += 1;
        }
        self.anchor = Some(range.start);
        self.cursor = range.end;
        self.last_edit = LastEdit::Other;
    }

    // ------------------------------------------------------------ editing

    /// Insert text at the cursor, replacing any selection.
    pub fn insert(&mut self, inserted: &str) {
        let range = self.selection();
        // Single-grapheme, non-whitespace insertions coalesce into the
        // previous typing change, so undo removes words rather than letters.
        let coalesce = self.last_edit == LastEdit::Typing
            && range.is_empty()
            && !inserted.contains(char::is_whitespace)
            && self
                .undo
                .last()
                .is_some_and(|c| c.at + c.inserted.len() == range.start && c.deleted.is_empty());

        self.apply(
            range,
            inserted,
            if coalesce {
                Coalesce::Append
            } else {
                Coalesce::No
            },
        );
        self.last_edit = if inserted.contains(char::is_whitespace) {
            LastEdit::Other
        } else {
            LastEdit::Typing
        };
    }

    /// Insert a newline, continuing a Markdown list, quote or task marker.
    ///
    /// Pressing return inside `- item` yields a new `- ` line; pressing it on
    /// a marker-only line removes the marker instead — the convention every
    /// Markdown editor shares, and the little things like this are what make
    /// an editor feel native to the format (MD-001).
    pub fn insert_newline(&mut self) {
        let line = self.line_range(self.cursor);
        let line_text = self.text[line.clone()].to_owned();
        let marker = list_marker(&line_text);

        match marker {
            Some(marker) if line_text.trim_end() == marker.trim_end() => {
                // Return on an empty item ends the list: remove the marker.
                let range = line.start..self.selection().end;
                self.apply(range, "\n", Coalesce::No);
            }
            Some(marker) if self.selection().is_empty() && self.cursor == line.end => {
                let continuation = format!("\n{}", next_marker(&marker));
                let range = self.selection();
                self.apply(range, &continuation, Coalesce::No);
            }
            _ => {
                let range = self.selection();
                self.apply(range, "\n", Coalesce::No);
            }
        }
        self.last_edit = LastEdit::Other;
    }

    /// Delete the selection, or one grapheme backwards.
    pub fn backspace(&mut self) {
        let range = if self.has_selection() {
            self.selection()
        } else {
            self.prev_grapheme(self.cursor)..self.cursor
        };
        if range.is_empty() {
            return;
        }
        let coalesce = self.last_edit == LastEdit::Deleting
            && self
                .undo
                .last()
                .is_some_and(|c| c.at == range.end && c.inserted.is_empty());
        self.apply(
            range,
            "",
            if coalesce {
                Coalesce::DeleteBack
            } else {
                Coalesce::No
            },
        );
        self.last_edit = LastEdit::Deleting;
    }

    /// Delete the selection, or one grapheme forwards.
    pub fn delete_forward(&mut self) {
        let range = if self.has_selection() {
            self.selection()
        } else {
            self.cursor..self.next_grapheme(self.cursor)
        };
        if range.is_empty() {
            return;
        }
        self.apply(range, "", Coalesce::No);
        self.last_edit = LastEdit::Other;
    }

    /// Delete to the start of the previous word (alt-backspace).
    pub fn delete_word_back(&mut self) {
        let range = if self.has_selection() {
            self.selection()
        } else {
            prev_word_boundary(&self.text, self.cursor)..self.cursor
        };
        if range.is_empty() {
            return;
        }
        self.apply(range, "", Coalesce::No);
        self.last_edit = LastEdit::Other;
    }

    /// Replace a range directly — the IME's path for marked text.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        let range = self.clamp(range.start)..self.clamp(range.end);
        self.apply(range, text, Coalesce::No);
        self.last_edit = LastEdit::Other;
    }

    // --------------------------------------------------------------- undo

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) {
        let Some(change) = self.undo.pop() else {
            return;
        };
        let end = change.at + change.inserted.len();
        self.text.replace_range(change.at..end, &change.deleted);
        self.cursor = change.cursor_before.min(self.text.len());
        self.anchor = change.anchor_before.map(|a| a.min(self.text.len()));
        self.redo.push(change);
        self.version += 1;
        self.last_edit = LastEdit::Other;
    }

    pub fn redo(&mut self) {
        let Some(change) = self.redo.pop() else {
            return;
        };
        let end = change.at + change.deleted.len();
        self.text.replace_range(change.at..end, &change.inserted);
        self.cursor = change.at + change.inserted.len();
        self.anchor = None;
        self.undo.push(change);
        self.version += 1;
        self.last_edit = LastEdit::Other;
    }

    // ----------------------------------------------------------- internal

    /// The one place the text actually changes.
    fn apply(&mut self, range: Range<usize>, inserted: &str, coalesce: Coalesce) {
        if range.is_empty() && inserted.is_empty() {
            return;
        }
        let deleted = self.text[range.clone()].to_owned();

        match (coalesce, self.undo.last_mut()) {
            (Coalesce::Append, Some(last)) => {
                last.inserted.push_str(inserted);
            }
            // A backward deletion grows the previous change leftwards: both
            // its position and its deleted text move.
            (Coalesce::DeleteBack, Some(last)) => {
                last.at = range.start;
                last.deleted = format!("{deleted}{}", last.deleted);
            }
            _ => {
                self.undo.push(Change {
                    at: range.start,
                    deleted,
                    inserted: inserted.to_owned(),
                    cursor_before: self.cursor,
                    anchor_before: self.anchor,
                });
                if self.undo.len() > UNDO_LIMIT {
                    self.undo.remove(0);
                }
            }
        }
        // A new edit invalidates the redo branch.
        self.redo.clear();

        self.text.replace_range(range.clone(), inserted);
        self.cursor = range.start + inserted.len();
        self.anchor = None;
        self.version += 1;
    }

    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while offset > 0 && !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn prev_grapheme(&self, from: usize) -> usize {
        self.text[..from]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_grapheme(&self, from: usize) -> usize {
        self.text[from..]
            .graphemes(true)
            .next()
            .map_or(self.text.len(), |g| from + g.len())
    }
}

/// Is this character part of a word?
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Start of the word before `from`: skip non-word characters backwards, then
/// the word itself.
fn prev_word_boundary(text: &str, from: usize) -> usize {
    let mut chars: Vec<(usize, char)> = text[..from].char_indices().collect();
    while let Some(&(_, ch)) = chars.last() {
        if is_word(ch) {
            break;
        }
        chars.pop();
    }
    while let Some(&(i, ch)) = chars.last() {
        if !is_word(ch) {
            return chars.last().map_or(0, |_| i + ch.len_utf8());
        }
        chars.pop();
    }
    chars.last().map_or(0, |&(i, ch)| i + ch.len_utf8())
}

/// End of the word after `from`: skip non-word characters, then the word.
fn next_word_boundary(text: &str, from: usize) -> usize {
    let mut iter = text[from..].char_indices().peekable();
    while let Some(&(_, ch)) = iter.peek() {
        if is_word(ch) {
            break;
        }
        iter.next();
    }
    while let Some(&(i, ch)) = iter.peek() {
        if !is_word(ch) {
            return from + i;
        }
        iter.next();
        if iter.peek().is_none() {
            return from + i + ch.len_utf8();
        }
    }
    text.len()
}

/// The word range containing `offset`, for double-click selection.
fn word_range(text: &str, offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let at = offset.min(text.len().saturating_sub(1));
    let ch = text[at..].chars().next().unwrap_or(' ');
    if !is_word(ch) {
        // Selecting on punctuation selects just that cluster.
        let end = text[at..]
            .grapheme_indices(true)
            .nth(1)
            .map_or(text.len(), |(i, _)| at + i);
        return at..end;
    }
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(at, |(i, c)| at + i + c.len_utf8());
    start..end
}

/// The Markdown list/quote/task marker opening a line, if any, including the
/// indentation before it and the space after it.
fn list_marker(line: &str) -> Option<String> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);

    for bullet in ["- [ ] ", "- [x] ", "- [X] ", "- ", "* ", "+ ", "> "] {
        if rest.starts_with(bullet) {
            return Some(format!("{indent}{bullet}"));
        }
    }
    // An ordered item: digits, a dot or paren, a space.
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let after = &rest[digits.len()..];
        for sep in [". ", ") "] {
            if after.starts_with(sep) {
                return Some(format!("{indent}{digits}{sep}"));
            }
        }
    }
    None
}

/// The marker for the *next* line: ordered items count up, a completed task
/// continues as an open one, everything else repeats.
fn next_marker(marker: &str) -> String {
    let trimmed = marker.trim_start();
    let indent = &marker[..marker.len() - trimmed.len()];
    if trimmed.starts_with("- [") {
        return format!("{indent}- [ ] ");
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        if let Ok(n) = digits.parse::<u64>() {
            let rest = &trimmed[digits.len()..];
            return format!("{indent}{}{rest}", n + 1);
        }
    }
    marker.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type a string one character at a time, as key events arrive.
    fn type_str(buffer: &mut Buffer, text: &str) {
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" {
                buffer.insert_newline();
            } else {
                buffer.insert(grapheme);
            }
        }
    }

    // ------------------------------------------------------------ editing

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut b = Buffer::new("");
        type_str(&mut b, "harbour");
        assert_eq!(b.text(), "harbour");
        assert_eq!(b.cursor(), 7);
        assert!(b.is_dirty());
    }

    #[test]
    fn typing_replaces_a_selection() {
        let mut b = Buffer::new("the harbour mouth");
        b.set_cursor(4, false);
        b.set_cursor(11, true);
        assert_eq!(b.selected_text(), "harbour");
        b.insert("river");
        assert_eq!(b.text(), "the river mouth");
        assert!(!b.has_selection());
        assert_eq!(b.cursor(), 9);
    }

    #[test]
    fn backspace_removes_one_grapheme_or_the_selection() {
        let mut b = Buffer::new("ab");
        b.move_cursor(Motion::DocumentEnd, false);
        b.backspace();
        assert_eq!(b.text(), "a");
        b.backspace();
        assert_eq!(b.text(), "");
        b.backspace(); // at the start: a no-op, not a panic
        assert_eq!(b.text(), "");

        let mut b = Buffer::new("the harbour mouth");
        b.set_cursor(4, false);
        b.set_cursor(12, true);
        b.backspace();
        assert_eq!(b.text(), "the mouth");
    }

    #[test]
    fn emoji_and_combining_marks_are_never_split() {
        // The flag is 8 bytes, the accented e is e + U+0301.
        let text = "a🇳🇱e\u{301}b";
        let mut b = Buffer::new(text);
        b.move_cursor(Motion::DocumentEnd, false);

        b.move_cursor(Motion::Left, false); // over b
        b.move_cursor(Motion::Left, false); // over e-acute as ONE unit
        let after_flag = 1 + "🇳🇱".len();
        assert_eq!(b.cursor(), after_flag);

        b.backspace(); // removes the whole flag
        assert_eq!(b.text(), "ae\u{301}b");

        // And clamping a mid-character offset snaps to a boundary.
        let mut b = Buffer::new("🇳🇱");
        b.set_cursor(3, false);
        assert!(b.text().is_char_boundary(b.cursor()));
    }

    #[test]
    fn delete_forward_mirrors_backspace() {
        let mut b = Buffer::new("ab");
        b.delete_forward();
        assert_eq!(b.text(), "b");
        b.move_cursor(Motion::DocumentEnd, false);
        b.delete_forward(); // at the end: no-op
        assert_eq!(b.text(), "b");
    }

    #[test]
    fn delete_word_back_takes_the_word_and_trailing_separators() {
        let mut b = Buffer::new("the harbour  ");
        b.move_cursor(Motion::DocumentEnd, false);
        b.delete_word_back();
        assert_eq!(b.text(), "the ");
        b.delete_word_back();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn ime_replacement_edits_an_arbitrary_range() {
        let mut b = Buffer::new("hello world");
        b.replace_range(0..5, "goodbye");
        assert_eq!(b.text(), "goodbye world");
        assert_eq!(b.cursor(), 7);
        // Out-of-range input from the platform is clamped, not trusted.
        b.replace_range(500..900, "!");
        assert_eq!(b.text(), "goodbye world!");
    }

    // ----------------------------------------------------------- movement

    #[test]
    fn word_motion_crosses_punctuation_and_unicode() {
        let text = "see [[Pharos]], l\u{e9}on!";
        let mut b = Buffer::new(text);
        b.move_cursor(Motion::WordRight, false);
        assert_eq!(&text[..b.cursor()], "see");
        b.move_cursor(Motion::WordRight, false);
        assert_eq!(&text[..b.cursor()], "see [[Pharos");
        b.move_cursor(Motion::WordRight, false);
        assert_eq!(&text[..b.cursor()], "see [[Pharos]], l\u{e9}on");

        b.move_cursor(Motion::WordLeft, false);
        assert_eq!(b.text()[b.cursor()..].chars().next(), Some('l'));
        b.move_cursor(Motion::WordLeft, false);
        assert_eq!(b.text()[b.cursor()..].chars().next(), Some('P'));
        b.move_cursor(Motion::WordLeft, false);
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn line_start_and_end_are_logical_lines() {
        let mut b = Buffer::new("first line\nsecond line\nthird");
        b.set_cursor(18, false); // inside "second"
        b.move_cursor(Motion::LineStart, false);
        assert_eq!(b.cursor(), 11);
        b.move_cursor(Motion::LineEnd, false);
        assert_eq!(b.cursor(), 22);
        assert_eq!(b.line_and_column(b.cursor()), (1, 11));
    }

    #[test]
    fn a_plain_arrow_collapses_the_selection_to_its_edge() {
        let mut b = Buffer::new("abcdef");
        b.set_cursor(1, false);
        b.set_cursor(4, true);
        b.move_cursor(Motion::Left, false);
        assert_eq!(b.cursor(), 1, "left goes to the selection start");
        assert!(!b.has_selection());

        b.set_cursor(4, true);
        b.move_cursor(Motion::Right, false);
        assert_eq!(b.cursor(), 4, "right goes to the selection end");
    }

    #[test]
    fn shift_motion_extends_and_reverses_a_selection() {
        let mut b = Buffer::new("abcdef");
        b.set_cursor(3, false);
        b.move_cursor(Motion::Right, true);
        b.move_cursor(Motion::Right, true);
        assert_eq!(b.selected_text(), "de");
        // Reversing through the anchor flips the selection direction.
        for _ in 0..4 {
            b.move_cursor(Motion::Left, true);
        }
        assert_eq!(b.selected_text(), "bc");
        assert_eq!(b.selection(), 1..3);
    }

    #[test]
    fn double_click_selects_a_word_and_triple_click_a_line() {
        let mut b = Buffer::new("the harbour mouth\nsecond line");
        b.select_word_at(6);
        assert_eq!(b.selected_text(), "harbour");
        b.select_word_at(3); // on the space: just that cluster
        assert_eq!(b.selected_text(), " ");

        b.select_line_at(6);
        assert_eq!(b.selected_text(), "the harbour mouth\n");
        b.select_line_at(20);
        assert_eq!(b.selected_text(), "second line");
    }

    #[test]
    fn select_all_covers_everything() {
        let mut b = Buffer::new("ab\ncd");
        b.select_all();
        assert_eq!(b.selected_text(), "ab\ncd");
    }

    // --------------------------------------------------------------- undo

    #[test]
    fn undo_reverses_and_redo_replays() {
        let mut b = Buffer::new("start");
        b.move_cursor(Motion::DocumentEnd, false);
        b.insert(" end");
        assert_eq!(b.text(), "start end");
        b.undo();
        assert_eq!(b.text(), "start");
        assert_eq!(b.cursor(), 5, "undo restores the cursor");
        b.redo();
        assert_eq!(b.text(), "start end");
        assert!(!b.can_redo());
    }

    #[test]
    fn typed_words_undo_as_units_not_letters() {
        // The coalescing rule: whitespace starts a new undo group.
        let mut b = Buffer::new("");
        type_str(&mut b, "the harbour");
        b.undo();
        assert_eq!(b.text(), "the ", "one undo removes the whole word");
        b.undo();
        assert_eq!(b.text(), "the");
        b.undo();
        assert_eq!(b.text(), "");
        assert!(!b.can_undo());
    }

    #[test]
    fn consecutive_backspaces_undo_as_one_group() {
        let mut b = Buffer::new("harbour");
        b.move_cursor(Motion::DocumentEnd, false);
        for _ in 0..3 {
            b.backspace();
        }
        assert_eq!(b.text(), "harb");
        b.undo();
        assert_eq!(b.text(), "harbour", "the run of deletes reverses together");
    }

    #[test]
    fn moving_the_cursor_breaks_undo_coalescing() {
        let mut b = Buffer::new("");
        type_str(&mut b, "ab");
        b.move_cursor(Motion::Left, false);
        b.move_cursor(Motion::Right, false);
        type_str(&mut b, "cd");
        b.undo();
        assert_eq!(b.text(), "ab", "the second burst undoes alone");
    }

    #[test]
    fn a_new_edit_discards_the_redo_branch() {
        let mut b = Buffer::new("");
        type_str(&mut b, "one");
        b.undo();
        type_str(&mut b, "two");
        assert!(!b.can_redo(), "redo after a divergent edit would corrupt");
        assert_eq!(b.text(), "two");
    }

    #[test]
    fn undo_restores_a_replaced_selection_and_its_bounds() {
        let mut b = Buffer::new("the harbour mouth");
        b.set_cursor(4, false);
        b.set_cursor(11, true);
        b.insert("X");
        assert_eq!(b.text(), "the X mouth");
        b.undo();
        assert_eq!(b.text(), "the harbour mouth");
        assert_eq!(b.selection(), 4..11, "the selection comes back too");
    }

    #[test]
    fn dirty_tracking_follows_undo_across_saves() {
        let mut b = Buffer::new("x");
        assert!(!b.is_dirty());
        b.insert("y");
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
        b.undo();
        assert!(b.is_dirty(), "undoing past a save point is a change");
    }

    // --------------------------------------------- markdown-aware newline

    #[test]
    fn return_continues_a_bullet_list() {
        let mut b = Buffer::new("- first");
        b.move_cursor(Motion::DocumentEnd, false);
        b.insert_newline();
        type_str(&mut b, "second");
        assert_eq!(b.text(), "- first\n- second");
    }

    #[test]
    fn return_continues_quotes_tasks_and_numbers() {
        for (line, next) in [
            ("> quoted", "> "),
            ("- [x] done", "- [ ] "),
            ("3. third", "4. "),
            ("  - indented", "  - "),
        ] {
            let mut b = Buffer::new(line);
            b.move_cursor(Motion::DocumentEnd, false);
            b.insert_newline();
            assert_eq!(b.text(), format!("{line}\n{next}"), "after {line:?}");
        }
    }

    #[test]
    fn return_on_an_empty_item_ends_the_list() {
        let mut b = Buffer::new("- first\n- ");
        b.move_cursor(Motion::DocumentEnd, false);
        b.insert_newline();
        assert_eq!(b.text(), "- first\n\n", "the empty marker is removed");
    }

    #[test]
    fn return_mid_line_does_not_invent_a_marker() {
        let mut b = Buffer::new("- some item");
        b.set_cursor(6, false);
        b.insert_newline();
        assert_eq!(b.text(), "- some\n item");
    }

    #[test]
    fn return_on_a_plain_line_is_just_a_newline() {
        let mut b = Buffer::new("plain text");
        b.move_cursor(Motion::DocumentEnd, false);
        b.insert_newline();
        assert_eq!(b.text(), "plain text\n");
    }

    #[test]
    fn a_dash_mid_sentence_is_not_a_list_marker() {
        let mut b = Buffer::new("well - that");
        b.move_cursor(Motion::DocumentEnd, false);
        b.insert_newline();
        assert_eq!(b.text(), "well - that\n");
    }

    // ------------------------------------------------------------- safety

    #[test]
    fn every_operation_survives_an_empty_buffer() {
        let mut b = Buffer::new("");
        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::WordLeft,
            Motion::WordRight,
            Motion::LineStart,
            Motion::LineEnd,
            Motion::DocumentStart,
            Motion::DocumentEnd,
        ] {
            b.move_cursor(motion, false);
            b.move_cursor(motion, true);
        }
        b.backspace();
        b.delete_forward();
        b.delete_word_back();
        b.select_all();
        b.select_word_at(0);
        b.select_line_at(0);
        b.undo();
        b.redo();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn undo_history_is_bounded() {
        let mut b = Buffer::new("");
        for i in 0..1_200 {
            b.insert(&format!(" {i}")); // whitespace breaks coalescing
        }
        let mut undos = 0;
        while b.can_undo() {
            b.undo();
            undos += 1;
            assert!(undos <= 2_500, "undo stack failed to terminate");
        }
        assert_eq!(undos, 1_000, "history is capped at the limit");
        // The edits that fell off the end of the stack can no longer be
        // reversed, so the earliest text is what remains — permanence of the
        // oldest, not the newest, is what a capped history means.
        assert!(b.text().starts_with(" 0 1 2"), "{:?}", &b.text()[..20]);
        assert!(!b.text().contains(" 1100"));
    }
}
