use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use log::{info, warn};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, StatefulWidget, Widget},
};

use super::browser_layout;
use super::parse::{FsEntry, list_drives, read_local_dir};
use crate::pane::{pane_inner, render_pane_border};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BrowserFocus {
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Clone)]
pub struct TransferStatus {
    pub filename: String,
    pub direction: TransferDirection,
    pub is_dir: bool,
    pub done: bool,
    pub progress: String,
    pub file_count: usize,
}

/// Action returned by `handle_browser_key` for browser-specific operations.
pub enum BrowserKeyAction {
    Handled,
    Enter,
    GoUp,
    Download,
    Upload,
    Delete,
    ConfirmDeleteYes,
}

/// Result of a drag-release gesture across panels.
pub enum DragAction {
    LocalToRemote,
    RemoteToLocal,
}

// ---------------------------------------------------------------------------
// BrowserCore — shared state for both SFTP and SCP browsers
// ---------------------------------------------------------------------------

pub struct BrowserCore {
    pub host: String,

    pub local_path: PathBuf,
    pub local_entries: Vec<FsEntry>,
    pub local_sel: ListState,

    pub remote_path: String,
    pub remote_entries: Vec<FsEntry>,
    pub remote_sel: ListState,

    pub focus: BrowserFocus,
    pub last_transfer: Option<TransferStatus>,
    pub status_msg: String,
    pub raw_snapshot: Vec<String>,
    pub needs_redraw: bool,
    pub confirm_delete: Option<String>,
    pub pending_delete_name: Option<String>,
    pub drive_picker: Option<(Vec<PathBuf>, ListState)>,
    pub status_color: Color,
    pub cmd_start: Option<Instant>,
    pub last_duration: Option<Duration>,
    pub local_scroll_x: usize,
    pub remote_scroll_x: usize,
    pub prompt_stable: u8,
    pub prev_raw_len: usize,
}

impl BrowserCore {
    pub fn new(host: &str) -> Self {
        let local_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local_entries = read_local_dir(&local_path);
        let mut local_sel = ListState::default();
        local_sel.select_first();
        let mut remote_sel = ListState::default();
        remote_sel.select_first();

        BrowserCore {
            host: host.to_string(),
            local_path,
            local_entries,
            local_sel,
            remote_path: String::from("."),
            remote_entries: vec![],
            remote_sel,
            focus: BrowserFocus::Local,
            last_transfer: None,
            status_msg: String::from("Connecting…"),
            raw_snapshot: vec![],
            needs_redraw: false,
            confirm_delete: None,
            pending_delete_name: None,
            drive_picker: None,
            status_color: Color::Yellow,
            cmd_start: None,
            last_duration: None,
            local_scroll_x: 0,
            remote_scroll_x: 0,
            prompt_stable: 0,
            prev_raw_len: 0,
        }
    }

    // ---- navigation --------------------------------------------------------

    pub fn toggle_focus(&mut self) {
        self.dismiss_drive_picker();
        self.focus = match self.focus {
            BrowserFocus::Local => BrowserFocus::Remote,
            BrowserFocus::Remote => BrowserFocus::Local,
        };
    }

    pub fn scroll_left(&mut self) {
        let sx = match self.focus {
            BrowserFocus::Local => &mut self.local_scroll_x,
            BrowserFocus::Remote => &mut self.remote_scroll_x,
        };
        if *sx > 0 {
            *sx = sx.saturating_sub(4);
            self.needs_redraw = true;
        }
    }

    pub fn scroll_right(&mut self) {
        let sx = match self.focus {
            BrowserFocus::Local => &mut self.local_scroll_x,
            BrowserFocus::Remote => &mut self.remote_scroll_x,
        };
        *sx += 4;
        self.needs_redraw = true;
    }

    pub fn nav_up(&mut self) {
        if let Some((_, sel)) = &mut self.drive_picker {
            sel.select_previous();
            self.needs_redraw = true;
            return;
        }
        match self.focus {
            BrowserFocus::Local => self.local_sel.select_previous(),
            BrowserFocus::Remote => self.remote_sel.select_previous(),
        }
    }

    pub fn nav_down(&mut self) {
        if let Some((_, sel)) = &mut self.drive_picker {
            sel.select_next();
            self.needs_redraw = true;
            return;
        }
        match self.focus {
            BrowserFocus::Local => self.local_sel.select_next(),
            BrowserFocus::Remote => self.remote_sel.select_next(),
        }
    }

    pub fn dismiss_drive_picker(&mut self) {
        if self.drive_picker.take().is_some() {
            self.needs_redraw = true;
        }
    }

    // ---- local-side operations ---------------------------------------------

    /// Handle Enter on the local panel (drive picker or directory navigation).
    pub fn local_enter(&mut self) {
        if self.drive_picker.is_some() {
            if let Some((drives, sel)) = self.drive_picker.take()
                && let Some(i) = sel.selected()
                && let Some(drive) = drives.get(i).cloned()
            {
                self.local_path = drive;
                self.local_entries = read_local_dir(&self.local_path);
                self.local_sel.select_first();
            }
            self.needs_redraw = true;
            return;
        }

        if let Some(i) = self.local_sel.selected() {
            let Some(entry) = self.local_entries.get(i).cloned() else {
                return;
            };
            if entry.name == ".." {
                if let Some(p) = self.local_path.parent() {
                    self.local_path = p.to_path_buf();
                } else {
                    self.show_drive_picker();
                    return;
                }
            } else if entry.is_dir {
                self.local_path.push(&entry.name);
            } else {
                return;
            }
            self.local_entries = read_local_dir(&self.local_path);
            self.local_sel.select_first();
            self.status_msg = format!("Local: {}", self.local_path.to_string_lossy());
            self.status_color = Color::Green;
            self.last_duration = None;
            self.needs_redraw = true;
        }
    }

    /// Handle Backspace on the local panel.
    pub fn local_go_up(&mut self) {
        if self.drive_picker.is_some() {
            self.dismiss_drive_picker();
            return;
        }
        if let Some(p) = self.local_path.parent() {
            self.local_path = p.to_path_buf();
            self.local_entries = read_local_dir(&self.local_path);
            self.local_sel.select_first();
            self.status_msg = format!("Local: {}", self.local_path.to_string_lossy());
            self.status_color = Color::Green;
            self.last_duration = None;
            self.needs_redraw = true;
        } else {
            self.show_drive_picker();
        }
    }

    fn show_drive_picker(&mut self) {
        let drives = list_drives();
        let mut drive_sel = ListState::default();
        drive_sel.select_first();
        self.drive_picker = Some((drives, drive_sel));
        self.needs_redraw = true;
    }

    // ---- remote path -------------------------------------------------------

    pub fn apply_cd(&mut self, name: &str) {
        if name == ".." {
            if let Some(pos) = self.remote_path.rfind('/') {
                self.remote_path = if pos == 0 {
                    "/".to_string()
                } else {
                    self.remote_path[..pos].to_string()
                };
            }
        } else {
            let base = self.remote_path.trim_end_matches('/');
            self.remote_path = format!("{}/{}", base, name);
        }
    }

    // ---- timer -------------------------------------------------------------

    pub fn stop_timer(&mut self) {
        if let Some(start) = self.cmd_start.take() {
            self.last_duration = Some(start.elapsed());
        }
    }

    pub fn format_duration(d: Duration) -> String {
        let ms = d.as_millis();
        if ms < 1000 {
            format!("{}ms", ms)
        } else {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}m", secs / 60)
            } else {
                format!("{}h", secs / 3600)
            }
        }
    }

    // ---- delete (local) ----------------------------------------------------

    pub fn local_delete_focused(&mut self) {
        if let Some(i) = self.local_sel.selected() {
            let Some(entry) = self.local_entries.get(i).cloned() else {
                return;
            };
            if entry.name == ".." {
                return;
            }
            let full_path = self.local_path.join(&entry.name);
            let kind = if entry.is_dir { "dir" } else { "file" };
            self.confirm_delete = Some(format!("local:{}:{}", kind, full_path.to_string_lossy()));
            self.needs_redraw = true;
        }
    }

    /// Execute a confirmed local delete. Returns true if handled.
    pub fn local_confirm_delete(&mut self) -> bool {
        let Some(ref tagged) = self.confirm_delete else {
            return false;
        };
        let Some(rest) = tagged.strip_prefix("local:") else {
            return false;
        };
        let is_dir = rest.starts_with("dir:");
        let full_path = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
        let path = PathBuf::from(full_path);
        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            warn!("local delete failed: {:?}: {}", path, e);
            self.status_msg = format!("Delete failed: {}", e);
            self.status_color = Color::Red;
        } else {
            info!("local delete ok: {}", full_path);
            self.status_msg = format!("Deleted: {}", full_path);
            self.status_color = Color::Green;
            self.local_entries = read_local_dir(&self.local_path);
        }
        self.confirm_delete = None;
        self.last_duration = None;
        self.needs_redraw = true;
        true
    }

    pub fn confirm_delete_no(&mut self) {
        self.confirm_delete = None;
        self.status_msg = String::from("Deletion cancelled.");
        self.status_color = Color::Yellow;
        self.needs_redraw = true;
    }

    // ---- click / drag ------------------------------------------------------

    pub fn click_select(&mut self, col: u16, row: u16, pane_area: Rect, leaf_count: usize) {
        let outer_inner = if leaf_count > 1 {
            pane_inner(pane_area)
        } else {
            pane_area
        };

        let layout = browser_layout(outer_inner);
        let in_remote = col >= layout.remote_panel.x;
        let panel_area = if in_remote {
            layout.remote_panel
        } else {
            layout.local_panel
        };

        // Each panel has its own block border (1-cell inset)
        let list_y = panel_area.y + 1;
        let list_height = panel_area.height.saturating_sub(2);

        if row < list_y || row >= list_y + list_height {
            return;
        }

        let click_row = (row - list_y) as usize;

        if in_remote {
            let offset = self.remote_sel.offset();
            let idx = offset + click_row;
            if idx < self.remote_entries.len() {
                self.remote_sel.select(Some(idx));
                self.needs_redraw = true;
            }
        } else if let Some((drives, drive_sel)) = &mut self.drive_picker {
            let offset = drive_sel.offset();
            let idx = offset + click_row;
            if idx < drives.len() {
                drive_sel.select(Some(idx));
                self.needs_redraw = true;
            }
        } else {
            let offset = self.local_sel.offset();
            let idx = offset + click_row;
            if idx < self.local_entries.len() {
                self.local_sel.select(Some(idx));
                self.needs_redraw = true;
            }
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16, pane_area: Rect, leaf_count: usize) {
        let outer_inner = if leaf_count > 1 {
            pane_inner(pane_area)
        } else {
            pane_area
        };
        let layout = browser_layout(outer_inner);
        self.focus = if col >= layout.remote_panel.x {
            BrowserFocus::Remote
        } else {
            BrowserFocus::Local
        };
        self.click_select(col, row, pane_area, leaf_count);
    }

    pub fn handle_drag_release(
        &mut self,
        col: u16,
        pane_area: Rect,
        leaf_count: usize,
    ) -> Option<DragAction> {
        let outer_inner = if leaf_count > 1 {
            pane_inner(pane_area)
        } else {
            pane_area
        };
        let layout = browser_layout(outer_inner);
        let in_remote = col >= layout.remote_panel.x;
        let drag_from = self.focus;
        self.focus = if in_remote {
            BrowserFocus::Remote
        } else {
            BrowserFocus::Local
        };
        if in_remote && drag_from == BrowserFocus::Local {
            Some(DragAction::LocalToRemote)
        } else if !in_remote && drag_from == BrowserFocus::Remote {
            Some(DragAction::RemoteToLocal)
        } else {
            None
        }
    }

    // ---- rendering ---------------------------------------------------------

    /// Render pane border + both panels. Returns the status bar area.
    pub fn render_panels(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        is_focus: bool,
        leaf_count: usize,
        title: &str,
    ) -> Rect {
        let inner = render_pane_border(area, buf, is_focus, leaf_count, Some(title));
        let layout = browser_layout(inner);
        self.render_panel(layout.local_panel, buf, BrowserFocus::Local, is_focus);
        self.render_panel(layout.remote_panel, buf, BrowserFocus::Remote, is_focus);
        layout.status
    }

    fn render_panel(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        side: BrowserFocus,
        pane_focused: bool,
    ) {
        let is_active = self.focus == side && pane_focused;

        let title = match side {
            BrowserFocus::Local if self.drive_picker.is_some() => " select drive ",
            BrowserFocus::Local => " local ",
            BrowserFocus::Remote => " remote ",
        };
        let path_str = match side {
            BrowserFocus::Local => self.local_path.to_string_lossy().to_string(),
            BrowserFocus::Remote => self.remote_path.clone(),
        };

        let border_col = if is_active {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_col))
            .title_top(Line::from(Span::styled(
                format!(" {} ", path_str),
                Style::default().fg(Color::DarkGray),
            )))
            .title_top(
                Line::from(Span::styled(
                    format!(" {} ", title),
                    Style::default().fg(Color::Yellow),
                ))
                .right_aligned(),
            );
        let inner = block.inner(area);
        block.render(area, buf);

        // Drive picker: shown in local panel instead of the normal file list.
        if side == BrowserFocus::Local
            && let Some((drives, drive_sel)) = &mut self.drive_picker
        {
            let items: Vec<String> = drives
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let list = List::new(items).highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            StatefulWidget::render(list, inner, buf, drive_sel);
            return;
        }

        let (entries, list_state) = match side {
            BrowserFocus::Local => (&self.local_entries, &mut self.local_sel),
            BrowserFocus::Remote => (&self.remote_entries, &mut self.remote_sel),
        };

        let only_dotdot =
            entries.len() <= 1 && entries.first().map(|e| e.name == "..").unwrap_or(true);
        if side == BrowserFocus::Remote && only_dotdot && !self.raw_snapshot.is_empty() {
            let visible: Vec<&String> = self
                .raw_snapshot
                .iter()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let start = visible.len().saturating_sub(inner.height as usize);
            for (i, line) in visible[start..].iter().enumerate() {
                let y = inner.y + i as u16;
                if y >= inner.y + inner.height {
                    break;
                }
                let span = Span::styled(
                    line.chars().take(inner.width as usize).collect::<String>(),
                    Style::default().fg(Color::DarkGray),
                );
                buf.set_span(inner.x, y, &span, inner.width);
            }
            return;
        }

        let w = inner.width as usize;
        let meta_width: usize = 9 + 1 + 16 + 1 + 10;

        let max_name_len = entries
            .iter()
            .map(|e| {
                if e.is_dir {
                    e.name.len() + 1
                } else {
                    e.name.len()
                }
            })
            .max()
            .unwrap_or(0);

        let virtual_width = (max_name_len + 1 + meta_width).max(w);

        let scroll_x = match side {
            BrowserFocus::Local => &mut self.local_scroll_x,
            BrowserFocus::Remote => &mut self.remote_scroll_x,
        };
        let max_scroll = virtual_width.saturating_sub(w);
        if *scroll_x > max_scroll {
            *scroll_x = max_scroll;
        }
        let sx = *scroll_x;

        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let name_col = if e.is_dir { Color::Cyan } else { Color::White };
                let display_name = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };

                let meta = format!("{:>9} {:<16} {:<10}", e.size, e.modified, e.perms);
                let name_len = display_name.chars().count();
                let gap = virtual_width - meta_width - name_len;
                let full = format!("{}{:gap$}{}", display_name, "", meta, gap = gap);

                let scrolled: String = full.chars().skip(sx).take(w).collect();
                let padded = format!("{:<width$}", scrolled, width = w);

                let visible_name_chars = if sx < name_len {
                    (name_len - sx).min(w)
                } else {
                    0
                };

                if visible_name_chars == 0 {
                    let line =
                        Line::from(Span::styled(padded, Style::default().fg(Color::DarkGray)));
                    ListItem::new(line)
                } else {
                    let name_part: String = padded.chars().take(visible_name_chars).collect();
                    let rest: String = padded.chars().skip(visible_name_chars).collect();
                    let line = Line::from(vec![
                        Span::styled(name_part, Style::default().fg(name_col)),
                        Span::styled(rest, Style::default().fg(Color::DarkGray)),
                    ]);
                    ListItem::new(line)
                }
            })
            .collect();

        let list = List::new(items).highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(if is_active {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        );
        StatefulWidget::render(list, inner, buf, list_state);
    }

    /// Render delete confirmation bar. Returns true if rendered.
    pub fn render_confirm_delete(&self, area: Rect, buf: &mut Buffer) -> bool {
        let Some(ref tagged) = self.confirm_delete else {
            return false;
        };
        let (side, rest) = if let Some(r) = tagged.strip_prefix("local:") {
            ("local", r)
        } else if let Some(r) = tagged.strip_prefix("remote:") {
            ("remote", r)
        } else {
            ("", tagged.as_str())
        };
        let name = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
        let msg = format!("  Delete {} '{}'?  [y] Yes   [n] No", side, name);
        let span = Span::styled(
            msg,
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_span(area.x, area.y, &span, area.width);
        true
    }

    /// Render the normal status bar (state badge + message + shortcuts).
    pub fn render_normal_status(
        &self,
        area: Rect,
        buf: &mut Buffer,
        state_label: &str,
        state_color: Color,
        progress_suffix: &str,
    ) {
        let help = " [T]xfer [Del]rm ";
        let help_len = help.chars().count() as u16;
        let help_x = area.x + area.width.saturating_sub(help_len);

        let duration_suffix = if let Some(d) = self.last_duration {
            format!(" ({})", Self::format_duration(d))
        } else {
            String::new()
        };

        let left_line = Line::from(vec![
            Span::styled(
                format!("[{}]", state_label),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}{}", self.status_msg, progress_suffix),
                Style::default().fg(state_color),
            ),
            Span::styled(duration_suffix, Style::default().fg(Color::DarkGray)),
        ]);
        buf.set_line(area.x, area.y, &left_line, help_x.saturating_sub(area.x));

        buf.set_span(
            help_x,
            area.y,
            &Span::styled(help, Style::default().fg(Color::DarkGray)),
            area.width.saturating_sub(help_x.saturating_sub(area.x)),
        );
    }
}

// ---------------------------------------------------------------------------
// Key handling helper
// ---------------------------------------------------------------------------

/// Handle a key event for a browser in idle mode (not connecting, not waiting
/// for password). Navigation keys are handled directly on `core`; actions that
/// need browser-specific logic are returned as a `BrowserKeyAction`.
pub fn handle_browser_key(core: &mut BrowserCore, code: KeyCode) -> BrowserKeyAction {
    if core.confirm_delete.is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return BrowserKeyAction::ConfirmDeleteYes,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                core.confirm_delete_no();
                return BrowserKeyAction::Handled;
            }
            _ => return BrowserKeyAction::Handled,
        }
    }
    match code {
        KeyCode::Tab => {
            core.toggle_focus();
            BrowserKeyAction::Handled
        }
        KeyCode::Esc => {
            core.dismiss_drive_picker();
            BrowserKeyAction::Handled
        }
        KeyCode::Up => {
            core.nav_up();
            BrowserKeyAction::Handled
        }
        KeyCode::Down => {
            core.nav_down();
            BrowserKeyAction::Handled
        }
        KeyCode::Left => {
            core.scroll_left();
            BrowserKeyAction::Handled
        }
        KeyCode::Right => {
            core.scroll_right();
            BrowserKeyAction::Handled
        }
        KeyCode::Char(' ') | KeyCode::Enter => BrowserKeyAction::Enter,
        KeyCode::Backspace => BrowserKeyAction::GoUp,
        KeyCode::Char('t') => match core.focus {
            BrowserFocus::Remote => BrowserKeyAction::Download,
            BrowserFocus::Local => BrowserKeyAction::Upload,
        },
        KeyCode::Delete => BrowserKeyAction::Delete,
        _ => BrowserKeyAction::Handled,
    }
}
