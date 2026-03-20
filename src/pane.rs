use std::sync::atomic::Ordering;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState, StatefulWidget, Widget},
};

use crate::sftp::FileBrowser;
use crate::ssh_browser::SshBrowser;
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
    },
    Session { terminal: EmbeddedTerminal },
    FileBrowser { browser: FileBrowser },
    SshBrowser { browser: SshBrowser },
    Split { kind: Split, children: Vec<Pane> },
}

impl Pane {
    pub fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect {
            list_state: ls,
            browser_menu: None,
        }
    }

    pub fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => vec![area],
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
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => {
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
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => {
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
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => {}
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
            Pane::Session { terminal } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::FileBrowser { browser } => {
                let pty_dirty = browser.sftp.dirty.swap(false, Ordering::AcqRel);
                let state_dirty = browser.needs_redraw;
                browser.needs_redraw = false;
                pty_dirty || state_dirty
            }
            Pane::SshBrowser { browser } => {
                let pty_dirty = browser.ssh.dirty.swap(false, Ordering::AcqRel);
                let scp_dirty = browser
                    .scp_pty
                    .as_ref()
                    .map(|s| s.dirty.swap(false, Ordering::AcqRel))
                    .unwrap_or(false);
                let state_dirty = browser.needs_redraw;
                browser.needs_redraw = false;
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
            Pane::Session { terminal } => {
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
            } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title(" connect ");
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else {
                    area
                };

                const HELP_LINES: u16 = 8;
                let list_area = Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: inner.height.saturating_sub(HELP_LINES + 1),
                };
                let help_area = Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(HELP_LINES),
                    width: inner.width,
                    height: HELP_LINES,
                };

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

                let shortcuts = [
                    ("Alt+T", "new tab"),
                    ("Alt+W", "close pane / tab"),
                    ("Alt+-", "split vertical"),
                    ("Alt++", "split horizontal"),
                    ("B", "file browser"),
                    ("Alt+\u{2191}\u{2193}", "cycle pane focus"),
                    ("Alt+\u{2190}\u{2192}", "switch tab"),
                    ("Ctrl+C", "quit"),
                ];
                for (i, (key, desc)) in shortcuts.iter().enumerate() {
                    let y = help_area.y + i as u16;
                    if y >= help_area.y + help_area.height {
                        break;
                    }
                    buf.set_line(
                        help_area.x,
                        y,
                        &Line::from(vec![
                            Span::raw(format!("  {:10}", key))
                                .style(Style::default().fg(Color::Yellow)),
                            Span::raw(*desc).style(Style::default().fg(Color::DarkGray)),
                        ]),
                        help_area.width,
                    );
                }

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
            }

            Pane::Session { terminal } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else {
                    area
                };
                terminal.render_into(inner, buf);
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
    match kind {
        Split::Horizontal => {
            let w = area.width / count as u16;
            (0..count)
                .map(|i| Rect {
                    x: area.x + i as u16 * w,
                    y: area.y,
                    width: if i == count - 1 {
                        area.width - i as u16 * w
                    } else {
                        w
                    },
                    height: area.height,
                })
                .collect()
        }
        Split::Vertical => {
            let h = area.height / count as u16;
            (0..count)
                .map(|i| Rect {
                    x: area.x,
                    y: area.y + i as u16 * h,
                    width: area.width,
                    height: if i == count - 1 {
                        area.height - i as u16 * h
                    } else {
                        h
                    },
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// remove_leaf
// ---------------------------------------------------------------------------

pub fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } | Pane::SshBrowser { .. } => {}
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
    if let Pane::Split { children, .. } = pane {
        if children.len() == 1 {
            *pane = children.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// pane_inner
// ---------------------------------------------------------------------------

/// The drawable area inside a pane's own border (1-cell inset on all sides).
pub fn pane_inner(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
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
    fn split_areas_horizontal_remainder_to_last() {
        let a = split_areas(r(101, 20), &Split::Horizontal, 2);
        assert_eq!(a[0].width + a[1].width, 101);
        assert_eq!(a[1].width, 51);
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
}
