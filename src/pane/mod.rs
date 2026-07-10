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

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &mut RenderCtx<'_>) {
        let is_focus = ctx.my_idx == ctx.focus_idx;
        ctx.my_idx += 1;
        let leaf_count = ctx.leaf_count;
        match self {
            Pane::Connect(pane) => {
                pane.render(area, buf, is_focus, ctx.hosts, leaf_count, ctx.keybindings);
            }
            Pane::Session {
                terminal,
                exit_selection,
                ssh_args,
            } => {
                session::render_session(
                    area,
                    buf,
                    terminal,
                    ssh_args,
                    *exit_selection,
                    is_focus,
                    leaf_count,
                );
            }
            Pane::FileBrowser { browser } => {
                browser.render(area, buf, is_focus, leaf_count, &ctx.keybindings.browser);
                browser::render_browser_exit_overlay(browser, area, buf);
            }
            Pane::SshBrowser { browser } => {
                browser.render(area, buf, is_focus, leaf_count, &ctx.keybindings.browser);
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

    /// Recursive render: separators + junction glyphs for splits, leaf
    /// content via `Pane::render`.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &mut RenderCtx<'_>) {
        render_node(&mut self.root, area, buf, ctx);
    }
}

fn render_node(node: &mut Node<Pane>, area: Rect, buf: &mut Buffer, ctx: &mut RenderCtx<'_>) {
    match node {
        Node::Leaf(pane) => pane.render(area, buf, ctx),
        Node::Split {
            kind,
            children,
            ratios,
        } => {
            let areas = split_areas(area, *kind, ratios);
            match kind {
                Split::LeftRight => {
                    let sep_style = Style::default().fg(Color::DarkGray);
                    for pair in areas.windows(2) {
                        let x = pair[0].right();
                        if area.height > 0 {
                            let top_sym = if area.y > 0 {
                                buf[(x, area.y - 1)].symbol()
                            } else {
                                ""
                            };
                            let top_connects = matches!(top_sym, "│" | "┬" | "├" | "┤" | "┼");
                            let junction = if top_connects { '┼' } else { '┬' };
                            buf[(x, area.y)].set_char(junction).set_style(sep_style);
                        }
                        for y in (area.y + 1)..(area.y + area.height) {
                            buf[(x, y)].set_char('│').set_style(sep_style);
                        }
                    }
                    for (child, a) in children.iter_mut().zip(areas.iter()) {
                        render_node(child, *a, buf, ctx);
                    }
                }
                Split::TopBottom => {
                    for (i, (child, a)) in children.iter_mut().zip(areas.iter()).enumerate() {
                        render_node(child, *a, buf, ctx);
                        if i > 0 {
                            let ty = a.y;
                            let buf_right = buf.area().x + buf.area().width;
                            if a.x > 0 {
                                let lx = a.x - 1;
                                let s = buf[(lx, ty)].style();
                                match buf[(lx, ty)].symbol() {
                                    "│" => {
                                        buf[(lx, ty)].set_char('├').set_style(s);
                                    }
                                    "┬" => {
                                        buf[(lx, ty)].set_char('┼').set_style(s);
                                    }
                                    _ => {}
                                }
                            }
                            let rx = a.x + a.width;
                            if rx < buf_right {
                                let s = buf[(rx, ty)].style();
                                match buf[(rx, ty)].symbol() {
                                    "│" => {
                                        buf[(rx, ty)].set_char('┤').set_style(s);
                                    }
                                    "┬" => {
                                        buf[(rx, ty)].set_char('┼').set_style(s);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RenderCtx — read-only context threaded through the recursive render pass
// ---------------------------------------------------------------------------

pub struct RenderCtx<'a> {
    pub hosts: &'a [SshHost],
    pub focus_idx: usize,
    pub leaf_count: usize,
    /// Running DFS leaf index — incremented by each leaf variant during render.
    pub my_idx: usize,
    pub keybindings: &'a KeyBindings,
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

/// Render a pane title in the top-border line when in multi-pane mode and return the
/// content area below it. In single-pane mode returns the full area unchanged.
pub fn render_pane_border(
    area: Rect,
    buf: &mut Buffer,
    is_focus: bool,
    leaf_count: usize,
    title: &str,
) -> Rect {
    if leaf_count > 1 {
        let border_style = Style::default().fg(Color::DarkGray);
        let name_style = if is_focus {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style)
            .title(Line::from(vec![
                Span::styled("── ", border_style),
                Span::styled(title, name_style),
                Span::styled(" ", border_style),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);
        // Post-process title-bar row: replace ─ with ┴ where a vertical separator
        // from a sibling above ends at this pane's top edge.
        if area.y > 0 {
            let ty = area.y;
            for x in area.x..area.x + area.width {
                if buf[(x, ty)].symbol() == "─" {
                    let above = buf[(x, ty - 1)].symbol();
                    if matches!(above, "│" | "┬" | "├" | "┤" | "┼") {
                        let s = buf[(x, ty)].style();
                        buf[(x, ty)].set_char('┴').set_style(s);
                    }
                }
            }
        }
        inner
    } else {
        area
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeyBindings;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }
    fn connect() -> Node<Pane> {
        Node::Leaf(Pane::new_connect())
    }
    fn split(kind: Split, children: Vec<Node<Pane>>) -> Node<Pane> {
        let ratios = vec![100; children.len()];
        Node::Split {
            kind,
            children,
            ratios,
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

    // ---- junction rendering ------------------------------------------------

    fn render_to_buf(tree: &mut SplitTree<Pane>, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let leaf_count = tree.leaf_count();
        let kb = KeyBindings::default();
        let mut ctx = RenderCtx {
            hosts: &[],
            focus_idx: 0,
            leaf_count,
            my_idx: 0,
            keybindings: &kb,
        };
        tree.render(area, &mut buf, &mut ctx);
        buf
    }

    #[test]
    fn junction_lr_separator_is_tee_down_at_top() {
        let area = r(21, 5);
        let mut tree = SplitTree {
            root: split(Split::LeftRight, vec![connect(), connect()]),
        };
        let buf = render_to_buf(&mut tree, area);
        assert_eq!(
            buf[(10u16, 0u16)].symbol(),
            "┬",
            "top of separator should be ┬"
        );
        for y in 1..5u16 {
            assert_eq!(
                buf[(10u16, y)].symbol(),
                "│",
                "separator body at y={y} should be │"
            );
        }
    }

    #[test]
    fn junction_title_bar_below_lr_separator_gets_tee_up() {
        let area = r(41, 10);
        let mut tree = SplitTree {
            root: split(
                Split::TopBottom,
                vec![
                    split(Split::LeftRight, vec![connect(), connect()]),
                    connect(),
                ],
            ),
        };
        let buf = render_to_buf(&mut tree, area);
        assert_eq!(
            buf[(20u16, 0u16)].symbol(),
            "┬",
            "separator top should be ┬"
        );
        for y in 1..5u16 {
            assert_eq!(
                buf[(20u16, y)].symbol(),
                "│",
                "separator body at y={y} should be │"
            );
        }
        assert_eq!(
            buf[(20u16, 5u16)].symbol(),
            "┴",
            "title bar below separator should be ┴"
        );
    }

    #[test]
    fn junction_lr_separator_left_of_inner_tb_becomes_tee_right() {
        let area = r(21, 10);
        let mut tree = SplitTree {
            root: split(
                Split::LeftRight,
                vec![
                    connect(),
                    split(Split::TopBottom, vec![connect(), connect()]),
                ],
            ),
        };
        let buf = render_to_buf(&mut tree, area);
        assert_eq!(buf[(10u16, 5u16)].symbol(), "├");
    }

    #[test]
    fn junction_lr_separator_right_of_inner_tb_becomes_tee_left() {
        let area = r(21, 10);
        let mut tree = SplitTree {
            root: split(
                Split::LeftRight,
                vec![
                    split(Split::TopBottom, vec![connect(), connect()]),
                    connect(),
                ],
            ),
        };
        let buf = render_to_buf(&mut tree, area);
        assert_eq!(buf[(10u16, 5u16)].symbol(), "┤");
    }

    #[test]
    fn junction_stacked_lr_separators_at_same_column_produce_cross() {
        let area = r(41, 10);
        let mut tree = SplitTree {
            root: split(
                Split::TopBottom,
                vec![
                    split(Split::LeftRight, vec![connect(), connect()]),
                    split(Split::LeftRight, vec![connect(), connect()]),
                ],
            ),
        };
        let buf = render_to_buf(&mut tree, area);
        assert_eq!(buf[(20u16, 5u16)].symbol(), "┼");
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
