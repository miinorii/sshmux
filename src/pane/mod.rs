mod browser;
pub mod connect;
mod session;

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::browser::common::Browser;
use crate::browser::{FileBrowser, SshBrowser};
use crate::keybindings::KeyBindings;
use crate::ssh_config::SshHost;
use crate::terminal::{EmbeddedTerminal, PtyChannel};
use connect::ConnectPane;

pub use crate::widgets::pane_tree::{
    FocusDir, Node, SeparatorHit, Split, SplitTree, find_directional_neighbor, split_areas,
};

// ---------------------------------------------------------------------------
// Pane — a single leaf of the split tree
// ---------------------------------------------------------------------------

pub enum Pane {
    Connect(ConnectPane),
    Session {
        terminal: EmbeddedTerminal,
        ssh_args: String,
        exit_selection: u8, // 0 = Reconnect, 1 = Close pane
    },
    FileBrowser {
        browser: FileBrowser,
    },
    SshBrowser {
        browser: SshBrowser,
    },
}

impl Pane {
    pub fn new_connect() -> Self {
        Pane::Connect(ConnectPane::new())
    }

    /// Returns `true` if this pane is a browser (SFTP or SCP).
    pub fn is_browser(&self) -> bool {
        matches!(self, Pane::FileBrowser { .. } | Pane::SshBrowser { .. })
    }

    /// Returns this pane as a `&dyn Browser` if it is a browser pane.
    pub fn as_browser(&self) -> Option<&dyn Browser> {
        match self {
            Pane::FileBrowser { browser } => Some(browser),
            Pane::SshBrowser { browser } => Some(browser),
            _ => None,
        }
    }

    /// Returns this pane as a `&mut dyn Browser` if it is a browser pane.
    pub fn as_browser_mut(&mut self) -> Option<&mut dyn Browser> {
        match self {
            Pane::FileBrowser { browser } => Some(browser),
            Pane::SshBrowser { browser } => Some(browser),
            _ => None,
        }
    }

    fn take_dirty(&mut self) -> bool {
        match self {
            Pane::Session { terminal, .. } => terminal.take_dirty(),
            Pane::FileBrowser { browser } => {
                let pty_dirty = browser.sftp.take_dirty();
                let state_dirty = browser.core.needs_redraw;
                browser.core.needs_redraw = false;
                pty_dirty || state_dirty
            }
            Pane::SshBrowser { browser } => {
                let pty_dirty = browser.ssh.take_dirty();
                let scp_dirty = browser
                    .scp_pty
                    .as_mut()
                    .map(|s| s.take_dirty())
                    .unwrap_or(false);
                let state_dirty = browser.core.needs_redraw;
                browser.core.needs_redraw = false;
                pty_dirty || scp_dirty || state_dirty
            }
            Pane::Connect(_) => false,
        }
    }

    fn tick_browser(&mut self) {
        match self {
            Pane::FileBrowser { browser } => browser.tick(),
            Pane::SshBrowser { browser } => browser.tick(),
            Pane::Connect(_) | Pane::Session { .. } => {}
        }
    }

    /// Resize this leaf's backing PTY (sessions only; browsers use a
    /// fixed-size hidden PTY and re-lay out their display each frame).
    pub fn resize_all(&mut self, area: Rect, multi_pane: bool) {
        if let Pane::Session { terminal, .. } = self {
            let (h, w) = if multi_pane {
                (area.height.saturating_sub(1), area.width)
            } else {
                (area.height, area.width)
            };
            terminal.resize(h, w);
        }
    }

    /// Title shown in this pane's top border (multi-pane mode).
    pub fn title(&self) -> String {
        match self {
            Pane::Connect(_) => "connect".to_string(),
            Pane::Session { ssh_args, .. } => ssh_args
                .split_whitespace()
                .last()
                .unwrap_or("ssh")
                .to_string(),
            Pane::FileBrowser { browser } => format!(" sftp: {} ", browser.core.host),
            Pane::SshBrowser { browser } => format!(" scp: {} ", browser.core.host),
        }
    }

    /// Render this leaf's content into its inner area (title bars are drawn
    /// by `PaneTreeView`).
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        is_focus: bool,
        hosts: &[SshHost],
        keybindings: &KeyBindings,
    ) {
        match self {
            Pane::Connect(pane) => {
                pane.render(area, buf, hosts, keybindings);
            }
            Pane::Session {
                terminal,
                exit_selection,
                ..
            } => {
                session::render_session(area, buf, terminal, *exit_selection);
            }
            Pane::FileBrowser { browser } => {
                browser.render(area, buf, is_focus, &keybindings.browser);
                browser::render_browser_exit_overlay(browser, area, buf);
            }
            Pane::SshBrowser { browser } => {
                browser.render(area, buf, is_focus, &keybindings.browser);
                browser::render_browser_exit_overlay(browser, area, buf);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pane-specific operations over the split tree
// ---------------------------------------------------------------------------

impl SplitTree<Pane> {
    /// Consume every leaf's dirty flag; true if any was dirty.
    pub fn take_dirty(&mut self) -> bool {
        // `|` (not `any`) so every leaf's flag is consumed.
        self.leaves_mut()
            .into_iter()
            .fold(false, |acc, p| p.take_dirty() | acc)
    }

    pub fn tick_browsers(&mut self) {
        for pane in self.leaves_mut() {
            pane.tick_browser();
        }
    }

    pub fn resize_all(&mut self, area: Rect, multi_pane: bool) {
        let areas = self.leaf_areas(area);
        for (pane, a) in self.leaves_mut().into_iter().zip(areas) {
            pane.resize_all(a, multi_pane);
        }
    }
}

// ---------------------------------------------------------------------------
// pane_inner / render_pane_border
// ---------------------------------------------------------------------------

/// The drawable area above the tab-bar row: strips the single bottom row, full width, no borders.
pub fn pane_inner(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// The drawable area inside a pane's top-border title line: 1 row from the top, full width.
pub fn pane_border_inner(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1).min(area.y + area.height),
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// Render the "session ended — Reconnect / Close pane" overlay shared by
/// session and browser panes.
pub(crate) fn render_exit_overlay(area: Rect, buf: &mut Buffer, exit_selection: u8) {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }
    // ---- pane_inner --------------------------------------------------------

    #[test]
    fn pane_inner_shrinks_by_one() {
        let inner = pane_inner(r(10, 8));
        assert_eq!(inner.x, 0);
        assert_eq!(inner.y, 0);
        assert_eq!(inner.width, 10);
        assert_eq!(inner.height, 7);
    }

    #[test]
    fn pane_inner_saturates_at_zero() {
        let inner = pane_inner(Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        assert_eq!(inner.height, 0);
    }

    // ---- Golden frames (behavior freeze for the widget refactor) ----------

    #[test]
    fn golden_exit_overlay() {
        let area = r(40, 5);
        let mut buf = Buffer::empty(area);
        render_exit_overlay(area, &mut buf, 0);
        crate::widgets::testing::assert_rows(
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
}
