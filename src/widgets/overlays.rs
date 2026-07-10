//! Small overlay widgets: exit menu and right-click context menu.

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

// ---------------------------------------------------------------------------
// Exit overlay ("session ended — Reconnect / Close pane")
// ---------------------------------------------------------------------------

/// Centered Reconnect / Close pane menu drawn over an exited pane.
pub struct ExitOverlay {
    /// 0 = Reconnect, 1 = Close pane.
    pub selection: u8,
}

impl Widget for ExitOverlay {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let menu_w = 34u16.min(area.width.saturating_sub(2));
        let menu_h = 3u16;
        let cx = area.x + area.width.saturating_sub(menu_w) / 2;
        let cy = area.y + area.height.saturating_sub(menu_h) / 2;
        let menu_area = Rect {
            x: cx,
            y: cy,
            width: menu_w,
            height: menu_h,
        };
        let sel = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let items = ["Reconnect", "Close pane"];
        let mut spans = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" / ").style(dim));
            }
            let style = if i as u8 == self.selection { sel } else { dim };
            spans.push(Span::raw(*item).style(style));
        }
        for y in menu_area.y..menu_area.y + menu_area.height {
            for x in menu_area.x..menu_area.x + menu_area.width {
                buf[(x, y)].reset();
            }
        }
        let paragraph = Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" session ended "),
            );
        paragraph.render(menu_area, buf);
    }
}

// ---------------------------------------------------------------------------
// Right-click context menu
// ---------------------------------------------------------------------------

pub const CONTEXT_MENU_ITEMS: [&str; 5] = [
    "New tab",
    "Close tab",
    "Split left/right",
    "Split top/bottom",
    "Exit",
];
const CONTEXT_MENU_WIDTH: u16 = 22; // longest item (18) + 2 padding + 2 border
const CONTEXT_MENU_HEIGHT: u16 = 7; // 5 items + 2 border

/// Compute the screen rectangle for the context menu, clamped to `screen`.
/// The origin (col, row) is placed at the top-center of the menu.
pub fn context_menu_rect(col: u16, row: u16, screen: Rect) -> Rect {
    let w = CONTEXT_MENU_WIDTH;
    let h = CONTEXT_MENU_HEIGHT;
    let x = (col as i32 - w as i32 / 2).max(screen.x as i32);
    let x = (x as u16).min(screen.x + screen.width.saturating_sub(w));
    let y = row
        .max(screen.y)
        .min(screen.y + screen.height.saturating_sub(h));
    Rect::new(x, y, w, h)
}

/// The context menu itself; render into the rect from [`context_menu_rect`].
pub struct ContextMenuView {
    pub selected: Option<usize>,
}

impl Widget for ContextMenuView {
    fn render(self, rect: Rect, buf: &mut Buffer) {
        // Clear background
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                buf[(x, y)].reset();
            }
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        block.render(rect, buf);
        for (i, item) in CONTEXT_MENU_ITEMS.iter().enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let style = if self.selected == Some(i) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let w = inner.width as usize;
            let pad = w.saturating_sub(item.len()) / 2;
            let label = format!("{:>pad$}{:<rest$}", "", item, pad = pad, rest = w - pad);
            let span = Span::styled(label, style);
            buf.set_line(inner.x, y, &Line::from(span), inner.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::testing::assert_rows;

    #[test]
    fn golden_exit_overlay() {
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        ExitOverlay { selection: 0 }.render(area, &mut buf);
        assert_rows(
            &buf,
            &[
                "",
                "   ┌ session ended ─────────────────┐",
                "   │     Reconnect / Close pane     │",
                "   └────────────────────────────────┘",
                "",
            ],
        );
    }

    #[test]
    fn context_menu_rect_clamps_to_screen() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(40, 10, screen);
        assert_eq!(r.width, CONTEXT_MENU_WIDTH);
        assert_eq!(r.height, CONTEXT_MENU_HEIGHT);
        assert_eq!(r.x, 40 - CONTEXT_MENU_WIDTH / 2);
        assert_eq!(r.y, 10);
        assert_eq!(context_menu_rect(2, 10, screen).x, 0);
        let r = context_menu_rect(78, 22, screen);
        assert!(r.x + r.width <= screen.width);
        assert!(r.y + r.height <= screen.height);
        let r = context_menu_rect(0, 0, screen);
        assert_eq!((r.x, r.y), (0, 0));
    }
}
