use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

use crate::terminal::EmbeddedTerminal;
use crate::widgets::terminal::TerminalView;

use super::render_exit_overlay;

/// Render a Session pane's content: terminal grid + exit overlay when the
/// process has ended. The title bar is drawn by `PaneTreeView`.
pub fn render_session(
    area: Rect,
    buf: &mut Buffer,
    terminal: &mut EmbeddedTerminal,
    exit_selection: u8,
) {
    TerminalView.render(area, buf, terminal);

    if terminal.process_exited() {
        render_exit_overlay(area, buf, exit_selection);
    }
}
