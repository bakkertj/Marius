//! Templates and daily notes (MD-011).
//!
//! A template is an ordinary Markdown file in the vault's templates
//! folder; nothing proprietary. Expansion knows three placeholders —
//! `{{date}}`, `{{time}}`, `{{title}}` — because three cover the daily
//! journal and the meeting note, and anything cleverer belongs in a
//! plugin.

use time::OffsetDateTime;

/// Fill a template's placeholders.
pub fn expand(template: &str, title: &str, now: OffsetDateTime) -> String {
    let date = format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        now.month() as u8,
        now.day()
    );
    let time = format!("{:02}:{:02}", now.hour(), now.minute());
    template
        .replace("{{date}}", &date)
        .replace("{{time}}", &time)
        .replace("{{title}}", title)
}

/// Today's daily-note file name.
pub fn daily_name(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}.md",
        now.year(),
        now.month() as u8,
        now.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn placeholders_expand_and_unknown_ones_survive() {
        let now = datetime!(2026-11-10 14:05 UTC);
        let out = expand(
            "# {{title}}\n\n{{date}} at {{time}} — {{weather}}\n",
            "Harbour walk",
            now,
        );
        assert_eq!(out, "# Harbour walk\n\n2026-11-10 at 14:05 — {{weather}}\n");
        assert_eq!(daily_name(now), "2026-11-10.md");
    }
}
