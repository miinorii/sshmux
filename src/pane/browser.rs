use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

use crate::browser::common::Browser;
use crate::widgets::overlays::ExitOverlay;

/// Render a "session ended — Reconnect / Close pane" overlay on top of a browser
/// pane when its underlying PTY has exited.
pub fn render_browser_exit_overlay(browser: &dyn Browser, area: Rect, buf: &mut Buffer) {
    if !browser.process_exited() {
        return;
    }
    ExitOverlay {
        selection: browser.core().exit_selection,
    }
    .render(area, buf);
}
