use std::sync::atomic::Ordering;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState, StatefulWidget, Widget},
};

use crate::sftp::FileBrowser;
use crate::ssh_config::SshHost;
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Split direction
// ---------------------------------------------------------------------------

pub enum Split { Horizontal, Vertical }

// ---------------------------------------------------------------------------
// Pane
// ---------------------------------------------------------------------------

pub enum Pane {
    Connect     { list_state: ListState },
    Session     { terminal: EmbeddedTerminal },
    FileBrowser { browser: FileBrowser },
    Split       { kind: Split, children: Vec<Pane> },
}

impl Pane {
    pub fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect { list_state: ls }
    }

    pub fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => vec![area],
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                children.iter().zip(areas).flat_map(|(c, a)| c.leaf_areas(a)).collect()
            }
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count { return child.leaf_mut(n - offset); }
                    offset += count;
                }
                None
            }
        }
    }

    pub fn leaf(&self, n: usize) -> Option<&Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count { return child.leaf(n - offset); }
                    offset += count;
                }
                None
            }
        }
    }

    pub fn split_leaf(&mut self, n: usize, kind: Split) {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {}
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for (i, child) in children.iter_mut().enumerate() {
                    let count = child.leaf_count();
                    if n < offset + count {
                        if count == 1 {
                            let old = std::mem::replace(child, Pane::new_connect());
                            *child = Pane::Split { kind, children: vec![old, Pane::new_connect()] };
                        } else {
                            child.split_leaf(n - offset, kind);
                        }
                        break;
                    }
                    offset += count;
                    let _ = i;
                }
            }
        }
    }

    pub fn any_dirty(&mut self) -> bool {
        match self {
            Pane::Session { terminal } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::FileBrowser { browser } => {
                let pty_dirty   = browser.sftp.dirty.swap(false, Ordering::AcqRel);
                let state_dirty = browser.needs_redraw;
                browser.needs_redraw = false;
                pty_dirty || state_dirty
            }
            Pane::Split { children, .. } => children.iter_mut().any(|c| c.any_dirty()),
            _ => false,
        }
    }

    pub fn tick_browsers(&mut self) {
        match self {
            Pane::FileBrowser { browser } => browser.tick(),
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
            Pane::FileBrowser { .. } => {}
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
            Pane::Connect { list_state } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus { Style::default().fg(Color::Blue) } else { Style::default().fg(Color::DarkGray) };
                    let block = Block::default().borders(Borders::ALL).border_style(border_style).title(" connect ");
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else { area };

                const HELP_LINES: u16 = 8;
                let list_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: inner.height.saturating_sub(HELP_LINES + 1) };
                let help_area = Rect { x: inner.x, y: inner.y + inner.height.saturating_sub(HELP_LINES), width: inner.width, height: HELP_LINES };

                let items: Vec<&str> = hosts.iter().map(|h| h.label.as_str()).collect();
                let list = List::new(items)
                    .style(Style::default().fg(Color::White))
                    .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                    .highlight_symbol("> ");
                StatefulWidget::render(list, list_area, buf, list_state);

                let shortcuts = [
                    ("Alt+T",  "new tab"),
                    ("Alt+W",  "close pane / tab"),
                    ("Alt+-",  "split vertical"),
                    ("Alt++",  "split horizontal"),
                    ("Alt+B",  "open file browser"),
                    ("Alt+↑↓", "cycle pane focus"),
                    ("Alt+←→", "switch tab"),
                    ("Ctrl+C", "quit"),
                ];
                for (i, (key, desc)) in shortcuts.iter().enumerate() {
                    let y = help_area.y + i as u16;
                    if y >= help_area.y + help_area.height { break; }
                    buf.set_line(help_area.x, y, &Line::from(vec![
                        Span::raw(format!("  {:10}", key)).style(Style::default().fg(Color::Yellow)),
                        Span::raw(*desc).style(Style::default().fg(Color::DarkGray)),
                    ]), help_area.width);
                }
            }

            Pane::Session { terminal } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus { Style::default().fg(Color::Blue) } else { Style::default().fg(Color::DarkGray) };
                    let block = Block::default().borders(Borders::ALL).border_style(border_style);
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else { area };
                terminal.render_into(inner, buf);
            }

            Pane::FileBrowser { browser } => {
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
    if count == 0 { return vec![]; }
    match kind {
        Split::Horizontal => {
            let w = area.width / count as u16;
            (0..count).map(|i| Rect {
                x:      area.x + i as u16 * w,
                y:      area.y,
                width:  if i == count - 1 { area.width - i as u16 * w } else { w },
                height: area.height,
            }).collect()
        }
        Split::Vertical => {
            let h = area.height / count as u16;
            (0..count).map(|i| Rect {
                x:      area.x,
                y:      area.y + i as u16 * h,
                width:  area.width,
                height: if i == count - 1 { area.height - i as u16 * h } else { h },
            }).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// remove_leaf
// ---------------------------------------------------------------------------

pub fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {}
        Pane::Split { children, .. } => {
            let mut offset = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 { to_remove = Some(i); } else { remove_leaf(child, n - offset); }
                    break;
                }
                offset += count;
            }
            if let Some(i) = to_remove { children.remove(i); }
        }
    }
}

// ---------------------------------------------------------------------------
// pane_inner
// ---------------------------------------------------------------------------

/// The drawable area inside a pane's own border (1-cell inset on all sides).
pub fn pane_inner(area: Rect) -> Rect {
    Rect {
        x:      area.x + 1,
        y:      area.y + 1,
        width:  area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}
