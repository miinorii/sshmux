use std::sync::{Arc, Mutex};

use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::pane::{Pane, pane_inner};
use crate::sftp::FileBrowser;
use crate::ssh_config::SshHost;
use crate::tab::Tab;
use crate::terminal::EmbeddedTerminal;

pub struct App {
    pub tabs:         Vec<Tab>,
    pub selected_tab: usize,
    pub hosts:        Vec<SshHost>,
    pub log:          Option<Arc<Mutex<std::fs::File>>>,
    pub drag_origin:  Option<usize>,
}

impl App {
    pub fn new(log: Option<Arc<Mutex<std::fs::File>>>) -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: crate::ssh_config::parse_ssh_config(),
            log,
            drag_origin: None,
        }
    }

    pub fn tab(&self)     -> &Tab     { &self.tabs[self.selected_tab] }
    pub fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.selected_tab] }

    pub fn any_dirty(&mut self) -> bool {
        self.tabs.iter_mut().any(|t| t.root.any_dirty())
    }

    pub fn tick_browsers(&mut self) {
        for tab in &mut self.tabs { tab.root.tick_browsers(); }
    }

    pub fn send_str(&mut self, s: &str) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_str(s);
        }
    }

    pub fn send_char(&mut self, c: char) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_char(c);
        }
    }

    pub fn open_session(&mut self, host_idx: usize, area: Rect) -> Result<()> {
        let host = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let pane_area = self.focused_pane_area(area);
        let term_area = if self.tab().leaf_count() > 1 { pane_inner(pane_area) } else { pane_area };
        let term = EmbeddedTerminal::ssh(term_area.height, term_area.width, &host.label, self.log.clone())?;
        if self.tab().leaf_count() == 1 { self.tab_mut().name = host.label.clone(); }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::Session { terminal: term };
        }
        Ok(())
    }

    pub fn open_browser(&mut self, host_idx: usize) -> Result<()> {
        let host = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let browser = FileBrowser::new(&host.label, self.log.clone())?;
        if self.tab().leaf_count() == 1 { self.tab_mut().name = format!("sftp:{}", host.label); }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::FileBrowser { browser };
        }
        Ok(())
    }

    pub fn focused_pane_area(&self, full: Rect) -> Rect {
        let content = content_area(full);
        let areas   = self.tab().root.leaf_areas(content);
        areas.get(self.tab().focus_idx).copied().unwrap_or(content)
    }

    pub fn resize_all(&mut self, full: Rect) {
        let content = content_area(full);
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
            if i > 0 { spans.push(Span::raw(" │ ").style(Style::default().fg(Color::DarkGray))); }
            let span = Span::raw(format!(" {} ", tab.display_name()));
            if i == self.selected_tab {
                spans.push(span.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(span.style(Style::default().fg(Color::White)));
            }
        }

        let outer_block = Block::default().borders(Borders::ALL).title(Line::from(spans));
        let content     = outer_block.inner(full);
        outer_block.render(full, buf);

        let focus_idx  = self.tabs[self.selected_tab].focus_idx;
        let hosts      = &self.hosts;
        let mut idx    = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab].root.render(content, buf, hosts, focus_idx, leaf_count, &mut idx);
    }
}

/// The drawable area inside the outer application border (1-cell inset on all sides).
pub fn content_area(full: Rect) -> Rect {
    Rect {
        x:      full.x + 1,
        y:      full.y + 1,
        width:  full.width.saturating_sub(2),
        height: full.height.saturating_sub(2),
    }
}
