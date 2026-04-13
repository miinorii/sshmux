mod browser;
pub mod connect;
mod session;
pub mod tree;

use std::sync::atomic::Ordering;

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::browser::common::Browser;
use crate::browser::{FileBrowser, SshBrowser};
use connect::ConnectPane;
use crate::keybindings::KeyBindings;
use crate::ssh_config::SshHost;
use crate::terminal::EmbeddedTerminal;

pub use tree::{
    SeparatorHit, find_directional_neighbor, hit_test_separator, remove_leaf, split_at_path_mut,
};

// ---------------------------------------------------------------------------
// Split direction
// ---------------------------------------------------------------------------

pub enum Split {
    LeftRight,
    TopBottom,
}

// ---------------------------------------------------------------------------
// Focus direction
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// Pane
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
    Split {
        kind: Split,
        children: Vec<Pane>,
        ratios: Vec<u16>,
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

    pub fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect(_)
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => vec![area],
            Pane::Split {
                kind,
                children,
                ratios,
            } => {
                let areas = split_areas(area, kind, ratios);
                children
                    .iter()
                    .zip(areas)
                    .flat_map(|(c, a)| c.leaf_areas(a))
                    .collect()
            }
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Pane::Connect(_)
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect(_)
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => {
                if n == 0 {
                    Some(self)
                } else {
                    None
                }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf_mut(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    pub fn leaf(&self, n: usize) -> Option<&Pane> {
        match self {
            Pane::Connect(_)
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => {
                if n == 0 {
                    Some(self)
                } else {
                    None
                }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    pub fn split_leaf(&mut self, n: usize, kind: Split) -> bool {
        match self {
            Pane::Connect(_)
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => false,
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children.iter_mut() {
                    let count = child.leaf_count();
                    if n < offset + count {
                        if count == 1 {
                            let old = std::mem::replace(child, Pane::new_connect());
                            *child = Pane::Split {
                                kind,
                                children: vec![old, Pane::new_connect()],
                                ratios: vec![100, 100],
                            };
                        } else {
                            child.split_leaf(n - offset, kind);
                        }
                        return true;
                    }
                    offset += count;
                }
                false
            }
        }
    }

    pub fn take_dirty(&mut self) -> bool {
        match self {
            Pane::Session { terminal, .. } => terminal.dirty.swap(false, Ordering::AcqRel),
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
            Pane::Split { children, .. } => children.iter_mut().any(|c| c.take_dirty()),
            Pane::Connect(_) => false,
        }
    }

    pub fn tick_browsers(&mut self) {
        match self {
            Pane::FileBrowser { browser } => browser.tick(),
            Pane::SshBrowser { browser } => browser.tick(),
            Pane::Split { children, .. } => children.iter_mut().for_each(|c| c.tick_browsers()),
            Pane::Connect(_) | Pane::Session { .. } => {}
        }
    }

    pub fn resize_all(&mut self, area: Rect, multi_pane: bool) {
        match self {
            Pane::Session { terminal, .. } => {
                let (h, w) = if multi_pane {
                    (area.height.saturating_sub(1), area.width)
                } else {
                    (area.height, area.width)
                };
                terminal.resize(h, w);
            }
            // Browsers use a fixed-size hidden PTY; their display is re-laid out each frame.
            Pane::FileBrowser { .. } | Pane::SshBrowser { .. } | Pane::Connect(_) => {}
            Pane::Split {
                kind,
                children,
                ratios,
            } => {
                let areas = split_areas(area, kind, ratios);
                for (child, a) in children.iter_mut().zip(areas) {
                    child.resize_all(a, true);
                }
            }
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &mut RenderCtx<'_>) {
        let focus_idx = ctx.focus_idx;
        let leaf_count = ctx.leaf_count;
        let hosts = ctx.hosts;
        let keybindings = ctx.keybindings;
        match self {
            Pane::Connect(pane) => {
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
                pane.render(area, buf, is_focus, hosts, leaf_count, keybindings);
            }

            Pane::Session {
                terminal,
                exit_selection,
                ssh_args,
            } => {
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
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
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count, &keybindings.browser);
                browser::render_browser_exit_overlay(browser, area, buf);
            }

            Pane::SshBrowser { browser } => {
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count, &keybindings.browser);
                browser::render_browser_exit_overlay(browser, area, buf);
            }

            Pane::Split {
                kind,
                children,
                ratios,
            } => {
                let areas = split_areas(area, kind, ratios);
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
                                let top_connects =
                                    matches!(top_sym, "│" | "┬" | "├" | "┤" | "┼");
                                let junction = if top_connects { '┼' } else { '┬' };
                                buf[(x, area.y)].set_char(junction).set_style(sep_style);
                            }
                            for y in (area.y + 1)..(area.y + area.height) {
                                buf[(x, y)].set_char('│').set_style(sep_style);
                            }
                        }
                        for (child, a) in children.iter_mut().zip(areas.iter()) {
                            child.render(*a, buf, ctx);
                        }
                    }
                    Split::TopBottom => {
                        for (i, (child, a)) in
                            children.iter_mut().zip(areas.iter()).enumerate()
                        {
                            child.render(*a, buf, ctx);
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
// split_areas
// ---------------------------------------------------------------------------

pub fn split_areas(area: Rect, kind: &Split, ratios: &[u16]) -> Vec<Rect> {
    if ratios.is_empty() {
        return vec![];
    }
    let direction = match kind {
        Split::LeftRight => Direction::Horizontal,
        Split::TopBottom => Direction::Vertical,
    };
    match kind {
        Split::LeftRight => {
            // Interleave Fill(ratio) pane constraints with Length(1) separator constraints so
            // a single-cell │ can be drawn between adjacent panes.
            let mut constraints = Vec::with_capacity(ratios.len() * 2 - 1);
            for (i, &r) in ratios.iter().enumerate() {
                if i > 0 {
                    constraints.push(Constraint::Length(1));
                }
                constraints.push(Constraint::Fill(r));
            }
            let all_areas = Layout::default()
                .direction(direction)
                .constraints(constraints)
                .split(area)
                .to_vec();
            // Return only the even-indexed areas (the pane areas, not the separators).
            all_areas.into_iter().step_by(2).collect()
        }
        Split::TopBottom => {
            // No spacing: each pane's own top-border title line acts as the visual
            // separator between stacked panes.
            let constraints: Vec<Constraint> =
                ratios.iter().map(|&r| Constraint::Fill(r)).collect();
            Layout::default()
                .direction(direction)
                .constraints(constraints)
                .split(area)
                .to_vec()
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
    fn connect() -> Pane {
        Pane::new_connect()
    }
    fn hsplit() -> Pane {
        Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        }
    }
    fn vsplit() -> Pane {
        Pane::Split {
            kind: Split::TopBottom,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        }
    }

    // ---- split_areas -------------------------------------------------------

    #[test]
    fn split_areas_horizontal_even() {
        let a = split_areas(r(100, 20), &Split::LeftRight, &[100, 100]);
        assert_eq!(
            a[0],
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 20
            }
        );
        assert_eq!(
            a[1],
            Rect {
                x: 51,
                y: 0,
                width: 49,
                height: 20
            }
        );
    }

    #[test]
    fn split_areas_horizontal_remainder() {
        let a = split_areas(r(101, 20), &Split::LeftRight, &[100, 100]);
        assert_eq!(a[0].width + a[1].width, 100);
    }

    #[test]
    fn split_areas_vertical_even() {
        let a = split_areas(r(80, 40), &Split::TopBottom, &[100, 100]);
        assert_eq!(
            a[0],
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 20
            }
        );
        assert_eq!(
            a[1],
            Rect {
                x: 0,
                y: 20,
                width: 80,
                height: 20
            }
        );
    }

    #[test]
    fn split_areas_vertical_three() {
        let a = split_areas(r(80, 30), &Split::TopBottom, &[100, 100, 100]);
        assert_eq!(a.len(), 3);
        assert_eq!(a.iter().map(|x| x.height).sum::<u16>(), 30);
    }

    #[test]
    fn split_areas_empty() {
        assert!(split_areas(r(80, 40), &Split::LeftRight, &[]).is_empty());
    }

    // ---- leaf_count --------------------------------------------------------

    #[test]
    fn leaf_count_single() {
        assert_eq!(connect().leaf_count(), 1);
    }

    #[test]
    fn leaf_count_split() {
        assert_eq!(hsplit().leaf_count(), 2);
    }

    #[test]
    fn leaf_count_nested() {
        let p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        assert_eq!(p.leaf_count(), 3);
    }

    // ---- leaf / leaf_areas -------------------------------------------------

    #[test]
    fn leaf_single_bounds() {
        let p = connect();
        assert!(p.leaf(0).is_some());
        assert!(p.leaf(1).is_none());
    }

    #[test]
    fn leaf_split_dfs_order() {
        let p = hsplit();
        assert!(matches!(p.leaf(0), Some(Pane::Connect(_))));
        assert!(p.leaf(2).is_none());
    }

    #[test]
    fn leaf_areas_covers_full() {
        assert_eq!(connect().leaf_areas(r(100, 50)), vec![r(100, 50)]);
    }

    #[test]
    fn leaf_areas_sum_equals_parent() {
        let a = hsplit().leaf_areas(r(100, 50));
        assert_eq!(a[0].width + a[1].width, 99);
    }

    #[test]
    fn leaf_areas_count_matches_leaf_count() {
        let p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        assert_eq!(p.leaf_areas(r(120, 60)).len(), p.leaf_count());
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

    // ---- leaf_mut ----------------------------------------------------------

    #[test]
    fn leaf_mut_single() {
        let mut p = connect();
        assert!(p.leaf_mut(0).is_some());
        assert!(p.leaf_mut(1).is_none());
    }

    #[test]
    fn leaf_mut_split() {
        let mut p = hsplit();
        assert!(p.leaf_mut(0).is_some());
        assert!(p.leaf_mut(1).is_some());
        assert!(p.leaf_mut(2).is_none());
    }

    #[test]
    fn leaf_mut_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        assert!(p.leaf_mut(0).is_some());
        assert!(p.leaf_mut(1).is_some());
        assert!(p.leaf_mut(2).is_some());
        assert!(p.leaf_mut(3).is_none());
    }

    #[test]
    fn leaf_mut_modifies_correct_pane() {
        let mut p = hsplit();
        if let Some(pane) = p.leaf_mut(1) {
            *pane = connect();
        }
        assert!(matches!(p.leaf(1), Some(Pane::Connect(_))));
    }

    // ---- split_leaf --------------------------------------------------------

    #[test]
    fn split_leaf_increases_count() {
        let mut p = hsplit();
        p.split_leaf(0, Split::TopBottom);
        assert_eq!(p.leaf_count(), 3);
    }

    #[test]
    fn split_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        p.split_leaf(2, Split::LeftRight);
        assert_eq!(p.leaf_count(), 4);
    }

    #[test]
    fn split_leaf_noop_on_single() {
        let mut p = connect();
        p.split_leaf(0, Split::LeftRight);
        assert_eq!(p.leaf_count(), 1);
    }

    // ---- split_areas (additional) ------------------------------------------

    #[test]
    fn split_areas_single_element() {
        let a = split_areas(r(100, 50), &Split::LeftRight, &[100]);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0], r(100, 50));
    }

    #[test]
    fn split_areas_vertical_remainder() {
        let a = split_areas(r(80, 31), &Split::TopBottom, &[100, 100]);
        assert_eq!(a[0].height + a[1].height, 31);
    }

    // ---- junction rendering ------------------------------------------------

    fn render_to_buf(p: &mut Pane, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let leaf_count = p.leaf_count();
        let kb = KeyBindings::default();
        let mut ctx = RenderCtx {
            hosts: &[],
            focus_idx: 0,
            leaf_count,
            my_idx: 0,
            keybindings: &kb,
        };
        p.render(area, &mut buf, &mut ctx);
        buf
    }

    #[test]
    fn junction_lr_separator_is_tee_down_at_top() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 21,
            height: 5,
        };
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
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
        let area = Rect {
            x: 0,
            y: 0,
            width: 41,
            height: 10,
        };
        let mut p = Pane::Split {
            kind: Split::TopBottom,
            children: vec![
                Pane::Split {
                    kind: Split::LeftRight,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
                connect(),
            ],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
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
        let area = Rect {
            x: 0,
            y: 0,
            width: 21,
            height: 10,
        };
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
        assert_eq!(buf[(10u16, 5u16)].symbol(), "├");
    }

    #[test]
    fn junction_lr_separator_right_of_inner_tb_becomes_tee_left() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 21,
            height: 10,
        };
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
                connect(),
            ],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
        assert_eq!(buf[(10u16, 5u16)].symbol(), "┤");
    }

    #[test]
    fn junction_stacked_lr_separators_at_same_column_produce_cross() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 41,
            height: 10,
        };
        let mut p = Pane::Split {
            kind: Split::TopBottom,
            children: vec![
                Pane::Split {
                    kind: Split::LeftRight,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
                Pane::Split {
                    kind: Split::LeftRight,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
        assert_eq!(buf[(20u16, 5u16)].symbol(), "┼");
    }

    // ---- split_areas with unequal ratios (drag resize effect) ---------------

    #[test]
    fn split_areas_lr_unequal_ratios_left_is_wider() {
        let a = split_areas(r(90, 20), &Split::LeftRight, &[200, 100]);
        assert!(
            a[0].width > a[1].width,
            "left pane should be wider with ratio 200:100"
        );
        assert!(
            a[0].width > a[1].width * 3 / 2,
            "left pane should be at least 1.5x right"
        );
        assert_eq!(a[0].width + a[1].width, 89);
    }

    #[test]
    fn split_areas_tb_unequal_ratios_top_is_taller() {
        let a = split_areas(r(80, 40), &Split::TopBottom, &[300, 100]);
        assert!(
            a[0].height > a[1].height,
            "top pane should be taller with ratio 300:100"
        );
        assert!(
            a[0].height > a[1].height * 2,
            "top pane should be more than 2x bottom"
        );
        assert_eq!(a[0].height + a[1].height, 40);
    }

    #[test]
    fn split_areas_lr_min_ratio_still_allocates_space() {
        let a = split_areas(r(60, 10), &Split::LeftRight, &[1, 99]);
        assert!(a[0].width >= 1);
        assert!(a[1].width >= 1);
        assert_eq!(a[0].width + a[1].width, 59);
    }

}
