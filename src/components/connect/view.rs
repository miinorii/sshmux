//! Connect pane rendering: host list + overlays (browser menu, manual
//! connect input, key editor).

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, Paragraph, StatefulWidget, Widget},
};

use super::state::{
    ConnectOverlay, ConnectPane, EDITOR_ROW_COUNT, HEADER_BROWSER, HEADER_CONNECT, HEADER_GLOBAL,
};
use crate::keybindings::KeyBindings;
use crate::ssh_config::SshHost;

/// Host list with the Connect pane's overlays; renders into the pane's
/// inner area (title bars are drawn by `PaneTreeView`).
pub struct ConnectView<'a> {
    pub hosts: &'a [SshHost],
    pub keybindings: &'a KeyBindings,
}

impl StatefulWidget for ConnectView<'_> {
    type State = ConnectPane;

    fn render(self, inner: Rect, buf: &mut Buffer, state: &mut ConnectPane) {
        let list_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };
        let hint_y = inner.y + inner.height.saturating_sub(1);

        let items: Vec<&str> = self.hosts.iter().map(|h| h.label.as_str()).collect();
        let list = List::new(items)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        StatefulWidget::render(list, list_area, buf, &mut state.list_state);

        let help_key = format!("  {}", self.keybindings.connect.help);
        buf.set_line(
            inner.x,
            hint_y,
            &Line::from(vec![
                Span::raw(help_key).style(Style::default().fg(Color::Yellow)),
                Span::raw(" keybindings").style(Style::default().fg(Color::DarkGray)),
            ]),
            inner.width,
        );

        match &mut state.overlay {
            ConnectOverlay::BrowserMenu(menu_state) => {
                let menu_w = 36u16;
                let menu_h = 4u16; // border + 2 items + border
                let cx = inner.x + inner.width.saturating_sub(menu_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(menu_h) / 2;
                let menu_area = Rect {
                    x: cx,
                    y: cy,
                    width: menu_w.min(inner.width),
                    height: menu_h.min(inner.height),
                };
                let menu_items = vec!["SFTP", "SCP (legacy, linux target)"];
                let menu_list = List::new(menu_items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" Browse with "),
                    )
                    .style(Style::default().fg(Color::White))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                StatefulWidget::render(menu_list, menu_area, buf, menu_state);
            }
            ConnectOverlay::KeyEditor(editor) => {
                let entries = self.keybindings.entries();
                let selected_display = editor.list_state.selected().unwrap_or(0);
                let groups = ["Global", "Connect", "Browser"];
                let header_indices = [HEADER_GLOBAL, HEADER_CONNECT, HEADER_BROWSER];

                let mut items: Vec<Line> = Vec::with_capacity(EDITOR_ROW_COUNT);
                let mut entry_idx = 0usize;
                for row in 0..EDITOR_ROW_COUNT {
                    if let Some(gi) = header_indices.iter().position(|&h| h == row) {
                        let label = format!(" ── {} ", groups[gi]);
                        items.push(Line::from(Span::styled(
                            label,
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )));
                    } else {
                        let e = &entries[entry_idx];
                        entry_idx += 1;
                        let key_str = if editor.editing && row == selected_display {
                            "[press key...]".to_string()
                        } else {
                            e.binding.to_string()
                        };
                        let key_style = if editor.editing && row == selected_display {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Yellow)
                        };
                        items.push(Line::from(vec![
                            Span::styled(format!(" {:14}", key_str), key_style),
                            Span::styled(e.description, Style::default().fg(Color::DarkGray)),
                        ]));
                    }
                }

                let title = if editor.editing {
                    " press a key to bind "
                } else {
                    " keybindings (Enter to edit) "
                };

                let status_line = editor.status.as_deref().unwrap_or("");
                let extra_h = if status_line.is_empty() { 0u16 } else { 1 };

                let ed_w = 44u16.min(inner.width.saturating_sub(2));
                let ed_h = (EDITOR_ROW_COUNT as u16 + 2 + extra_h).min(inner.height);
                let cx = inner.x + inner.width.saturating_sub(ed_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(ed_h) / 2;
                let ed_area = Rect {
                    x: cx,
                    y: cy,
                    width: ed_w,
                    height: ed_h,
                };

                let ed_list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(title),
                    )
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                StatefulWidget::render(ed_list, ed_area, buf, &mut editor.list_state);

                if !status_line.is_empty() {
                    let status_y = ed_area.y + ed_area.height.saturating_sub(2);
                    buf.set_line(
                        ed_area.x + 2,
                        status_y,
                        &Line::from(Span::styled(status_line, Style::default().fg(Color::Green))),
                        ed_area.width.saturating_sub(4),
                    );
                }
            }
            ConnectOverlay::ConnectInput(input) => {
                let input_w = 50u16.min(inner.width.saturating_sub(2));
                let input_h = 4u16;
                let cx = inner.x + inner.width.saturating_sub(input_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(input_h) / 2;
                let input_area = Rect {
                    x: cx,
                    y: cy,
                    width: input_w,
                    height: input_h,
                };
                let text_w = input_w.saturating_sub(2) as usize;
                let (display, cursor_col) = input.view(text_w);
                let paragraph = Paragraph::new(vec![
                    Line::from(Span::raw(display).style(Style::default().fg(Color::White))),
                    Line::from(
                        Span::raw("e.g. -o StrictHostKeyChecking=no user@host")
                            .style(Style::default().fg(Color::DarkGray)),
                    ),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(" ssh "),
                );
                paragraph.render(input_area, buf);
                // Block cursor: reverse the cell at the cursor column.
                let cur_x = input_area.x + 1 + cursor_col as u16;
                let cur_y = input_area.y + 1;
                if cur_x < input_area.x + input_w.saturating_sub(1) {
                    let style = buf[(cur_x, cur_y)].style().add_modifier(Modifier::REVERSED);
                    buf[(cur_x, cur_y)].set_style(style);
                }
            }
            ConnectOverlay::None => {}
        }
    }
}
