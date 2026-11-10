//! HTML tables → pipe tables (IM-008).
//!
//! htmd's own table handler misses the most common shape in the wild:
//! html5ever wraps bare `<tr>`s in an implicit `<tbody>`, and the
//! default handler never promotes a tbody's first row to a header — so
//! an Outlook email's table came out as pipe-separated lines with **no
//! delimiter row**, which no Markdown parser reads as a table. This
//! handler walks the DOM itself and always emits a complete table.

use markup5ever_rcdom::{Node, NodeData};
use std::rc::Rc;

/// Replacement for htmd's `table` handler (registered last, so it wins).
pub(crate) fn table_handler(element: htmd::Element) -> Option<String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut caption = String::new();
    collect_rows(element.node, &mut rows, &mut caption);
    rows.retain(|row| !row.is_empty());
    if rows.is_empty() {
        // Not really tabular (a layout table with no cells): keep the
        // already-converted content rather than eating it.
        return Some(format!("\n\n{}\n\n", element.content.trim()));
    }

    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = String::from("\n\n");
    if !caption.trim().is_empty() {
        out.push_str(caption.trim());
        out.push_str("\n\n");
    }
    for (index, row) in rows.iter().enumerate() {
        out.push('|');
        for column in 0..columns {
            out.push(' ');
            out.push_str(row.get(column).map(String::as_str).unwrap_or(""));
            out.push_str(" |");
        }
        out.push('\n');
        if index == 0 {
            // The delimiter row is what makes it a table at all.
            out.push('|');
            for _ in 0..columns {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.push('\n');
    Some(out)
}

/// Rows from a table node: direct `<tr>`s and those under
/// `thead`/`tbody`/`tfoot` (html5ever inserts `tbody` even when the
/// source had none). A `caption` is kept as a leading line.
fn collect_rows(node: &Rc<Node>, rows: &mut Vec<Vec<String>>, caption: &mut String) {
    for child in node.children.borrow().iter() {
        let NodeData::Element { name, .. } = &child.data else {
            continue;
        };
        match name.local.as_ref() {
            "caption" => caption.push_str(&cell_text(child)),
            "thead" | "tbody" | "tfoot" => collect_rows(child, rows, caption),
            "tr" => {
                let mut cells = Vec::new();
                for cell in child.children.borrow().iter() {
                    if let NodeData::Element { name, .. } = &cell.data {
                        if matches!(name.local.as_ref(), "td" | "th") {
                            cells.push(cell_text(cell));
                        }
                    }
                }
                rows.push(cells);
            }
            _ => {}
        }
    }
}

/// A cell's visible text on one line: block boundaries and `<br>` become
/// spaces, whitespace collapses, and `|` is escaped so the cell cannot
/// break out of its column.
fn cell_text(node: &Rc<Node>) -> String {
    let mut raw = String::new();
    gather_text(node, &mut raw);
    let mut out = String::with_capacity(raw.len());
    let mut last_space = true;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else if ch == '|' {
            out.push_str("\\|");
            last_space = false;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

fn gather_text(node: &Rc<Node>, out: &mut String) {
    match &node.data {
        NodeData::Text { contents } => out.push_str(&contents.borrow()),
        NodeData::Element { name, .. } => {
            if name.local.as_ref() == "br" {
                out.push(' ');
            }
            for child in node.children.borrow().iter() {
                gather_text(child, out);
            }
            if matches!(name.local.as_ref(), "p" | "div" | "li" | "tr") {
                out.push(' ');
            }
        }
        _ => {
            for child in node.children.borrow().iter() {
                gather_text(child, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_outlook_table_becomes_a_real_pipe_table() {
        // Bare <tr>s (html5ever adds the tbody), every cell wrapped in a
        // <p class=MsoNormal> — the exact shape Outlook writes.
        let html = r#"<table border=1>
          <tr><td><p class=MsoNormal>Part</p></td><td><p class=MsoNormal>Qty</p></td></tr>
          <tr><td><p class=MsoNormal>VMF-5LCU</p></td><td><p>4</p></td></tr>
          <tr><td><p>Switch<br>spare</p></td><td><p>2 | 3</p></td></tr>
        </table>"#;
        let markdown = crate::html_to_markdown(html).unwrap();
        let lines: Vec<&str> = markdown.trim().lines().collect();
        assert_eq!(lines[0], "| Part | Qty |", "{markdown}");
        assert_eq!(lines[1], "| --- | --- |", "{markdown}");
        assert_eq!(lines[2], "| VMF-5LCU | 4 |");
        assert_eq!(lines[3], "| Switch spare | 2 \\| 3 |");
    }

    #[test]
    fn thead_and_caption_are_honoured_and_ragged_rows_are_padded() {
        let html = r#"<table><caption>Depths</caption>
          <thead><tr><th>Week</th><th>Depth</th></tr></thead>
          <tbody><tr><td>1</td></tr></tbody></table>"#;
        let markdown = crate::html_to_markdown(html).unwrap();
        let text = markdown.trim();
        assert!(text.starts_with("Depths"), "{text}");
        assert!(text.contains("| Week | Depth |"), "{text}");
        assert!(text.contains("| 1 |  |"), "{text}");
    }
}
