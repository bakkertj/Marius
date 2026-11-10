//! A hover tooltip that shows a status line's full text — the footer
//! truncates long errors, and a message you cannot read is no message.

use gpui::{div, prelude::*, px, AnyView, App, Context, SharedString, Window};

use crate::theme::Theme;

pub struct TextTip {
    text: SharedString,
    theme: Theme,
}

impl TextTip {
    /// Build the tooltip constructor an element's `.tooltip(...)` wants.
    pub fn build(
        text: SharedString,
        theme: Theme,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        move |_window, cx| {
            let text = text.clone();
            cx.new(|_| TextTip { text, theme }).into()
        }
    }
}

impl Render for TextTip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .max_w(px(560.))
            .p(px(10.))
            .rounded(px(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .shadow_md()
            .text_size(px(12.))
            .text_color(theme.text)
            .child(self.text.clone())
    }
}
