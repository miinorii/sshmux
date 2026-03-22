use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::browser::{FileBrowser, SshBrowser};
use crate::pane::{Pane, pane_inner};
use crate::ssh_config::SshHost;
use crate::tab::Tab;
use crate::terminal::EmbeddedTerminal;

pub struct App {
    pub tabs: Vec<Tab>,
    pub selected_tab: usize,
    pub hosts: Vec<SshHost>,
}

impl App {
    pub fn new() -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: crate::ssh_config::parse_ssh_config(),
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.selected_tab]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.selected_tab]
    }

    pub fn any_dirty(&mut self) -> bool {
        self.tabs.iter_mut().any(|t| t.root.any_dirty())
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
        match self.tab().focused_pane() {
            Some(Pane::FileBrowser { browser }) => !browser.core.paste_buf.is_empty(),
            Some(Pane::SshBrowser { browser }) => !browser.core.paste_buf.is_empty(),
            _ => false,
        }
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
            pane_inner(pane_area)
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
        let name = (self.tabs.len() + 1).to_string();
        self.tabs.push(Tab::new(&name));
        self.selected_tab = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self) {
        self.tabs.remove(self.selected_tab);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new("1"));
            self.selected_tab = 0;
        } else if self.selected_tab >= self.tabs.len() {
            self.selected_tab = self.tabs.len() - 1;
        }
    }

    pub fn render(&mut self, full: Rect, buf: &mut Buffer) {
        let mut spans: Vec<Span> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" │ ").style(Style::default().fg(Color::DarkGray)));
            }
            let span = Span::raw(format!(" {} ", tab.display_name()));
            if i == self.selected_tab {
                spans.push(
                    span.style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
            } else {
                spans.push(span.style(Style::default().fg(Color::White)));
            }
        }

        let outer_block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(spans));
        let content = outer_block.inner(full);
        outer_block.render(full, buf);

        let focus_idx = self.tabs[self.selected_tab].focus_idx;
        let hosts = &self.hosts;
        let mut idx = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab]
            .root
            .render(content, buf, hosts, focus_idx, leaf_count, &mut idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> App {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: vec![],
        }
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
}
