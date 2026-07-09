use ratatui::{buffer::Buffer, layout::Rect};

use crate::terminal::EmbeddedTerminal;

use super::{render_exit_overlay, render_pane_border};

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
        render_exit_overlay(inner, buf, exit_selection);
    }
}
