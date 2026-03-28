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
    LeftRight,
    TopBottom,
}

// ---------------------------------------------------------------------------
// Connect pane overlay (mutually exclusive states)
// ---------------------------------------------------------------------------

pub enum ConnectOverlay {
    None,
    BrowserMenu(ListState),
    ConnectInput(String),
    KeyEditor(KeyEditorState),
}

pub struct KeyEditorState {
    pub list_state: ListState,
    pub editing: bool,
    pub status: Option<String>,
}

impl KeyEditorState {
    pub fn new() -> Self {
        let mut ls = ListState::default();
        ls.select(Some(1)); // first binding (index 0 is a header)
        Self {
            list_state: ls,
            editing: false,
            status: None,
        }
    }
}

/// Number of bindings per group (for header index calculation).
const GLOBAL_COUNT: usize = 9;
const CONNECT_COUNT: usize = 6;

/// Header indices in the flat display list.
const HEADER_GLOBAL: usize = 0;
const HEADER_CONNECT: usize = GLOBAL_COUNT + 1; // 10
const HEADER_BROWSER: usize = GLOBAL_COUNT + 1 + CONNECT_COUNT + 1; // 17

/// Total rows in the editor list (3 headers + 24 bindings).
const EDITOR_ROW_COUNT: usize = 27;

/// Returns true if the given index is a section header row.
pub fn is_editor_header(idx: usize) -> bool {
    idx == HEADER_GLOBAL || idx == HEADER_CONNECT || idx == HEADER_BROWSER
}

/// Map a display index to a binding entry index (0..26), or None for headers.
pub fn editor_binding_index(display_idx: usize) -> Option<usize> {
    if is_editor_header(display_idx) {
        return None;
    }
    let binding_idx = if display_idx < HEADER_CONNECT {
        display_idx - 1 // subtract global header
    } else if display_idx < HEADER_BROWSER {
        display_idx - 2 // subtract global + connect headers
    } else {
        display_idx - 3 // subtract all 3 headers
    };
    Some(binding_idx)
}

/// Move selection to next non-header row (wrapping).
pub fn editor_nav_down(list_state: &mut ListState) {
    let cur = list_state.selected().unwrap_or(0);
    let mut next = cur + 1;
    if next >= EDITOR_ROW_COUNT {
        next = 1; // wrap to first binding
    }
    if is_editor_header(next) {
        next += 1;
    }
    list_state.select(Some(next));
}

/// Move selection to previous non-header row (wrapping).
pub fn editor_nav_up(list_state: &mut ListState) {
    let cur = list_state.selected().unwrap_or(0);
    let mut prev = if cur == 0 {
        EDITOR_ROW_COUNT - 1
    } else {
        cur - 1
    };
    if is_editor_header(prev) {
        prev = if prev == 0 {
            EDITOR_ROW_COUNT - 1
        } else {
            prev - 1
        };
    }
    list_state.select(Some(prev));
}

// ---------------------------------------------------------------------------
// Pane
// ---------------------------------------------------------------------------

pub enum Pane {
    Connect {
        list_state: ListState,
        overlay: ConnectOverlay,
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
            overlay: ConnectOverlay::None,
        }
    }

    /// Returns `true` if this pane is a browser (SFTP or SCP).
    pub fn is_browser(&self) -> bool {
        matches!(self, Pane::FileBrowser { .. } | Pane::SshBrowser { .. })
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

    pub fn split_leaf(&mut self, n: usize, kind: Split) -> bool {
        match self {
            Pane::Connect { .. }
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
            Pane::Split { children, .. } => children.iter_mut().any(|c| c.take_dirty()),
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

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        hosts: &[SshHost],
        focus_idx: usize,
        leaf_count: usize,
        my_idx: &mut usize,
        keybindings: &crate::keybindings::KeyBindings,
    ) {
        match self {
            Pane::Connect {
                list_state,
                overlay,
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

                let help_key = format!("  {}", keybindings.connect.help);
                buf.set_line(
                    inner.x,
                    hint_y,
                    &Line::from(vec![
                        Span::raw(help_key).style(Style::default().fg(Color::Yellow)),
                        Span::raw(" keybindings").style(Style::default().fg(Color::DarkGray)),
                    ]),
                    inner.width,
                );

                match overlay {
                    ConnectOverlay::BrowserMenu(menu_state) => {
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
                    ConnectOverlay::KeyEditor(editor) => {
                        let entries = keybindings.entries();
                        let selected_display = editor.list_state.selected().unwrap_or(0);
                        let groups = ["Global", "Connect", "Browser"];
                        let header_indices = [HEADER_GLOBAL, HEADER_CONNECT, HEADER_BROWSER];

                        let mut items: Vec<Line> = Vec::with_capacity(EDITOR_ROW_COUNT);
                        let mut entry_idx = 0usize;
                        for row in 0..EDITOR_ROW_COUNT {
                            if let Some(gi) = header_indices.iter().position(|&h| h == row) {
                                // Section header
                                let label = format!(" ── {} ", groups[gi]);
                                items.push(Line::from(Span::styled(
                                    label,
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::BOLD),
                                )));
                            } else {
                                // Binding row
                                let e = &entries[entry_idx];
                                entry_idx += 1;
                                let key_str = if editor.editing && row == selected_display {
                                    "[press key...]".to_string()
                                } else {
                                    e.binding.to_string()
                                };
                                let key_style = if editor.editing && row == selected_display {
                                    Style::default()
                                        .fg(Color::Cyan)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::Yellow)
                                };
                                items.push(Line::from(vec![
                                    Span::styled(format!(" {:14}", key_str), key_style),
                                    Span::styled(
                                        e.description,
                                        Style::default().fg(Color::DarkGray),
                                    ),
                                ]));
                            }
                        }

                        let title = if editor.editing {
                            " press a key to bind "
                        } else {
                            " keybindings (Enter to edit) "
                        };

                        // Status line at bottom if present
                        let status_line = editor.status.as_deref().unwrap_or("");
                        let extra_h = if status_line.is_empty() { 0u16 } else { 1 };

                        let ed_w = 44u16.min(inner.width.saturating_sub(2));
                        let ed_h = (EDITOR_ROW_COUNT as u16 + 2 + extra_h).min(inner.height);
                        let cx = inner.x + inner.width.saturating_sub(ed_w) / 2;
                        let cy = inner.y + inner.height.saturating_sub(ed_h) / 2;
                        let ed_area = Rect {
                            x: cx,
                            y: cy,
                            width: ed_w,
                            height: ed_h,
                        };

                        let ed_list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Yellow))
                                    .title(title),
                            )
                            .highlight_style(
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("> ");
                        StatefulWidget::render(ed_list, ed_area, buf, &mut editor.list_state);

                        // Render status at bottom of overlay (inside border)
                        if !status_line.is_empty() {
                            let status_y = ed_area.y + ed_area.height.saturating_sub(2);
                            buf.set_line(
                                ed_area.x + 2,
                                status_y,
                                &Line::from(Span::styled(
                                    status_line,
                                    Style::default().fg(Color::Green),
                                )),
                                ed_area.width.saturating_sub(4),
                            );
                        }
                    }
                    ConnectOverlay::ConnectInput(input) => {
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
                    ConnectOverlay::None => {}
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
                    child.render(a, buf, hosts, focus_idx, leaf_count, my_idx, keybindings);
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
        Split::LeftRight => Direction::Horizontal,
        Split::TopBottom => Direction::Vertical,
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
            kind: Split::LeftRight,
            children: vec![connect(), connect()],
        }
    }
    fn vsplit() -> Pane {
        Pane::Split {
            kind: Split::TopBottom,
            children: vec![connect(), connect()],
        }
    }

    // ---- split_areas -------------------------------------------------------

    #[test]
    fn split_areas_horizontal_even() {
        let a = split_areas(r(100, 20), &Split::LeftRight, 2);
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
        let a = split_areas(r(101, 20), &Split::LeftRight, 2);
        assert_eq!(a[0].width + a[1].width, 101);
    }

    #[test]
    fn split_areas_vertical_even() {
        let a = split_areas(r(80, 40), &Split::TopBottom, 2);
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
        let a = split_areas(r(80, 30), &Split::TopBottom, 3);
        assert_eq!(a.len(), 3);
        assert_eq!(a.iter().map(|x| x.height).sum::<u16>(), 30);
    }

    #[test]
    fn split_areas_empty() {
        assert!(split_areas(r(80, 40), &Split::LeftRight, 0).is_empty());
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
            kind: Split::LeftRight,
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
            kind: Split::LeftRight,
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
            kind: Split::LeftRight,
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
        p.split_leaf(0, Split::TopBottom);
        assert_eq!(p.leaf_count(), 3);
    }

    #[test]
    fn split_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
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
        assert!(matches!(p, Pane::Connect { .. }));
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
        let a = split_areas(r(100, 50), &Split::LeftRight, 1);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0], r(100, 50));
    }

    #[test]
    fn split_areas_vertical_remainder() {
        let a = split_areas(r(80, 31), &Split::TopBottom, 2);
        assert_eq!(a[0].height + a[1].height, 31);
    }
}
