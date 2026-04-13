use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::browser::common::Browser;

/// Render a "session ended — Reconnect / Close pane" overlay on top of a browser
/// pane when its underlying PTY has exited.
pub fn render_browser_exit_overlay(browser: &dyn Browser, area: Rect, buf: &mut Buffer) {
    if !browser.process_exited() {
        return;
    }
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
        let style = if i as u8 == browser.core().exit_selection {
            sel
        } else {
            dim
        };
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
