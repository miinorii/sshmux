//! Feature components. Each folder colocates a component's state
//! (`state.rs` / `session.rs` / `tree.rs`) with its render-only view
//! (`view.rs`); simple render-only components are single files
//! (`bottom_bar`, `overlays`).
//!
//! # View conventions (enforced by `views_are_render_only`)
//!
//! - Views are cheap structs built fresh each frame, holding `&'a` config
//!   (titles, focus flags, bindings) — they never own application state.
//! - Interactive views implement [`ratatui::widgets::StatefulWidget`]
//!   with `State` = the component's state struct (`BrowserCore`,
//!   `EmbeddedTerminal`, …); there is no separate view-model layer.
//! - Views never write to a PTY, never handle events, and never mutate
//!   state, with one framework exception: ratatui's own scroll-clamp idiom
//!   (`ListState` offsets, `scroll_x` clamping) is allowed — that is what
//!   `StatefulWidget` is for.
//! - Event handling and keybindings live in `input.rs` and the state
//!   modules. When a view needs a decision, it takes a precomputed config
//!   field instead of making the decision itself.

pub mod bottom_bar;
pub mod browser;
pub mod connect;
pub mod overlays;
pub mod pane_tree;
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

#[cfg(test)]
mod render_only_guard {
    /// Views must stay render-only: no PTY writes, no event handling, no
    /// process-state checks. Colocating state and view in one folder removed
    /// the old `widgets/` directory boundary; this test replaces it.
    #[test]
    fn views_are_render_only() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/components");
        let views = [
            "bottom_bar.rs",
            "overlays.rs",
            "browser/view.rs",
            "connect/view.rs",
            "pane_tree/view.rs",
            "terminal/view.rs",
        ];
        let forbidden = [
            "send_str",
            "send_char",
            "drain_raw",
            "process_exited",
            "KeyCode",
            "MouseEventKind",
        ];
        for view in views {
            let path = format!("{root}/{view}");
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
            for token in forbidden {
                assert!(
                    !src.contains(token),
                    "{view} references `{token}` — views must stay render-only"
                );
            }
        }
    }
}
