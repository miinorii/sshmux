//! Render-only ratatui widgets.
//!
//! # Conventions
//!
//! - Widgets are cheap structs built fresh each frame, holding `&'a` config
//!   (titles, focus flags, bindings) — they never own application state.
//! - Interactive components implement [`ratatui::widgets::StatefulWidget`]
//!   with `State` = the existing state struct (`BrowserCore`,
//!   `EmbeddedTerminal`, …); there is no separate view-model layer.
//! - Widgets never write to a PTY and never mutate state, with one framework
//!   exception: ratatui's own scroll-clamp idiom (`ListState` offsets,
//!   `scroll_x` clamping) is allowed — that is what `StatefulWidget` is for.
//! - Event handling and keybindings live in `input.rs` and the state
//!   modules. When a widget needs a decision, it takes a precomputed config
//!   field instead of making the decision itself.

pub mod bottom_bar;
pub mod terminal;

/// Test support: buffer snapshotting shared by golden-frame tests.
#[cfg(test)]
pub mod testing {
    use ratatui::buffer::Buffer;

    /// Render a buffer as one string per row (symbols only, trailing
    /// whitespace trimmed) for golden-frame comparisons.
    pub fn buffer_rows(buf: &Buffer) -> Vec<String> {
        let area = *buf.area();
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Assert that a buffer's rows match `expected` exactly.
    pub fn assert_rows(buf: &Buffer, expected: &[&str]) {
        let actual = buffer_rows(buf);
        let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual, expected,
            "\n--- actual rows ---\n{:#?}\n--- expected rows ---\n{:#?}",
            actual, expected
        );
    }
}
