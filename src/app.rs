use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::browser::{FileBrowser, SshBrowser};
use crate::keybindings::KeyBindings;
use crate::pane::{Pane, pane_border_inner, pane_inner, split_areas, split_at_path_mut};
use crate::ssh_config::{SshHost, parse_ssh_config};
use crate::tab::Tab;
use crate::terminal::EmbeddedTerminal;

pub const CONTEXT_MENU_ITEMS: [&str; 5] = [
    "New tab",
    "Close tab",
    "Split left/right",
    "Split top/bottom",
    "Exit",
];
const CONTEXT_MENU_WIDTH: u16 = 22; // longest item (18) + 2 padding + 2 border
const CONTEXT_MENU_HEIGHT: u16 = 7; // 5 items + 2 border

pub struct ContextMenu {
    pub col: u16,
    pub row: u16,
    pub selected: Option<usize>,
}

/// Compute the screen rectangle for the context menu, clamped to `screen`.
/// The origin (col, row) is placed at the top-center of the menu.
pub fn context_menu_rect(col: u16, row: u16, screen: Rect) -> Rect {
    let w = CONTEXT_MENU_WIDTH;
    let h = CONTEXT_MENU_HEIGHT;
    let x = (col as i32 - w as i32 / 2).max(screen.x as i32);
    let x = (x as u16).min(screen.x + screen.width.saturating_sub(w));
    let y = row
        .max(screen.y)
        .min(screen.y + screen.height.saturating_sub(h));
    Rect::new(x, y, w, h)
}

pub struct PaneResizeDrag {
    /// Path from root to the Split node being resized.
    pub path: Vec<usize>,
    /// Separator index: between `children[sep_idx]` and `children[sep_idx + 1]`.
    pub sep_idx: usize,
    /// True for LeftRight (horizontal drag), false for TopBottom (vertical drag).
    pub horizontal: bool,
    /// Mouse coordinate at drag start (column for LR, row for TB).
    pub start_pos: u16,
    /// Original ratios of the two children adjacent to the separator.
    pub start_ratios: (u16, u16),
    /// Combined pixel span of the two adjacent children (plus separator for LR).
    pub span: u16,
    /// Screen area of the Split node — used for highlight rendering.
    pub split_area: ratatui::layout::Rect,
}

pub struct App {
    pub tabs: Vec<Tab>,
    pub selected_tab: usize,
    pub hosts: Vec<SshHost>,
    pub context_menu: Option<ContextMenu>,
    pub keybindings: KeyBindings,
    pub pane_resize_drag: Option<PaneResizeDrag>,
    next_tab_id: usize,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: parse_ssh_config(),
            context_menu: None,
            keybindings: KeyBindings::load(),
            pane_resize_drag: None,
            next_tab_id: 2,
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.selected_tab]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.selected_tab]
    }

    pub fn take_dirty(&mut self) -> bool {
        self.tabs.iter_mut().any(|t| t.root.take_dirty())
    }

    pub fn tick_browsers(&mut self) {
        for tab in &mut self.tabs {
            tab.root.tick_browsers();
        }
    }

    pub fn send_str(&mut self, s: &str) {
        if let Some(Pane::Session { terminal, .. }) = self.tab_mut().focused_pane_mut() {
            terminal.send_str(s);
        }
    }

    pub fn send_char(&mut self, c: char) {
        if let Some(Pane::Session { terminal, .. }) = self.tab_mut().focused_pane_mut() {
            terminal.send_char(c);
        }
    }

    /// Returns true when the focused browser pane is accumulating paste chars.
    /// Used to suppress unnecessary redraws during file-drop detection.
    pub fn paste_accumulating(&self) -> bool {
        self.tab()
            .focused_pane()
            .and_then(|p| p.as_browser())
            .is_some_and(|b| !b.core().paste_buf.is_empty())
    }

    pub fn open_session(&mut self, host_idx: usize, area: Rect) -> Result<()> {
        let host = self
            .hosts
            .get(host_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        self.open_session_raw(&host.label, area)
    }

    pub fn open_session_raw(&mut self, args: &str, area: Rect) -> Result<()> {
        let pane_area = self.focused_pane_area(area);
        let term_area = if self.tab().leaf_count() > 1 {
            pane_border_inner(pane_area)
        } else {
            pane_area
        };
        let term = EmbeddedTerminal::ssh_raw(term_area.height, term_area.width, args)?;
        let name = args.split_whitespace().last().unwrap_or("ssh").to_string();
        if self.tab().leaf_count() == 1 {
            self.tab_mut().name = name;
        }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::Session {
                terminal: term,
                ssh_args: args.to_string(),
                exit_selection: 0,
            };
        }
        Ok(())
    }

    pub fn open_browser(&mut self, host_idx: usize) -> Result<()> {
        let host = self
            .hosts
            .get(host_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let browser = FileBrowser::new(&host.label)?;
        if self.tab().leaf_count() == 1 {
            self.tab_mut().name = format!("sftp:{}", host.label);
        }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::FileBrowser { browser };
        }
        Ok(())
    }

    pub fn open_ssh_browser(&mut self, host_idx: usize) -> Result<()> {
        let host = self
            .hosts
            .get(host_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let browser = SshBrowser::new(&host.label)?;
        if self.tab().leaf_count() == 1 {
            self.tab_mut().name = format!("scp:{}", host.label);
        }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::SshBrowser { browser };
        }
        Ok(())
    }

    pub fn focused_pane_app_cursor(&self) -> bool {
        if let Some(Pane::Session { terminal, .. }) = self.tab().focused_pane() {
            terminal.app_cursor()
        } else {
            false
        }
    }

    pub fn focused_pane_area(&self, full: Rect) -> Rect {
        let content = pane_inner(full);
        let areas = self.tab().root.leaf_areas(content);
        areas.get(self.tab().focus_idx).copied().unwrap_or(content)
    }

    pub fn resize_all(&mut self, full: Rect) {
        let content = pane_inner(full);
        for tab in &mut self.tabs {
            let multi = tab.leaf_count() > 1;
            tab.root.resize_all(content, multi);
        }
    }

    pub fn new_tab(&mut self) {
        let name = self.next_tab_id.to_string();
        self.next_tab_id += 1;
        self.tabs.push(Tab::new(&name));
        self.selected_tab = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self) {
        self.tabs.remove(self.selected_tab);
        if self.tabs.is_empty() {
            self.next_tab_id = 2;
            self.tabs.push(Tab::new("1"));
            self.selected_tab = 0;
        } else if self.selected_tab >= self.tabs.len() {
            self.selected_tab = self.tabs.len() - 1;
        }
    }

    pub fn render(&mut self, full: Rect, buf: &mut Buffer) {
        let content = pane_inner(full);
        let tab_y = full.y + content.height;

        // Fill the entire tab bar row with a solid background.
        let bar_bg = Style::default().bg(Color::DarkGray);
        buf.set_style(
            Rect { x: full.x, y: tab_y, width: full.width, height: 1 },
            bar_bg,
        );

        // Tab entries — no separators, each tab is a styled block.
        let mut spans: Vec<Span> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let label = format!(" {} ", tab.display_name());
            if i == self.selected_tab {
                spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    label,
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
            }
        }
        buf.set_line(full.x, tab_y, &Line::from(spans), full.width);

        let focus_idx = self.tabs[self.selected_tab].focus_idx;
        let hosts = &self.hosts;
        let mut idx = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab].root.render(
            content,
            buf,
            hosts,
            focus_idx,
            leaf_count,
            &mut idx,
            &self.keybindings,
        );

        // Active resize drag: highlight the separator being dragged in Yellow.
        if let Some(ref drag) = self.pane_resize_drag {
            let highlight = Style::default().fg(Color::Yellow);
            let a = drag.split_area;
            if let Some(Pane::Split { kind, ratios, .. }) =
                split_at_path_mut(&mut self.tabs[self.selected_tab].root, &drag.path)
            {
                let areas = split_areas(a, kind, ratios);
                if drag.horizontal {
                    // Re-color the LeftRight separator column computed from current ratios.
                    if let Some(pair) = areas.windows(2).nth(drag.sep_idx) {
                        let x = pair[0].right();
                        for y in a.y..a.y + a.height {
                            buf[(x, y)].set_style(highlight);
                        }
                    }
                } else {
                    // Re-color the TopBottom title-bar row (a.y of child[sep_idx+1]).
                    if let Some(child_area) = areas.get(drag.sep_idx + 1) {
                        let ty = child_area.y;
                        for x in child_area.x..child_area.x + child_area.width {
                            buf[(x, ty)].set_style(highlight);
                        }
                    }
                }
            }
        }

        // Context menu overlay (on top of everything)
        if let Some(ref menu) = self.context_menu {
            let rect = context_menu_rect(menu.col, menu.row, full);
            // Clear background
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    buf[(x, y)].reset();
                }
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow));
            let inner = block.inner(rect);
            block.render(rect, buf);
            for (i, item) in CONTEXT_MENU_ITEMS.iter().enumerate() {
                let y = inner.y + i as u16;
                if y >= inner.y + inner.height {
                    break;
                }
                let style = if menu.selected == Some(i) {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let w = inner.width as usize;
                let pad = w.saturating_sub(item.len()) / 2;
                let label = format!("{:>pad$}{:<rest$}", "", item, pad = pad, rest = w - pad);
                let span = Span::styled(label, style);
                buf.set_line(inner.x, y, &Line::from(span), inner.width);
            }
        }
    }
}

impl App {
    /// Test-only constructor: no SSH config parsing, default keybindings, no disk I/O.
    #[cfg(test)]
    pub fn test_new() -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: vec![],
            context_menu: None,
            keybindings: KeyBindings::default(),
            pane_resize_drag: None,
            next_tab_id: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> App {
        App::test_new()
    }

    #[test]
    fn new_app_has_one_tab() {
        let app = make_app();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn new_tab_appends_and_selects() {
        let mut app = make_app();
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.selected_tab, 1);
    }

    #[test]
    fn new_tab_names_incrementally() {
        let mut app = make_app();
        app.new_tab();
        assert_eq!(app.tabs[1].name, "2");
        app.new_tab();
        assert_eq!(app.tabs[2].name, "3");
    }

    #[test]
    fn close_tab_removes_current() {
        let mut app = make_app();
        app.new_tab();
        app.new_tab();
        assert_eq!(app.tabs.len(), 3);
        app.selected_tab = 1;
        app.close_tab();
        assert_eq!(app.tabs.len(), 2);
    }

    #[test]
    fn close_last_tab_recreates_default() {
        let mut app = make_app();
        app.close_tab();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.selected_tab, 0);
        assert_eq!(app.tabs[0].name, "1");
    }

    #[test]
    fn new_tab_after_close_uses_monotonic_id() {
        let mut app = make_app();
        app.new_tab(); // "2"
        app.new_tab(); // "3"
        app.selected_tab = 0;
        app.close_tab(); // removes "1", tabs are ["2", "3"]
        app.new_tab(); // should be "4", not "3"
        assert_eq!(app.tabs.last().unwrap().name, "4");
    }

    #[test]
    fn close_all_tabs_resets_counter() {
        let mut app = make_app();
        app.new_tab(); // "2"
        app.close_tab(); // closes "2"
        app.close_tab(); // closes "1", recreates default "1"
        assert_eq!(app.tabs[0].name, "1");
        app.new_tab(); // should be "2" (counter reset)
        assert_eq!(app.tabs[1].name, "2");
    }

    #[test]
    fn close_tab_clamps_index() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 1;
        app.close_tab();
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn close_tab_middle_preserves_order() {
        let mut app = make_app();
        app.new_tab(); // "2"
        app.new_tab(); // "3"
        app.selected_tab = 1;
        app.close_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].name, "1");
        assert_eq!(app.tabs[1].name, "3");
    }

    #[test]
    fn tab_accessors() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 0;
        assert_eq!(app.tab().name, "1");
        app.tab_mut().name = "renamed".to_string();
        assert_eq!(app.tab().name, "renamed");
    }

    #[test]
    fn focused_pane_is_connect_by_default() {
        let app = make_app();
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect { .. })
        ));
    }

    // ---- context_menu_rect tests ----

    #[test]
    fn context_menu_rect_center() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(40, 10, screen);
        assert_eq!(r.width, CONTEXT_MENU_WIDTH);
        assert_eq!(r.height, CONTEXT_MENU_HEIGHT);
        assert_eq!(r.x, 40 - CONTEXT_MENU_WIDTH / 2);
        assert_eq!(r.y, 10);
    }

    #[test]
    fn context_menu_rect_clamp_left() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(2, 10, screen);
        assert_eq!(r.x, 0);
    }

    #[test]
    fn context_menu_rect_clamp_right() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(78, 10, screen);
        assert!(r.x + r.width <= screen.width);
    }

    #[test]
    fn context_menu_rect_clamp_bottom() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(40, 22, screen);
        assert!(r.y + r.height <= screen.height);
    }

    #[test]
    fn context_menu_rect_top_left_corner() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(0, 0, screen);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn context_menu_rect_bottom_right_corner() {
        let screen = Rect::new(0, 0, 80, 24);
        let r = context_menu_rect(79, 23, screen);
        assert!(r.x + r.width <= screen.width);
        assert!(r.y + r.height <= screen.height);
    }
}
