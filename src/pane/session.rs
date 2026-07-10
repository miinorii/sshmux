use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

use crate::terminal::EmbeddedTerminal;
use crate::widgets::terminal::TerminalView;

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
    TerminalView.render(inner, buf, terminal);

    if terminal.process_exited() {
        render_exit_overlay(inner, buf, exit_selection);
    }
}
