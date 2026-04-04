use std::sync::atomic::Ordering;

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::browser::common::Browser;
use crate::browser::{FileBrowser, SshBrowser};
use crate::connect::ConnectPane;
use crate::ssh_config::SshHost;
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Split direction
// ---------------------------------------------------------------------------

pub enum Split {
    LeftRight,
    TopBottom,
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

                let host = ssh_args.split_whitespace().last().unwrap_or("ssh");
                let inner = render_pane_border(area, buf, is_focus, leaf_count, host);
                terminal.render_into(inner, buf);

                if terminal.process_exited() {
                    let menu_w = 34u16.min(inner.width.saturating_sub(2));
                    let menu_h = 3u16;
                    let cx = inner.x + inner.width.saturating_sub(menu_w) / 2;
                    let cy = inner.y + inner.height.saturating_sub(menu_h) / 2;
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
                        let style = if i as u8 == *exit_selection { sel } else { dim };
                        spans.push(Span::raw(*item).style(style));
                    }
                    // Clear the overlay area so terminal content doesn't bleed through
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
            }

            Pane::FileBrowser { browser } => {
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count, &keybindings.browser);
            }

            Pane::SshBrowser { browser } => {
                let is_focus = ctx.my_idx == focus_idx;
                ctx.my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count, &keybindings.browser);
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
                            // Use ┼ instead of ┬ when a vertical separator from a sibling
                            // above ends exactly at this row (TopBottom split above us).
                            if area.height > 0 {
                                let top_sym =
                                    if area.y > 0 { buf[(x, area.y - 1)].symbol() } else { "" };
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
                                // Fix ├/┤/┼ junction characters where outer vertical
                                // separators meet the title-bar row of this pane.
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
    pub keybindings: &'a crate::keybindings::KeyBindings,
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
            let constraints: Vec<Constraint> = ratios.iter().map(|&r| Constraint::Fill(r)).collect();
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
// Separator hit-testing and resize support
// ---------------------------------------------------------------------------

/// Identifies which separator was hit by a mouse click.
pub struct SeparatorHit {
    /// Path from root to the Split node, as child indices.
    pub path: Vec<usize>,
    /// Index of the separator: between `children[sep_idx]` and `children[sep_idx + 1]`.
    pub sep_idx: usize,
    /// True for LeftRight (drag horizontally), false for TopBottom (drag vertically).
    pub horizontal: bool,
    /// The screen area of the Split node that owns this separator.
    pub split_area: Rect,
}

/// Walk the pane tree and check if `(col, row)` lands on a separator.
/// Returns the innermost separator hit, or `None`.
pub fn hit_test_separator(
    pane: &Pane,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<SeparatorHit> {
    match pane {
        Pane::Split {
            kind,
            children,
            ratios,
        } => {
            let areas = split_areas(area, kind, ratios);
            match kind {
                Split::LeftRight => {
                    // Check explicit 1-cell separator columns first.
                    for (i, pair) in areas.windows(2).enumerate() {
                        let sep_x = pair[0].right();
                        if col == sep_x && row >= area.y && row < area.y + area.height {
                            return Some(SeparatorHit {
                                path: vec![],
                                sep_idx: i,
                                horizontal: true,
                                split_area: area,
                            });
                        }
                    }
                }
                Split::TopBottom => {
                    // The separator is the title-bar row of child[i] for i > 0.
                    for (i, a) in areas.iter().enumerate().skip(1) {
                        if row == a.y && col >= a.x && col < a.x + a.width {
                            return Some(SeparatorHit {
                                path: vec![],
                                sep_idx: i - 1,
                                horizontal: false,
                                split_area: area,
                            });
                        }
                    }
                }
            }
            // Not a separator at this level — recurse into the child that contains
            // the point and prepend our child index to the path.
            for (i, (child, a)) in children.iter().zip(areas.iter()).enumerate() {
                if col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height {
                    if let Some(mut hit) = hit_test_separator(child, *a, col, row) {
                        hit.path.insert(0, i);
                        return Some(hit);
                    }
                    break;
                }
            }
            None
        }
        _ => None,
    }
}

/// Navigate the pane tree to the Split node identified by `path`.
pub fn split_at_path_mut<'a>(pane: &'a mut Pane, path: &[usize]) -> Option<&'a mut Pane> {
    if path.is_empty() {
        return Some(pane);
    }
    if let Pane::Split { children, .. } = pane {
        children
            .get_mut(path[0])
            .and_then(|child| split_at_path_mut(child, &path[1..]))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// remove_leaf
// ---------------------------------------------------------------------------

pub fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect(_)
        | Pane::Session { .. }
        | Pane::FileBrowser { .. }
        | Pane::SshBrowser { .. } => {}
        Pane::Split {
            children, ratios, ..
        } => {
            let mut offset = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 {
                        to_remove = Some(i);
                    } else {
                        remove_leaf(child, n - offset);
                    }
                    break;
                }
                offset += count;
            }
            if let Some(i) = to_remove {
                children.remove(i);
                ratios.remove(i);
            }
        }
    }
    // Collapse a Split that is down to a single child into that child directly.
    if let Pane::Split { children, .. } = pane
        && children.len() == 1
    {
        *pane = children.remove(0);
    }
}

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
        // 100-wide split into 2 panes with a 1-cell separator: widths are 50 + 49 = 99.
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
        // 1 cell reserved for separator between the 2 panes.
        let a = split_areas(r(101, 20), &Split::LeftRight, &[100, 100]);
        assert_eq!(a[0].width + a[1].width, 100);
    }

    #[test]
    fn split_areas_vertical_even() {
        // TopBottom has no spacing: each pane's title bar acts as the visual separator.
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
        // TopBottom has no spacing: heights sum to the full parent height.
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
        // 1 cell is the separator between the two panes.
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

    // ---- remove_leaf -------------------------------------------------------

    #[test]
    fn remove_leaf_first() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        assert_eq!(p.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_second() {
        let mut p = hsplit();
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 2);
    }

    #[test]
    fn remove_leaf_noop_on_single() {
        let mut p = connect();
        remove_leaf(&mut p, 0); // must not panic
        assert_eq!(p.leaf_count(), 1);
    }

    // ---- pane_inner --------------------------------------------------------

    #[test]
    fn pane_inner_shrinks_by_one() {
        // pane_inner strips the bottom tab-bar row; x/y/width are unchanged.
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
        // Replace leaf 1 with a differently-configured connect pane
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
        // split_leaf on a non-Split pane is a no-op
        let mut p = connect();
        p.split_leaf(0, Split::LeftRight);
        assert_eq!(p.leaf_count(), 1);
    }

    // ---- remove_leaf (additional) ------------------------------------------

    #[test]
    fn remove_leaf_collapses_to_single() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        // After removing one child from a 2-child split, it collapses
        assert!(matches!(p, Pane::Connect(_)));
    }

    #[test]
    fn remove_leaf_deep_nested() {
        // 4-leaf tree: split(connect, split(connect, split(connect, connect)))
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![
                        connect(),
                        Pane::Split {
                            kind: Split::LeftRight,
                            children: vec![connect(), connect()],
                            ratios: vec![100, 100],
                        },
                    ],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        assert_eq!(p.leaf_count(), 4);
        remove_leaf(&mut p, 3); // remove deepest right leaf
        assert_eq!(p.leaf_count(), 3);
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
        // TopBottom has no spacing: heights sum to the full parent height.
        let a = split_areas(r(80, 31), &Split::TopBottom, &[100, 100]);
        assert_eq!(a[0].height + a[1].height, 31);
    }

    // ---- junction rendering ------------------------------------------------

    fn render_to_buf(p: &mut Pane, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let leaf_count = p.leaf_count();
        let kb = crate::keybindings::KeyBindings::default();
        let mut ctx = RenderCtx { hosts: &[], focus_idx: 0, leaf_count, my_idx: 0, keybindings: &kb };
        p.render(area, &mut buf, &mut ctx);
        buf
    }

    // With Fill(1)/Length(1)/Fill(1) constraints and width=21, the two pane
    // areas are x=0 w=10 and x=11 w=10, with the separator drawn at x=10.
    // With width=41 the separator is at x=20, safely past the 11-char title.

    #[test]
    fn junction_lr_separator_is_tee_down_at_top() {
        // LeftRight 2-pane: separator column starts with ┬ then │ below.
        let area = Rect { x: 0, y: 0, width: 21, height: 5 };
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        };
        let buf = render_to_buf(&mut p, area);
        assert_eq!(buf[(10u16, 0u16)].symbol(), "┬", "top of separator should be ┬");
        for y in 1..5u16 {
            assert_eq!(buf[(10u16, y)].symbol(), "│", "separator body at y={y} should be │");
        }
    }

    #[test]
    fn junction_title_bar_below_lr_separator_gets_tee_up() {
        let area = Rect { x: 0, y: 0, width: 41, height: 10 };
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
        assert_eq!(buf[(20u16, 0u16)].symbol(), "┬", "separator top should be ┬");
        for y in 1..5u16 {
            assert_eq!(buf[(20u16, y)].symbol(), "│", "separator body at y={y} should be │");
        }
        assert_eq!(buf[(20u16, 5u16)].symbol(), "┴", "title bar below separator should be ┴");
    }

    #[test]
    fn junction_lr_separator_left_of_inner_tb_becomes_tee_right() {
        let area = Rect { x: 0, y: 0, width: 21, height: 10 };
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
        let area = Rect { x: 0, y: 0, width: 21, height: 10 };
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
        let area = Rect { x: 0, y: 0, width: 41, height: 10 };
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
        // ratio [200, 100] should give the left pane roughly double the right pane.
        let a = split_areas(r(90, 20), &Split::LeftRight, &[200, 100]);
        // Total pane space = 89 (1 separator). Left ~ 59, right ~ 30.
        assert!(a[0].width > a[1].width, "left pane should be wider with ratio 200:100");
        assert!(a[0].width > a[1].width * 3 / 2, "left pane should be at least 1.5x right");
        assert_eq!(a[0].width + a[1].width, 89);
    }

    #[test]
    fn split_areas_tb_unequal_ratios_top_is_taller() {
        // ratio [300, 100] should give the top pane roughly 3x the bottom.
        let a = split_areas(r(80, 40), &Split::TopBottom, &[300, 100]);
        assert!(a[0].height > a[1].height, "top pane should be taller with ratio 300:100");
        assert!(a[0].height > a[1].height * 2, "top pane should be more than 2x bottom");
        assert_eq!(a[0].height + a[1].height, 40);
    }

    #[test]
    fn split_areas_lr_min_ratio_still_allocates_space() {
        // Even with a very small ratio, the pane still gets at least some space.
        let a = split_areas(r(60, 10), &Split::LeftRight, &[1, 99]);
        assert!(a[0].width >= 1);
        assert!(a[1].width >= 1);
        assert_eq!(a[0].width + a[1].width, 59); // 1 separator
    }

    // ---- hit_test_separator -------------------------------------------------

    #[test]
    fn hit_test_lr_hits_separator() {
        // r(21,10) with [100,100]: separator is at x=10 (child[0] has width=10).
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        let hit = hit_test_separator(&p, r(21, 10), sep_x, 5);
        assert!(hit.is_some(), "should hit LR separator at x={sep_x}");
        let hit = hit.unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(hit.horizontal);
        assert_eq!(hit.path, vec![]);
    }

    #[test]
    fn hit_test_lr_misses_left_of_separator() {
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        assert!(hit_test_separator(&p, r(21, 10), sep_x - 1, 5).is_none());
    }

    #[test]
    fn hit_test_lr_misses_right_of_separator() {
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        assert!(hit_test_separator(&p, r(21, 10), sep_x + 1, 5).is_none());
    }

    #[test]
    fn hit_test_tb_hits_separator() {
        // r(80,20) with [100,100]: separator is the title bar row of child[1] at y=10.
        let p = vsplit();
        let areas = split_areas(r(80, 20), &Split::TopBottom, &[100, 100]);
        let sep_y = areas[1].y;
        let hit = hit_test_separator(&p, r(80, 20), 40, sep_y);
        assert!(hit.is_some(), "should hit TB separator at y={sep_y}");
        let hit = hit.unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(!hit.horizontal);
        assert_eq!(hit.path, vec![]);
    }

    #[test]
    fn hit_test_tb_misses_above_separator() {
        let p = vsplit();
        let areas = split_areas(r(80, 20), &Split::TopBottom, &[100, 100]);
        let sep_y = areas[1].y;
        assert!(hit_test_separator(&p, r(80, 20), 40, sep_y - 1).is_none());
    }

    #[test]
    fn hit_test_leaf_returns_none() {
        assert!(hit_test_separator(&connect(), r(80, 20), 10, 10).is_none());
    }

    #[test]
    fn hit_test_lr_three_panes_second_separator() {
        // 3-child LR split: sep[0] between child[0] and child[1], sep[1] between [1] and [2].
        let p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect(), connect()],
            ratios: vec![100, 100, 100],
        };
        let area = r(62, 10);
        let areas = split_areas(area, &Split::LeftRight, &[100, 100, 100]);
        let sep1_x = areas[1].right(); // right edge of child[1] = second separator
        let hit = hit_test_separator(&p, area, sep1_x, 5);
        assert!(hit.is_some(), "should hit second LR separator");
        assert_eq!(hit.unwrap().sep_idx, 1);
    }

    #[test]
    fn hit_test_nested_prepends_path() {
        // Outer LeftRight: child[0]=connect, child[1]=TopBottom(connect, connect).
        // Clicking the TB separator inside child[1] should return path=[1].
        let inner_area_r = r(80, 20);
        let outer = Pane::Split {
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
        let outer_areas = split_areas(inner_area_r, &Split::LeftRight, &[100, 100]);
        let right_area = outer_areas[1];
        let inner_areas = split_areas(right_area, &Split::TopBottom, &[100, 100]);
        let sep_y = inner_areas[1].y;
        // Click in the right child's TB separator row.
        let hit = hit_test_separator(&outer, inner_area_r, right_area.x + 2, sep_y);
        assert!(hit.is_some(), "should hit nested TB separator");
        let hit = hit.unwrap();
        assert_eq!(hit.path, vec![1], "path should point to child[1]");
        assert!(!hit.horizontal);
        assert_eq!(hit.sep_idx, 0);
    }

    #[test]
    fn hit_test_split_area_is_containing_node_area() {
        // The split_area in the returned hit should match the area passed to the hit-testing Split.
        let p = hsplit();
        let area = r(21, 10);
        let areas = split_areas(area, &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        let hit = hit_test_separator(&p, area, sep_x, 5).unwrap();
        assert_eq!(hit.split_area, area);
    }

    // ---- split_at_path_mut --------------------------------------------------

    #[test]
    fn split_at_path_empty_returns_root() {
        let mut p = hsplit();
        let result = split_at_path_mut(&mut p, &[]);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Pane::Split { kind: Split::LeftRight, .. }));
    }

    #[test]
    fn split_at_path_single_step_child0() {
        // path [0] navigates into child[0] — which is a leaf, so returns Some(leaf)
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![vsplit(), connect()],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[0]);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Pane::Split { kind: Split::TopBottom, .. }));
    }

    #[test]
    fn split_at_path_single_step_child1() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[1]);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Pane::Split { kind: Split::TopBottom, .. }));
    }

    #[test]
    fn split_at_path_nested_two_levels() {
        // root=LR(connect, TB(connect, LR(connect, connect)))
        // path [1, 1] should reach the inner LR split
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![
                        connect(),
                        Pane::Split {
                            kind: Split::LeftRight,
                            children: vec![connect(), connect()],
                            ratios: vec![100, 100],
                        },
                    ],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[1, 1]);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Pane::Split { kind: Split::LeftRight, .. }));
    }

    #[test]
    fn split_at_path_out_of_bounds_returns_none() {
        let mut p = hsplit(); // 2 children
        assert!(split_at_path_mut(&mut p, &[5]).is_none());
    }

    #[test]
    fn split_at_path_into_leaf_returns_none() {
        // path [0, 0] into an LR split whose children are leaves — can't descend further
        let mut p = hsplit();
        assert!(split_at_path_mut(&mut p, &[0, 0]).is_none());
    }

    #[test]
    fn split_at_path_mut_modifies_ratios() {
        // Verify we can actually mutate the node returned by split_at_path_mut.
        let mut p = hsplit();
        if let Some(Pane::Split { ratios, .. }) = split_at_path_mut(&mut p, &[]) {
            ratios[0] = 200;
        }
        if let Pane::Split { ratios, .. } = &p {
            assert_eq!(ratios[0], 200, "mutation through split_at_path_mut should be visible");
        }
    }
}
