use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{StatefulWidget, Widget},
};

use crate::terminal::EmbeddedTerminal;
use crate::widgets::overlays::ExitOverlay;
use crate::widgets::terminal::TerminalView;

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
        ExitOverlay {
            selection: exit_selection,
        }
        .render(area, buf);
    }
}
