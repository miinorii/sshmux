use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::terminal::EmbeddedTerminal;

use super::render_pane_border;

/// Render a Session pane: terminal content + exit overlay when the process has ended.
pub fn render_session(
    area: Rect,
    buf: &mut Buffer,
    terminal: &mut EmbeddedTerminal,
    ssh_args: &str,
    exit_selection: u8,
    is_focus: bool,
    leaf_count: usize,
) {
    let host = ssh_args.split_whitespace().last().unwrap_or("ssh");
    let inner = render_pane_border(area, buf, is_focus, leaf_count, host);
    terminal.render_into(inner, buf);

    if terminal.process_exited() {
        render_session_exit_overlay(inner, buf, exit_selection);
    }
}

/// "session ended — Reconnect / Close pane" overlay.
fn render_session_exit_overlay(area: Rect, buf: &mut Buffer, exit_selection: u8) {
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
        let style = if i as u8 == exit_selection { sel } else { dim };
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
