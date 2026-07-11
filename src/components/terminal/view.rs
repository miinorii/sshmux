//! vt100 screen-grid rendering for SSH session panes.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use vt100::Screen;

use super::session::EmbeddedTerminal;

/// Map a `vt100::Color` to a ratatui `Color`.
pub(crate) fn vc(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        vt100::Color::Idx(i) => Color::Indexed(i),
        _ => Color::Reset,
    }
}

/// Render a vt100 screen grid into `area`, mirroring per-cell styles. When
/// `show_cursor` is true (i.e. the view is live, not scrolled back) and the
/// screen's cursor is visible, the cursor cell is drawn reversed.
pub fn render_screen(screen: &Screen, show_cursor: bool, area: Rect, buf: &mut Buffer) {
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = screen.cell(y, x) {
                let s = cell.contents();
                let sym = if s.is_empty() { " " } else { s };
                if let Some(bc) = buf.cell_mut((area.x + x, area.y + y)) {
                    bc.set_symbol(sym);
                    let mut style = Style::default()
                        .fg(vc(cell.fgcolor()))
                        .bg(vc(cell.bgcolor()));
                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.dim() {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    bc.set_style(style);
                }
            }
        }
    }

    if show_cursor && !screen.hide_cursor() {
        let (cy, cx) = screen.cursor_position();
        let sx = area.x + cx;
        let sy = area.y + cy;
        if sx < area.x + area.width
            && sy < area.y + area.height
            && let Some(bc) = buf.cell_mut((sx, sy))
        {
            let style = bc.style().add_modifier(Modifier::REVERSED);
            bc.set_style(style);
        }
    }
}

/// SSH session pane content: the emulated terminal's screen grid.
pub struct TerminalView;

impl StatefulWidget for TerminalView {
    type State = EmbeddedTerminal;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        state.render_into(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::testing::assert_rows;

    fn render(parser: &vt100::Parser, show_cursor: bool, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render_screen(parser.screen(), show_cursor, area, &mut buf);
        buf
    }

    #[test]
    fn golden_plain_text_grid() {
        let mut parser = vt100::Parser::new(4, 10, 0);
        parser.process(b"hi\r\nworld");
        let buf = render(&parser, false, 10, 4);
        assert_rows(&buf, &["hi", "world", "", ""]);
    }

    #[test]
    fn cursor_cell_is_reversed_when_live() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"ab");
        let buf = render(&parser, true, 10, 2);
        // Cursor sits after 'b' at column 2.
        assert!(
            buf[(2u16, 0u16)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "cursor cell must be reversed"
        );
        assert!(
            !buf[(1u16, 0u16)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "non-cursor cells untouched"
        );
    }

    #[test]
    fn cursor_hidden_during_scrollback() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"ab");
        let buf = render(&parser, false, 10, 2);
        assert!(
            !buf[(2u16, 0u16)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "no reversed cursor while scrolled back"
        );
    }

    #[test]
    fn colors_and_attributes_map_to_ratatui() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"\x1b[1;31mR\x1b[0m\x1b[44mB");
        let buf = render(&parser, false, 10, 2);
        let r = buf[(0u16, 0u16)].style();
        assert_eq!(r.fg, Some(Color::Indexed(1)));
        assert!(r.add_modifier.contains(Modifier::BOLD));
        let b = buf[(1u16, 0u16)].style();
        assert_eq!(b.bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn vc_all_indexed_stay_indexed() {
        // vc() always returns Color::Indexed for Idx — the ColorBackend
        // handles the basic-ANSI vs 256-colour distinction at draw time.
        assert_eq!(vc(vt100::Color::Idx(0)), Color::Indexed(0));
        assert_eq!(vc(vt100::Color::Idx(4)), Color::Indexed(4));
        assert_eq!(vc(vt100::Color::Idx(15)), Color::Indexed(15));
        assert_eq!(vc(vt100::Color::Idx(16)), Color::Indexed(16));
        assert_eq!(vc(vt100::Color::Idx(231)), Color::Indexed(231));
        assert_eq!(vc(vt100::Color::Idx(255)), Color::Indexed(255));
    }

    #[test]
    fn vc_rgb_passthrough() {
        assert_eq!(vc(vt100::Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        assert_eq!(
            vc(vt100::Color::Rgb(255, 255, 255)),
            Color::Rgb(255, 255, 255)
        );
    }

    #[test]
    fn vc_default_is_reset() {
        assert_eq!(vc(vt100::Color::Default), Color::Reset);
    }
}
