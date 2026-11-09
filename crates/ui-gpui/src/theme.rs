//! Colours and metrics, in light and dark.
//!
//! Requirements: UI-004 (support dark and light appearance), UI-010
//! (provenance indicators must be visible), UI-009 (legible contrast).
//!
//! One palette type with two constructors, rather than colours sprinkled
//! through the views: a theme that lives in one place can be switched at
//! runtime and checked for contrast, and a view that reaches for `rgb(0x…)`
//! directly is a view that will look wrong in the other appearance.

use gpui::{rgb, Hsla, Rgba};

/// Which appearance is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// Every colour the interface uses.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub appearance: Appearance,

    /// Window background, behind everything.
    pub background: Rgba,
    /// Panels that sit above the background: sidebar, headers, footers.
    pub surface: Rgba,
    /// A panel raised above `surface`, such as the command palette.
    pub elevated: Rgba,
    /// Hairlines between panels.
    pub border: Rgba,

    /// Primary reading text.
    pub text: Rgba,
    /// Secondary text: paths, counts, captions.
    pub muted: Rgba,
    /// Text on an accent background.
    pub on_accent: Rgba,

    /// Selection and focus.
    pub accent: Rgba,
    /// A row under the pointer.
    pub hover: Rgba,
    /// The selected row.
    pub selected: Rgba,

    /// Search-match highlight behind text.
    pub highlight: Rgba,
    /// Inline code and code blocks.
    pub code_background: Rgba,
    pub code_text: Rgba,
    /// A `[[wiki link]]` that resolves.
    pub link: Rgba,
    /// A link that points at nothing yet.
    pub broken_link: Rgba,

    /// A tag the user applied or imported (UI-010).
    pub tag_manual: Rgba,
    /// A tag a model proposed and nobody has reviewed.
    pub tag_proposed: Rgba,
    /// A model proposal the user accepted.
    pub tag_confirmed: Rgba,
}

impl Theme {
    pub fn new(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    pub fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,
            background: rgb(0x16181a),
            surface: rgb(0x1e2123),
            elevated: rgb(0x272b2e),
            border: rgb(0x32373b),
            text: rgb(0xe4e6e8),
            muted: rgb(0x9aa2a8),
            on_accent: rgb(0xffffff),
            accent: rgb(0x3b82c4),
            hover: rgb(0x24282b),
            selected: rgb(0x2b4f73),
            highlight: rgb(0x6b5a1f),
            code_background: rgb(0x23282b),
            code_text: rgb(0xd3b58d),
            link: rgb(0x7bb3e0),
            broken_link: rgb(0xd08770),
            tag_manual: rgb(0x8fbf8f),
            tag_proposed: rgb(0xc9a227),
            tag_confirmed: rgb(0x7bb3e0),
        }
    }

    pub fn light() -> Self {
        Self {
            appearance: Appearance::Light,
            background: rgb(0xfdfdfc),
            surface: rgb(0xf2f1ee),
            elevated: rgb(0xffffff),
            border: rgb(0xdedcd6),
            text: rgb(0x1f2224),
            muted: rgb(0x60666b),
            on_accent: rgb(0xffffff),
            accent: rgb(0x2a6ba8),
            hover: rgb(0xe9e8e4),
            selected: rgb(0xcfe0f0),
            highlight: rgb(0xf5e6a8),
            code_background: rgb(0xeceae4),
            code_text: rgb(0x7a4b12),
            link: rgb(0x1f5e96),
            broken_link: rgb(0xa8422a),
            tag_manual: rgb(0x2f6b2f),
            tag_proposed: rgb(0x8a6d1a),
            tag_confirmed: rgb(0x1f5e96),
        }
    }

    /// The colour a tag chip is drawn in, from its provenance (UI-010).
    ///
    /// A reader must be able to tell at a glance which of their tags they
    /// chose and which a model chose for them, without hovering or clicking.
    pub fn tag_colour(&self, provenance: marius_model::Provenance) -> Rgba {
        use marius_model::Provenance::*;
        match provenance {
            Manual | Imported | Derived => self.tag_manual,
            AiGenerated => self.tag_proposed,
            AiConfirmed => self.tag_confirmed,
        }
    }
}

/// Relative luminance, per WCAG 2.1.
fn luminance(colour: Rgba) -> f32 {
    fn channel(c: f32) -> f32 {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
}

/// WCAG contrast ratio between two colours, from 1.0 to 21.0.
pub fn contrast_ratio(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Convert for the few GPUI APIs that want HSLA.
pub fn hsla(colour: Rgba) -> Hsla {
    colour.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG AA for body text.
    const AA_BODY: f32 = 4.5;
    /// WCAG AA for large text and non-text indicators.
    const AA_LARGE: f32 = 3.0;

    #[test]
    fn reading_text_meets_contrast_in_both_appearances() {
        // UI-009. Asserted rather than eyeballed, because a palette that is
        // merely "fine on my monitor" is how accessibility is lost.
        for theme in [Theme::dark(), Theme::light()] {
            let label = theme.appearance.label();
            for (name, background) in [
                ("background", theme.background),
                ("surface", theme.surface),
                ("elevated", theme.elevated),
            ] {
                let ratio = contrast_ratio(theme.text, background);
                assert!(
                    ratio >= AA_BODY,
                    "{label}: text on {name} is {ratio:.2}:1, below {AA_BODY}:1"
                );
            }
        }
    }

    #[test]
    fn secondary_text_is_still_legible() {
        for theme in [Theme::dark(), Theme::light()] {
            let ratio = contrast_ratio(theme.muted, theme.surface);
            assert!(
                ratio >= AA_LARGE,
                "{}: muted text is {ratio:.2}:1",
                theme.appearance.label()
            );
        }
    }

    #[test]
    fn text_stays_readable_on_a_selected_row() {
        // A selection that swallows its own label is a common and avoidable bug.
        for theme in [Theme::dark(), Theme::light()] {
            let ratio = contrast_ratio(theme.text, theme.selected);
            assert!(
                ratio >= AA_LARGE,
                "{}: selected row text is {ratio:.2}:1",
                theme.appearance.label()
            );
        }
    }

    #[test]
    fn a_search_highlight_does_not_hide_the_match() {
        for theme in [Theme::dark(), Theme::light()] {
            let ratio = contrast_ratio(theme.text, theme.highlight);
            assert!(
                ratio >= AA_LARGE,
                "{}: highlighted text is {ratio:.2}:1",
                theme.appearance.label()
            );
        }
    }

    #[test]
    fn provenance_colours_are_distinguishable_from_each_other() {
        // UI-010 fails if "yours" and "the model's" look alike.
        use marius_model::Provenance;
        for theme in [Theme::dark(), Theme::light()] {
            let manual = theme.tag_colour(Provenance::Manual);
            let proposed = theme.tag_colour(Provenance::AiGenerated);
            let confirmed = theme.tag_colour(Provenance::AiConfirmed);
            assert_ne!(manual, proposed);
            assert_ne!(manual, confirmed);
            assert_ne!(proposed, confirmed);
            for (name, colour) in [
                ("manual", manual),
                ("proposed", proposed),
                ("confirmed", confirmed),
            ] {
                let ratio = contrast_ratio(colour, theme.surface);
                assert!(
                    ratio >= AA_LARGE,
                    "{}: {name} tag is {ratio:.2}:1",
                    theme.appearance.label()
                );
            }
        }
    }

    #[test]
    fn a_broken_link_is_visibly_different_from_a_working_one() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_ne!(theme.link, theme.broken_link);
            assert!(contrast_ratio(theme.broken_link, theme.background) >= AA_LARGE);
        }
    }

    #[test]
    fn toggling_appearance_round_trips() {
        assert_eq!(Appearance::Dark.toggled(), Appearance::Light);
        assert_eq!(Appearance::Dark.toggled().toggled(), Appearance::Dark);
        assert_eq!(Theme::new(Appearance::Light).appearance, Appearance::Light);
    }

    #[test]
    fn contrast_ratio_is_calibrated() {
        // Black on white is the known maximum.
        let ratio = contrast_ratio(rgb(0x000000), rgb(0xffffff));
        assert!((ratio - 21.0).abs() < 0.1, "{ratio}");
        assert!((contrast_ratio(rgb(0x777777), rgb(0x777777)) - 1.0).abs() < 0.01);
    }
}
