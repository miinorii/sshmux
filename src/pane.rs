use std::sync::atomic::Ordering;

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState, Paragraph, StatefulWidget, Widget},
};

use crate::browser::common::Browser;
use crate::browser::{FileBrowser, SshBrowser};
use crate::ssh_config::SshHost;
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Split direction
// ---------------------------------------------------------------------------

pub enum Split {
    Horizontal,
    Vertical,
}

// ---------------------------------------------------------------------------
// Pane
// ---------------------------------------------------------------------------

pub enum Pane {
    Connect {
        list_state: ListState,
        browser_menu: Option<ListState>,
        connect_input: Option<String>,
        show_help: bool,
    },
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
    },
}

impl Pane {
    pub fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect {
            list_state: ls,
            browser_menu: None,
            connect_input: None,
            show_help: false,
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
            Pane::Connect { .. }
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => vec![area],
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
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
            Pane::Connect { .. }
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect { .. }
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
            Pane::Connect { .. }
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

    pub fn split_leaf(&mut self, n: usize, kind: Split) {
        match self {
            Pane::Connect { .. }
            | Pane::Session { .. }
            | Pane::FileBrowser { .. }
            | Pane::SshBrowser { .. } => {}
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
                            };
                        } else {
                            child.split_leaf(n - offset, kind);
                        }
                        break;
                    }
                    offset += count;
                }
            }
        }
    }

    pub fn any_dirty(&mut self) -> bool {
        match self {
            Pane::Session { terminal, .. } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::FileBrowser { browser } => {
                let pty_dirty = browser.sftp.dirty.swap(false, Ordering::AcqRel);
                let state_dirty = browser.core.needs_redraw;
                browser.core.needs_redraw = false;
                pty_dirty || state_dirty
            }
            Pane::SshBrowser { browser } => {
                let pty_dirty = browser.ssh.dirty.swap(false, Ordering::AcqRel);
                let scp_dirty = browser
                    .scp_pty
                    .as_ref()
                    .map(|s| s.dirty.swap(false, Ordering::AcqRel))
                    .unwrap_or(false);
                let state_dirty = browser.core.needs_redraw;
                browser.core.needs_redraw = false;
                pty_dirty || scp_dirty || state_dirty
            }
            Pane::Split { children, .. } => children.iter_mut().any(|c| c.any_dirty()),
            _ => false,
        }
    }

    pub fn tick_browsers(&mut self) {
        match self {
            Pane::FileBrowser { browser } => browser.tick(),
            Pane::SshBrowser { browser } => browser.tick(),
            Pane::Split { children, .. } => children.iter_mut().for_each(|c| c.tick_browsers()),
            _ => {}
        }
    }

    pub fn resize_all(&mut self, area: Rect, multi_pane: bool) {
        match self {
            Pane::Session { terminal, .. } => {
                let (h, w) = if multi_pane {
                    (area.height.saturating_sub(2), area.width.saturating_sub(2))
                } else {
                    (area.height, area.width)
                };
                terminal.resize(h, w);
            }
            Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => {}
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    child.resize_all(a, true);
                }
            }
            _ => {}
        }
    }

    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        hosts: &[SshHost],
        focus_idx: usize,
        leaf_count: usize,
        my_idx: &mut usize,
    ) {
        match self {
            Pane::Connect {
                list_state,
                browser_menu,
                connect_input,
                show_help,
            } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = render_pane_border(area, buf, is_focus, leaf_count, Some(" connect "));

                let list_area = Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: inner.height.saturating_sub(1),
                };
                let hint_y = inner.y + inner.height.saturating_sub(1);

                let items: Vec<&str> = hosts.iter().map(|h| h.label.as_str()).collect();
                let list = List::new(items)
                    .style(Style::default().fg(Color::White))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                StatefulWidget::render(list, list_area, buf, list_state);

                buf.set_line(
                    inner.x,
                    hint_y,
                    &Line::from(vec![
                        Span::raw("  H").style(Style::default().fg(Color::Yellow)),
                        Span::raw(" help").style(Style::default().fg(Color::DarkGray)),
                    ]),
                    inner.width,
                );

                // Browser type picker overlay
                if let Some(menu_state) = browser_menu {
                    let menu_w = 36u16;
                    let menu_h = 4u16; // border + 2 items + border
                    let cx = inner.x + inner.width.saturating_sub(menu_w) / 2;
                    let cy = inner.y + inner.height.saturating_sub(menu_h) / 2;
                    let menu_area = Rect {
                        x: cx,
                        y: cy,
                        width: menu_w.min(inner.width),
                        height: menu_h.min(inner.height),
                    };
                    let menu_items = vec!["SFTP", "SCP (legacy, linux target)"];
                    let menu_list = List::new(menu_items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Yellow))
                                .title(" Browse with "),
                        )
                        .style(Style::default().fg(Color::White))
                        .highlight_style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");
                    StatefulWidget::render(menu_list, menu_area, buf, menu_state);
                }

                // Help overlay
                if *show_help {
                    let shortcuts = [
                        ("Enter", "connect"),
                        ("C", "connect (manual)"),
                        ("B", "file browser"),
                        ("Alt+T", "new tab"),
                        ("Alt+W", "close pane / tab"),
                        ("Alt+-", "split top/bottom"),
                        ("Alt++", "split left/right"),
                        ("Alt+\u{2191}\u{2193}", "cycle pane focus"),
                        ("Alt+\u{2190}\u{2192}", "switch tab"),
                        ("Alt+Q", "quit"),
                    ];
                    let help_w = 36u16.min(inner.width.saturating_sub(2));
                    let help_h = (shortcuts.len() as u16 + 2).min(inner.height);
                    let cx = inner.x + inner.width.saturating_sub(help_w) / 2;
                    let cy = inner.y + inner.height.saturating_sub(help_h) / 2;
                    let help_area = Rect {
                        x: cx,
                        y: cy,
                        width: help_w,
                        height: help_h,
                    };
                    let items: Vec<Line> = shortcuts
                        .iter()
                        .map(|(key, desc)| {
                            Line::from(vec![
                                Span::raw(format!(" {:10}", key))
                                    .style(Style::default().fg(Color::Yellow)),
                                Span::raw(*desc).style(Style::default().fg(Color::DarkGray)),
                            ])
                        })
                        .collect();
                    let help_list = List::new(items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" shortcuts "),
                    );
                    Widget::render(help_list, help_area, buf);
                }

                // Connect input overlay
                if let Some(input) = connect_input {
                    let input_w = 50u16.min(inner.width.saturating_sub(2));
                    let input_h = 4u16;
                    let cx = inner.x + inner.width.saturating_sub(input_w) / 2;
                    let cy = inner.y + inner.height.saturating_sub(input_h) / 2;
                    let input_area = Rect {
                        x: cx,
                        y: cy,
                        width: input_w,
                        height: input_h,
                    };
                    let display = format!("{}_", input);
                    let paragraph = Paragraph::new(vec![
                        Line::from(Span::raw(display).style(Style::default().fg(Color::White))),
                        Line::from(
                            Span::raw("e.g. -o StrictHostKeyChecking=no user@host")
                                .style(Style::default().fg(Color::DarkGray)),
                        ),
                    ])
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" ssh "),
                    );
                    paragraph.render(input_area, buf);
                }
            }

            Pane::Session {
                terminal,
                exit_selection,
                ..
            } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = render_pane_border(area, buf, is_focus, leaf_count, None);
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
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count);
            }

            Pane::SshBrowser { browser } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count);
            }

            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    child.render(a, buf, hosts, focus_idx, leaf_count, my_idx);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// split_areas
// ---------------------------------------------------------------------------

pub fn split_areas(area: Rect, kind: &Split, count: usize) -> Vec<Rect> {
    if count == 0 {
        return vec![];
    }
    let direction = match kind {
        Split::Horizontal => Direction::Horizontal,
        Split::Vertical => Direction::Vertical,
    };
    let constraints = vec![Constraint::Fill(1); count];
    Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

// ---------------------------------------------------------------------------
// pane_inner / render_pane_border
// ---------------------------------------------------------------------------

/// The drawable area inside a pane's own border (1-cell inset on all sides).
pub fn pane_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// Render a pane border when in multi-pane mode and return the inner area.
/// In single-pane mode the full area is returned unchanged.
pub fn render_pane_border(
    area: Rect,
    buf: &mut Buffer,
    is_focus: bool,
    leaf_count: usize,
    title: Option<&str>,
) -> Rect {
    if leaf_count > 1 {
        let border_style = if is_focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style);
        if let Some(t) = title {
            block = block.title(t);
        }
        let inner = block.inner(area);
        block.render(area, buf);
        inner
    } else {
        area
    }
}

// ---------------------------------------------------------------------------
// remove_leaf
// ---------------------------------------------------------------------------

pub fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect { .. }
        | Pane::Session { .. }
        | Pane::FileBrowser { .. }
        | Pane::SshBrowser { .. } => {}
        Pane::Split { children, .. } => {
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
    use ratatui::layout::Rect;

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
            kind: Split::Horizontal,
            children: vec![connect(), connect()],
        }
    }
    fn vsplit() -> Pane {
        Pane::Split {
            kind: Split::Vertical,
            children: vec![connect(), connect()],
        }
    }

    // ---- split_areas -------------------------------------------------------

    #[test]
    fn split_areas_horizontal_even() {
        let a = split_areas(r(100, 20), &Split::Horizontal, 2);
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
                x: 50,
                y: 0,
                width: 50,
                height: 20
            }
        );
    }

    #[test]
    fn split_areas_horizontal_remainder() {
        let a = split_areas(r(101, 20), &Split::Horizontal, 2);
        assert_eq!(a[0].width + a[1].width, 101);
    }

    #[test]
    fn split_areas_vertical_even() {
        let a = split_areas(r(80, 40), &Split::Vertical, 2);
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
        let a = split_areas(r(80, 30), &Split::Vertical, 3);
        assert_eq!(a.len(), 3);
        assert_eq!(a.iter().map(|x| x.height).sum::<u16>(), 30);
    }

    #[test]
    fn split_areas_empty() {
        assert!(split_areas(r(80, 40), &Split::Horizontal, 0).is_empty());
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
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
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
        assert!(matches!(p.leaf(0), Some(Pane::Connect { .. })));
        assert!(p.leaf(2).is_none());
    }

    #[test]
    fn leaf_areas_covers_full() {
        assert_eq!(connect().leaf_areas(r(100, 50)), vec![r(100, 50)]);
    }

    #[test]
    fn leaf_areas_sum_equals_parent() {
        let a = hsplit().leaf_areas(r(100, 50));
        assert_eq!(a[0].width + a[1].width, 100);
    }

    #[test]
    fn leaf_areas_count_matches_leaf_count() {
        let p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
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
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
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
        let inner = pane_inner(r(10, 8));
        assert_eq!(inner.x, 1);
        assert_eq!(inner.y, 1);
        assert_eq!(inner.width, 8);
        assert_eq!(inner.height, 6);
    }

    #[test]
    fn pane_inner_saturates_at_zero() {
        let inner = pane_inner(Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        assert_eq!(inner.width, 0);
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
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
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
        assert!(matches!(p.leaf(1), Some(Pane::Connect { .. })));
    }

    // ---- split_leaf --------------------------------------------------------

    #[test]
    fn split_leaf_increases_count() {
        let mut p = hsplit();
        p.split_leaf(0, Split::Vertical);
        assert_eq!(p.leaf_count(), 3);
    }

    #[test]
    fn split_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
        };
        p.split_leaf(2, Split::Horizontal);
        assert_eq!(p.leaf_count(), 4);
    }

    #[test]
    fn split_leaf_noop_on_single() {
        // split_leaf on a non-Split pane is a no-op
        let mut p = connect();
        p.split_leaf(0, Split::Horizontal);
        assert_eq!(p.leaf_count(), 1);
    }

    // ---- remove_leaf (additional) ------------------------------------------

    #[test]
    fn remove_leaf_collapses_to_single() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        // After removing one child from a 2-child split, it collapses
        assert!(matches!(p, Pane::Connect { .. }));
    }

    #[test]
    fn remove_leaf_deep_nested() {
        // 4-leaf tree: split(connect, split(connect, split(connect, connect)))
        let mut p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::Vertical,
                    children: vec![
                        connect(),
                        Pane::Split {
                            kind: Split::Horizontal,
                            children: vec![connect(), connect()],
                        },
                    ],
                },
            ],
        };
        assert_eq!(p.leaf_count(), 4);
        remove_leaf(&mut p, 3); // remove deepest right leaf
        assert_eq!(p.leaf_count(), 3);
    }

    // ---- split_areas (additional) ------------------------------------------

    #[test]
    fn split_areas_single_element() {
        let a = split_areas(r(100, 50), &Split::Horizontal, 1);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0], r(100, 50));
    }

    #[test]
    fn split_areas_vertical_remainder() {
        let a = split_areas(r(80, 31), &Split::Vertical, 2);
        assert_eq!(a[0].height + a[1].height, 31);
    }
}
