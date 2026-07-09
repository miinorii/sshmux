use ratatui::{buffer::Buffer, layout::Rect};

use crate::browser::common::Browser;

use super::render_exit_overlay;

/// Render a "session ended — Reconnect / Close pane" overlay on top of a browser
/// pane when its underlying PTY has exited.
pub fn render_browser_exit_overlay(browser: &dyn Browser, area: Rect, buf: &mut Buffer) {
    if !browser.process_exited() {
        return;
    }
    render_exit_overlay(area, buf, browser.core().exit_selection);
}
