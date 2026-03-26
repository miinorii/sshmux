use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use log::{debug, info, warn};
use ratatui::{
    buffer::Buffer,
    layout::Alignment,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget},
};

use super::browser_layout;
use super::parse::{FsEntry, list_drives, read_local_dir};
use crate::pane::{pane_inner, render_pane_border};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    UploadPaths,
    Delete,
    ConfirmDeleteYes,
}

/// Result of a drag-release gesture across panels.
pub enum DragAction {
    LocalToRemote,
    RemoteToLocal,
}

/// State tracked during a left-button drag gesture in a browser pane.
pub struct DragState {
    pub origin: BrowserFocus,
    pub label: String,
    pub mouse_col: u16,
    pub mouse_row: u16,
}

/// Shared interface for browser panes (SFTP and SCP).
pub trait Browser {
    fn core_mut(&mut self) -> &mut BrowserCore;
    fn upload(&mut self);
    fn download(&mut self);
    fn enter(&mut self);
    fn go_up(&mut self);
    fn upload_pending_paths(&mut self);
    fn delete_focused(&mut self);
    fn confirm_delete_yes(&mut self);
    /// True when the browser is in a connecting/auth state that should forward raw keys.
    fn is_connecting(&self) -> bool;
    /// Forward a key during the connecting phase.
    fn send_connect_key(&mut self, code: KeyCode);
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
    // ---- multi-select ----
    pub selected: BTreeSet<usize>,
    pub select_anchor: Option<usize>,
    // ---- multi-transfer queues ----
    pub pending_transfers: Vec<String>, // filenames queued for batch transfer
    pub pending_deletes: Vec<String>,   // tagged delete strings queued for batch delete
    // ---- drag-and-drop paste detection ----
    pub paste_buf: String,
    pub paste_deadline: Option<Instant>,
    pub pending_uploads: Vec<PathBuf>,
    pub upload_scroll_x: usize,
    pub upload_scroll_y: usize,
    pub last_inner: Rect,
    // ---- drag visual feedback ----
    pub drag: Option<DragState>,
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
            selected: BTreeSet::new(),
            select_anchor: None,
            pending_transfers: vec![],
            pending_deletes: vec![],
            paste_buf: String::new(),
            paste_deadline: None,
            pending_uploads: vec![],
            upload_scroll_x: 0,
            upload_scroll_y: 0,
            last_inner: Rect::default(),
            drag: None,
        }
    }

    // ---- navigation --------------------------------------------------------

    pub fn clear_selection(&mut self) {
        if !self.selected.is_empty() || self.select_anchor.is_some() {
            self.selected.clear();
            self.select_anchor = None;
            self.needs_redraw = true;
        }
    }

    /// Returns the currently focused index for the active panel.
    pub fn focused_index(&self) -> Option<usize> {
        match self.focus {
            BrowserFocus::Local => self.local_sel.selected(),
            BrowserFocus::Remote => self.remote_sel.selected(),
        }
    }

    /// Returns the indices to operate on: the multi-select set if non-empty,
    /// otherwise the single focused index (excluding `..` at index 0).
    pub fn selected_indices(&self) -> Vec<usize> {
        if !self.selected.is_empty() {
            self.selected.iter().copied().collect()
        } else if let Some(i) = self.focused_index() {
            if i == 0 { vec![] } else { vec![i] }
        } else {
            vec![]
        }
    }

    /// Returns the direction of the last completed transfer, defaulting to Upload.
    pub fn last_transfer_direction(&self) -> TransferDirection {
        self.last_transfer
            .as_ref()
            .map(|t| t.direction)
            .unwrap_or(TransferDirection::Upload)
    }

    /// Build a drag label from the current selection. Returns None if nothing to drag.
    pub fn drag_label(&self) -> Option<String> {
        let indices = self.selected_indices();
        if indices.is_empty() {
            return None;
        }
        let entries = match self.focus {
            BrowserFocus::Local => &self.local_entries,
            BrowserFocus::Remote => &self.remote_entries,
        };
        if indices.len() > 1 {
            Some(format!("{} files", indices.len()))
        } else {
            entries.get(indices[0]).map(|e| e.name.clone())
        }
    }

    /// Update selection range between anchor and current cursor position.
    /// Skips index 0 (`..`).
    pub fn update_selection(&mut self) {
        let Some(anchor) = self.select_anchor else {
            return;
        };
        let Some(cursor) = self.focused_index() else {
            return;
        };
        let lo = anchor.min(cursor).max(1); // skip ".."
        let hi = anchor.max(cursor);
        self.selected.clear();
        for i in lo..=hi {
            self.selected.insert(i);
        }
        self.needs_redraw = true;
    }

    pub fn toggle_focus(&mut self) {
        self.dismiss_drive_picker();
        self.clear_selection();
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

    // ---- paste / drag-and-drop detection ------------------------------------

    /// Called each tick. If the paste deadline has expired, parse the buffer
    /// for valid file paths and populate `pending_uploads`.
    pub fn check_paste_deadline(&mut self) {
        let expired = self
            .paste_deadline
            .map(|d| Instant::now() >= d)
            .unwrap_or(false);
        if !expired {
            return;
        }
        let text = std::mem::take(&mut self.paste_buf);
        self.paste_deadline = None;
        debug!(
            "paste deadline expired: {} chars, text={:?}",
            text.len(),
            &text[..text.len().min(200)]
        );

        let parse_start = Instant::now();
        let paths = parse_dropped_paths(&text);
        let parse_elapsed = parse_start.elapsed();
        debug!(
            "parse_dropped_paths took {:?}, found {} path(s)",
            parse_elapsed,
            paths.len()
        );

        if paths.is_empty() {
            self.status_msg.clear();
            self.needs_redraw = true;
            return;
        }
        info!(
            "drag-drop detected: {} file(s): {:?}",
            paths.len(),
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );
        self.pending_uploads = paths;
        self.upload_scroll_x = 0;
        self.upload_scroll_y = 0;
        self.status_msg = format!("{} file(s) ready to upload", self.pending_uploads.len());
        self.status_color = Color::Cyan;
        self.needs_redraw = true;
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
        loop {
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
                self.pending_deletes.clear();
            } else {
                info!("local delete ok: {}", full_path);
                self.status_msg = format!("Deleted: {}", full_path);
                self.status_color = Color::Green;
                self.local_entries = read_local_dir(&self.local_path);
            }
            self.confirm_delete = None;
            self.last_duration = None;
            if !self.pop_pending_delete() {
                break;
            }
        }
        self.needs_redraw = true;
        true
    }

    /// Build tagged delete strings for local multi-select deletion.
    /// Queues all selected items and shows confirmation for the first one.
    pub fn local_delete_selected(&mut self) {
        let indices = self.selected_indices();
        if indices.len() <= 1 {
            self.clear_selection();
            self.local_delete_focused();
            return;
        }
        let mut tags: Vec<String> = Vec::new();
        for &i in &indices {
            let Some(entry) = self.local_entries.get(i) else {
                continue;
            };
            if entry.name == ".." {
                continue;
            }
            let full_path = self.local_path.join(&entry.name);
            let kind = if entry.is_dir { "dir" } else { "file" };
            tags.push(format!("local:{}:{}", kind, full_path.to_string_lossy()));
        }
        self.clear_selection();
        if let Some(first) = tags.first().cloned() {
            self.pending_deletes = tags[1..].to_vec();
            self.confirm_delete = Some(first);
            self.needs_redraw = true;
        }
    }

    /// Pop the next pending delete and set it as confirm_delete.
    /// Returns true if there was a next item to delete.
    pub fn pop_pending_delete(&mut self) -> bool {
        if let Some(next) = self.pending_deletes.first().cloned() {
            self.pending_deletes.remove(0);
            self.confirm_delete = Some(next);
            true
        } else {
            false
        }
    }

    /// Convert selected indices to filenames and store in `pending_transfers`.
    pub fn queue_transfers_from_indices(&mut self, indices: &[usize]) {
        let entries = match self.focus {
            BrowserFocus::Local => &self.local_entries,
            BrowserFocus::Remote => &self.remote_entries,
        };
        self.pending_transfers = indices
            .iter()
            .filter_map(|&i| entries.get(i))
            .filter(|e| e.name != "..")
            .map(|e| e.name.clone())
            .collect();
    }

    /// Pop the next pending transfer filename and find its index in the current
    /// entry list. Returns `Some(index)` if found, `None` if name no longer exists.
    pub fn pop_pending_transfer(&mut self) -> Option<usize> {
        while let Some(name) = self.pending_transfers.first().cloned() {
            self.pending_transfers.remove(0);
            let entries = match self.focus {
                BrowserFocus::Local => &self.local_entries,
                BrowserFocus::Remote => &self.remote_entries,
            };
            if let Some(idx) = entries.iter().position(|e| e.name == name) {
                return Some(idx);
            }
            warn!("pending transfer '{}' no longer in listing, skipping", name);
        }
        None
    }

    pub fn confirm_delete_no(&mut self) {
        self.confirm_delete = None;
        self.pending_deletes.clear();
        self.status_msg = String::from("Deletion cancelled.");
        self.status_color = Color::Yellow;
        self.needs_redraw = true;
    }

    /// Build tagged delete strings for remote multi-select or single-item deletion.
    /// Sets `confirm_delete` for the first item and queues the rest in `pending_deletes`.
    pub fn remote_delete_focused(&mut self) {
        let indices = self.selected_indices();
        if indices.len() > 1 {
            let mut tags: Vec<String> = Vec::new();
            for &i in &indices {
                let Some(entry) = self.remote_entries.get(i) else {
                    continue;
                };
                if entry.name == ".." {
                    continue;
                }
                let full_path =
                    format!("{}/{}", self.remote_path.trim_end_matches('/'), entry.name);
                let kind = if entry.is_dir { "dir" } else { "file" };
                tags.push(format!("remote:{}:{}", kind, full_path));
            }
            self.clear_selection();
            if let Some(first) = tags.first().cloned() {
                self.pending_deletes = tags[1..].to_vec();
                self.confirm_delete = Some(first);
                self.needs_redraw = true;
            }
        } else {
            self.clear_selection();
            if let Some(i) = self.remote_sel.selected() {
                let Some(entry) = self.remote_entries.get(i).cloned() else {
                    return;
                };
                if entry.name == ".." {
                    return;
                }
                let full_path =
                    format!("{}/{}", self.remote_path.trim_end_matches('/'), entry.name);
                let kind = if entry.is_dir { "dir" } else { "file" };
                self.confirm_delete = Some(format!("remote:{}:{}", kind, full_path));
                self.needs_redraw = true;
            }
        }
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

    /// Handle all mouse events for browser panes. Returns `Some(DragAction)`
    /// on mouse-up when the drag crossed panels (caller should trigger transfer).
    pub fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        col: u16,
        row: u16,
        pane_area: Rect,
        leaf_count: usize,
    ) -> Option<DragAction> {
        if !self.pending_uploads.is_empty() {
            return None;
        }
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_click(col, row, pane_area, leaf_count);
                if let Some(label) = self.drag_label() {
                    self.drag = Some(DragState {
                        origin: self.focus,
                        label,
                        mouse_col: col,
                        mouse_row: row,
                    });
                }
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(ref mut d) = self.drag {
                    d.mouse_col = col;
                    d.mouse_row = row;
                    self.needs_redraw = true;
                }
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                let indices = self.selected_indices();
                if indices.len() > 1 {
                    self.queue_transfers_from_indices(&indices);
                    self.clear_selection();
                }
                self.handle_drag_release(col, pane_area, leaf_count)
            }
            _ => None,
        }
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

        let is_sel_panel = self.focus == side;
        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(idx, e)| {
                let selected = is_sel_panel && self.selected.contains(&idx);
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

                let sel_style = Style::default()
                    .fg(Color::Black)
                    .bg(if is_active {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })
                    .add_modifier(Modifier::BOLD);
                if visible_name_chars == 0 {
                    let style = if selected {
                        sel_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    ListItem::new(Line::from(Span::styled(padded, style)))
                } else {
                    let name_part: String = padded.chars().take(visible_name_chars).collect();
                    let rest: String = padded.chars().skip(visible_name_chars).collect();
                    if selected {
                        ListItem::new(Line::from(vec![
                            Span::styled(name_part, sel_style),
                            Span::styled(rest, sel_style),
                        ]))
                    } else {
                        ListItem::new(Line::from(vec![
                            Span::styled(name_part, Style::default().fg(name_col)),
                            Span::styled(rest, Style::default().fg(Color::DarkGray)),
                        ]))
                    }
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
        let remaining = self.pending_deletes.len();
        let msg = if remaining > 0 {
            format!(
                "  Delete {} '{}' (+{} more)?  [y] Yes   [n] No",
                side, name, remaining
            )
        } else {
            format!("  Delete {} '{}'?  [y] Yes   [n] No", side, name)
        };
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

    /// Render the upload confirmation overlay when files are pending.
    /// Returns true if the overlay was rendered (callers can skip status bar).
    pub fn render_upload_confirm(&mut self, inner: Rect, buf: &mut Buffer) -> bool {
        if self.pending_uploads.is_empty() {
            return false;
        }

        let count = self.pending_uploads.len();
        let scroll_x = self.upload_scroll_x;
        let scroll_y = self.upload_scroll_y;

        self.last_inner = inner;
        let box_w = 60u16.min(inner.width.saturating_sub(4));
        // Fixed lines: title + blank + indicator/blank + hints = 4, borders = 2
        let max_file_rows = 5.min((inner.height as usize).saturating_sub(6));
        let visible_files: Vec<_> = self
            .pending_uploads
            .iter()
            .skip(scroll_y)
            .take(max_file_rows.max(1))
            .collect();

        // Build file list lines
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!(" Upload {} file(s)? ", count),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for path in &visible_files {
            let full = format!("  {}", path.display());
            let display = if scroll_x > 0 && scroll_x < full.len() {
                format!("…{}", &full[scroll_x + 1..])
            } else {
                full
            };
            lines.push(Line::from(Span::styled(
                display,
                Style::default().fg(Color::Cyan),
            )));
        }
        // Show scroll indicator if not all files are visible
        if count > visible_files.len() {
            let indicator = format!(
                "  [{}-{} of {}] ↑↓ to scroll",
                scroll_y + 1,
                scroll_y + visible_files.len(),
                count
            );
            lines.push(Line::from(Span::styled(
                indicator,
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(""));
        }

        let hint_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled("[y]", hint_style),
            Span::styled(" Upload  ", dim),
            Span::styled("[n]", hint_style),
            Span::styled(" Cancel  ", dim),
            Span::styled("[←→]", hint_style),
            Span::styled(" Scroll", dim),
        ]));

        let box_h = (lines.len() as u16 + 2).min(inner.height);
        let cx = inner.x + inner.width.saturating_sub(box_w) / 2;
        let cy = inner.y + inner.height.saturating_sub(box_h) / 2;
        let overlay = Rect {
            x: cx,
            y: cy,
            width: box_w,
            height: box_h,
        };

        // Clear the area
        for y in overlay.y..overlay.y + overlay.height {
            for x in overlay.x..overlay.x + overlay.width {
                if x < buf.area().width && y < buf.area().height {
                    buf[(x, y)].reset();
                }
            }
        }

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(Color::Black))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                    .title(" Drop Upload ")
                    .title_alignment(Alignment::Center),
            );
        paragraph.render(overlay, buf);
        true
    }

    /// Render directional arrows on the panel border during a cross-panel drag.
    pub fn render_drag_arrow(&self, area: Rect, buf: &mut Buffer, leaf_count: usize) {
        let drag = match self.drag {
            Some(ref d) => d,
            None => return,
        };
        let inner = if leaf_count > 1 {
            pane_inner(area)
        } else {
            area
        };
        let layout = browser_layout(inner);
        // Arrows always show: direction is based on drag origin panel.
        let chars: [char; 2] = match drag.origin {
            BrowserFocus::Local => ['>', '>'],
            BrowserFocus::Remote => ['<', '<'],
        };
        let x0 = layout.local_panel.x + layout.local_panel.width - 1;
        let x1 = layout.remote_panel.x;
        let style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let h = layout.local_panel.height;
        let count = 3.min(h as usize);
        let mid = layout.local_panel.y + h / 2;
        let start = mid.saturating_sub(count as u16 / 2);
        for y in start..start + count as u16 {
            for (cx, ch) in [(x0, chars[0]), (x1, chars[1])] {
                buf[(cx, y)].set_char(ch).set_style(style);
            }
        }
    }

    /// Render a ghost label near the cursor during a drag gesture.
    pub fn render_drag_ghost(&self, buf: &mut Buffer) {
        let drag = match self.drag {
            Some(ref d) => d,
            None => return,
        };
        let label = format!(" {} ", drag.label);
        let x = drag.mouse_col + 2;
        let y = drag.mouse_row;
        let Rect {
            x: bx,
            y: by,
            width: bw,
            height: bh,
        } = *buf.area();
        if y < by || y >= by + bh {
            return;
        }
        for (i, ch) in label.chars().enumerate() {
            let cx = x + i as u16;
            if cx >= bx && cx < bx + bw {
                let cell = &mut buf[(cx, y)];
                cell.set_char(ch);
                cell.fg = Color::Yellow;
                cell.modifier.insert(Modifier::BOLD);
            }
        }
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
pub fn handle_browser_key(core: &mut BrowserCore, code: KeyCode, shift: bool) -> BrowserKeyAction {
    // ---- Upload confirmation overlay ----
    if !core.pending_uploads.is_empty() {
        match code {
            KeyCode::Up => {
                core.upload_scroll_y = core.upload_scroll_y.saturating_sub(1);
                core.needs_redraw = true;
            }
            KeyCode::Down => {
                let max_rows = 5.min((core.last_inner.height as usize).saturating_sub(6));
                let max_y = core.pending_uploads.len().saturating_sub(max_rows);
                if core.upload_scroll_y < max_y {
                    core.upload_scroll_y += 1;
                    core.needs_redraw = true;
                }
            }
            KeyCode::Left => {
                core.upload_scroll_x = core.upload_scroll_x.saturating_sub(1);
                core.needs_redraw = true;
            }
            KeyCode::Right => {
                let box_w = 60u16.min(core.last_inner.width.saturating_sub(4));
                let content_w = (box_w as usize).saturating_sub(2);
                let longest = core
                    .pending_uploads
                    .iter()
                    .map(|p| format!("  {}", p.display()).len())
                    .max()
                    .unwrap_or(0);
                let max_scroll = longest.saturating_sub(content_w);
                if core.upload_scroll_x < max_scroll {
                    core.upload_scroll_x += 1;
                    core.needs_redraw = true;
                }
            }
            KeyCode::Char('y') => {
                return BrowserKeyAction::UploadPaths;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                core.pending_uploads.clear();
                core.upload_scroll_x = 0;
                core.upload_scroll_y = 0;
                core.status_msg = "File drop canceled".to_string();
                core.status_color = Color::Yellow;
                core.needs_redraw = true;
            }
            _ => {}
        }
        return BrowserKeyAction::Handled;
    }

    // ---- Paste accumulation: capture all chars while buffer is active ----
    if !core.paste_buf.is_empty()
        && let KeyCode::Char(c) = code
    {
        core.paste_buf.push(c);
        core.paste_deadline = Some(Instant::now() + Duration::from_millis(150));
        debug!("paste accumulating: {} chars total", core.paste_buf.len());
        return BrowserKeyAction::Handled;
    }

    // ---- Delete confirmation ----
    if core.confirm_delete.is_some() {
        match code {
            KeyCode::Char('y') => return BrowserKeyAction::ConfirmDeleteYes,
            KeyCode::Char('n') | KeyCode::Esc => {
                core.confirm_delete_no();
                return BrowserKeyAction::Handled;
            }
            _ => return BrowserKeyAction::Handled,
        }
    }

    // ---- Normal mode ----
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
            if shift {
                if core.select_anchor.is_none() {
                    core.select_anchor = core.focused_index();
                }
                core.nav_up();
                core.update_selection();
            } else {
                core.clear_selection();
                core.nav_up();
            }
            BrowserKeyAction::Handled
        }
        KeyCode::Down => {
            if shift {
                if core.select_anchor.is_none() {
                    core.select_anchor = core.focused_index();
                }
                core.nav_down();
                core.update_selection();
            } else {
                core.clear_selection();
                core.nav_down();
            }
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
        KeyCode::Char(' ') | KeyCode::Enter => {
            core.clear_selection();
            BrowserKeyAction::Enter
        }
        KeyCode::Backspace => {
            core.clear_selection();
            BrowserKeyAction::GoUp
        }
        KeyCode::Char('t') => {
            let indices = core.selected_indices();
            if indices.len() > 1 {
                core.queue_transfers_from_indices(&indices);
                core.clear_selection();
            }
            match core.focus {
                BrowserFocus::Remote => BrowserKeyAction::Download,
                BrowserFocus::Local => BrowserKeyAction::Upload,
            }
        }
        KeyCode::Delete => BrowserKeyAction::Delete,
        // Unrecognized char: start paste accumulation (no redraw to avoid
        // hundreds of draws while characters stream in from a file drop)
        KeyCode::Char(c) => {
            debug!("paste accumulation started with char {:?}", c);
            core.paste_buf.push(c);
            core.paste_deadline = Some(Instant::now() + Duration::from_millis(150));
            BrowserKeyAction::Handled
        }
        _ => BrowserKeyAction::Handled,
    }
}

// ---------------------------------------------------------------------------
// Path parsing for drag-and-drop detection
// ---------------------------------------------------------------------------

/// Parse file paths from text pasted by the OS (drag-and-drop).
/// Handles quoted paths (for names with spaces) and multiple paths.
/// Only returns paths that actually exist on disk.
fn parse_dropped_paths(text: &str) -> Vec<PathBuf> {
    let text = text.trim();
    let mut paths = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let token = if rest.starts_with('"') {
            let inner = &rest[1..];
            if let Some(end) = inner.find('"') {
                let tok = &inner[..end];
                rest = &inner[end + 1..];
                tok
            } else {
                rest = "";
                inner
            }
        } else if let Some(split_pos) = find_path_boundary(rest) {
            let tok = &rest[..split_pos];
            rest = &rest[split_pos..];
            tok.trim_end()
        } else {
            let tok = rest;
            rest = "";
            tok
        };

        let path = std::path::Path::new(token);
        if path.exists() {
            paths.push(path.to_path_buf());
        }
    }

    paths
}

/// Find where one unquoted path ends and the next begins.
/// Looks for ` X:\` or ` /` boundaries that signal a new absolute path.
fn find_path_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b' ' && i + 1 < bytes.len() {
            let after = &s[i + 1..];
            if (after.len() >= 3 && after.as_bytes()[1] == b':' && after.as_bytes()[2] == b'\\')
                || after.starts_with('/')
            {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_entry(name: &str, is_dir: bool) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            is_dir,
            size: "0".to_string(),
            perms: "drwxr-xr-x".to_string(),
            modified: "2025-01-01".to_string(),
        }
    }

    fn core_with_remote_entries(names: &[(&str, bool)]) -> BrowserCore {
        let mut core = BrowserCore::new("test-host");
        core.remote_entries = names.iter().map(|(n, d)| dummy_entry(n, *d)).collect();
        core.remote_sel.select(Some(0));
        core
    }

    #[test]
    fn nav_down_advances_remote_selection() {
        let mut core =
            core_with_remote_entries(&[("..", true), ("dir1", true), ("file.txt", false)]);
        core.focus = BrowserFocus::Remote;
        core.remote_sel.select(Some(0));
        core.nav_down();
        assert_eq!(core.remote_sel.selected(), Some(1));
    }

    #[test]
    fn nav_up_retreats_remote_selection() {
        let mut core =
            core_with_remote_entries(&[("..", true), ("dir1", true), ("file.txt", false)]);
        core.focus = BrowserFocus::Remote;
        core.remote_sel.select(Some(2));
        core.nav_up();
        assert_eq!(core.remote_sel.selected(), Some(1));
    }

    // ---- toggle_focus -------------------------------------------------------

    #[test]
    fn toggle_focus_switches_local_to_remote() {
        let mut core = BrowserCore::new("host");
        assert_eq!(core.focus, BrowserFocus::Local);
        core.toggle_focus();
        assert_eq!(core.focus, BrowserFocus::Remote);
    }

    #[test]
    fn toggle_focus_switches_remote_to_local() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        core.toggle_focus();
        assert_eq!(core.focus, BrowserFocus::Local);
    }

    #[test]
    fn toggle_focus_dismisses_drive_picker() {
        let mut core = BrowserCore::new("host");
        core.drive_picker = Some((vec![], ListState::default()));
        core.toggle_focus();
        assert!(core.drive_picker.is_none());
    }

    // ---- scroll_left / scroll_right -----------------------------------------

    #[test]
    fn scroll_right_increments_by_four() {
        let mut core = BrowserCore::new("host");
        core.scroll_right();
        assert_eq!(core.local_scroll_x, 4);
        assert!(core.needs_redraw);
    }

    #[test]
    fn scroll_left_decrements_by_four() {
        let mut core = BrowserCore::new("host");
        core.local_scroll_x = 8;
        core.scroll_left();
        assert_eq!(core.local_scroll_x, 4);
    }

    #[test]
    fn scroll_left_saturates_at_zero() {
        let mut core = BrowserCore::new("host");
        core.local_scroll_x = 2;
        core.scroll_left();
        assert_eq!(core.local_scroll_x, 0);
    }

    #[test]
    fn scroll_left_noop_when_zero() {
        let mut core = BrowserCore::new("host");
        core.needs_redraw = false;
        core.scroll_left();
        assert!(!core.needs_redraw);
    }

    #[test]
    fn scroll_affects_remote_when_focused() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        core.scroll_right();
        assert_eq!(core.remote_scroll_x, 4);
        assert_eq!(core.local_scroll_x, 0);
    }

    // ---- apply_cd -----------------------------------------------------------

    #[test]
    fn apply_cd_subdir() {
        let mut core = BrowserCore::new("host");
        core.remote_path = "/home/user".to_string();
        core.apply_cd("docs");
        assert_eq!(core.remote_path, "/home/user/docs");
    }

    #[test]
    fn apply_cd_parent() {
        let mut core = BrowserCore::new("host");
        core.remote_path = "/home/user/docs".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote_path, "/home/user");
    }

    #[test]
    fn apply_cd_parent_at_root() {
        let mut core = BrowserCore::new("host");
        core.remote_path = "/".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote_path, "/");
    }

    #[test]
    fn apply_cd_parent_from_top_level_dir() {
        let mut core = BrowserCore::new("host");
        core.remote_path = "/home".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote_path, "/");
    }

    #[test]
    fn apply_cd_no_double_slash() {
        let mut core = BrowserCore::new("host");
        core.remote_path = "/home/user/".to_string();
        core.apply_cd("docs");
        assert_eq!(core.remote_path, "/home/user/docs");
    }

    // ---- format_duration ----------------------------------------------------

    #[test]
    fn format_duration_millis() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(42)),
            "42ms"
        );
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(BrowserCore::format_duration(Duration::from_secs(5)), "5s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(BrowserCore::format_duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_secs(7200)),
            "2h"
        );
    }

    #[test]
    fn format_duration_boundary_999ms() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(999)),
            "999ms"
        );
    }

    #[test]
    fn format_duration_boundary_1000ms() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(1000)),
            "1s"
        );
    }

    // ---- confirm_delete_no --------------------------------------------------

    #[test]
    fn confirm_delete_no_clears_state() {
        let mut core = BrowserCore::new("host");
        core.confirm_delete = Some("remote:file:/tmp/test.txt".to_string());
        core.confirm_delete_no();
        assert!(core.confirm_delete.is_none());
        assert_eq!(core.status_msg, "Deletion cancelled.");
        assert_eq!(core.status_color, Color::Yellow);
        assert!(core.needs_redraw);
    }

    // ---- local_delete_focused -----------------------------------------------

    #[test]
    fn local_delete_focused_sets_confirm() {
        let mut core = BrowserCore::new("host");
        // local_entries already populated from cwd; add a known entry
        core.local_entries = vec![dummy_entry("..", true), dummy_entry("myfile.txt", false)];
        core.local_sel.select(Some(1));
        core.local_delete_focused();
        assert!(core.confirm_delete.is_some());
        let tag = core.confirm_delete.unwrap();
        assert!(tag.starts_with("local:file:"));
        assert!(tag.contains("myfile.txt"));
    }

    #[test]
    fn local_delete_focused_skips_dotdot() {
        let mut core = BrowserCore::new("host");
        core.local_entries = vec![dummy_entry("..", true)];
        core.local_sel.select(Some(0));
        core.local_delete_focused();
        assert!(core.confirm_delete.is_none());
    }

    // ---- stop_timer ---------------------------------------------------------

    #[test]
    fn stop_timer_records_duration() {
        let mut core = BrowserCore::new("host");
        core.cmd_start = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(5));
        core.stop_timer();
        assert!(core.cmd_start.is_none());
        assert!(core.last_duration.is_some());
    }

    #[test]
    fn stop_timer_noop_without_start() {
        let mut core = BrowserCore::new("host");
        core.stop_timer();
        assert!(core.last_duration.is_none());
    }

    // ---- dismiss_drive_picker -----------------------------------------------

    #[test]
    fn dismiss_drive_picker_when_active() {
        let mut core = BrowserCore::new("host");
        core.drive_picker = Some((vec![], ListState::default()));
        core.needs_redraw = false;
        core.dismiss_drive_picker();
        assert!(core.drive_picker.is_none());
        assert!(core.needs_redraw);
    }

    #[test]
    fn dismiss_drive_picker_noop_when_none() {
        let mut core = BrowserCore::new("host");
        core.needs_redraw = false;
        core.dismiss_drive_picker();
        assert!(!core.needs_redraw);
    }

    // ---- handle_browser_key -------------------------------------------------

    #[test]
    fn key_tab_toggles_focus() {
        let mut core = BrowserCore::new("host");
        assert_eq!(core.focus, BrowserFocus::Local);
        let action = handle_browser_key(&mut core, KeyCode::Tab, false);
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert_eq!(core.focus, BrowserFocus::Remote);
    }

    #[test]
    fn key_enter_returns_enter_action() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Enter, false),
            BrowserKeyAction::Enter
        ));
    }

    #[test]
    fn key_space_returns_enter_action() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Char(' '), false),
            BrowserKeyAction::Enter
        ));
    }

    #[test]
    fn key_backspace_returns_go_up() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Backspace, false),
            BrowserKeyAction::GoUp
        ));
    }

    #[test]
    fn key_t_remote_returns_download() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Char('t'), false),
            BrowserKeyAction::Download
        ));
    }

    #[test]
    fn key_t_local_returns_upload() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Local;
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Char('t'), false),
            BrowserKeyAction::Upload
        ));
    }

    #[test]
    fn key_delete_returns_delete() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Delete, false),
            BrowserKeyAction::Delete
        ));
    }

    #[test]
    fn key_unknown_returns_handled() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::F(5), false),
            BrowserKeyAction::Handled
        ));
    }

    // ---- confirm delete key dispatch ----------------------------------------

    #[test]
    fn key_y_during_confirm_returns_confirm_yes() {
        let mut core = BrowserCore::new("host");
        core.confirm_delete = Some("remote:file:/tmp/x".to_string());
        assert!(matches!(
            handle_browser_key(&mut core, KeyCode::Char('y'), false),
            BrowserKeyAction::ConfirmDeleteYes
        ));
    }

    #[test]
    fn key_n_during_confirm_cancels() {
        let mut core = BrowserCore::new("host");
        core.confirm_delete = Some("remote:file:/tmp/x".to_string());
        let action = handle_browser_key(&mut core, KeyCode::Char('n'), false);
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert!(core.confirm_delete.is_none());
    }

    #[test]
    fn key_esc_during_confirm_cancels() {
        let mut core = BrowserCore::new("host");
        core.confirm_delete = Some("remote:file:/tmp/x".to_string());
        let action = handle_browser_key(&mut core, KeyCode::Esc, false);
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert!(core.confirm_delete.is_none());
    }

    #[test]
    fn key_random_during_confirm_is_swallowed() {
        let mut core = BrowserCore::new("host");
        core.confirm_delete = Some("remote:file:/tmp/x".to_string());
        let action = handle_browser_key(&mut core, KeyCode::Char('z'), false);
        assert!(matches!(action, BrowserKeyAction::Handled));
        // confirm_delete is still active
        assert!(core.confirm_delete.is_some());
    }

    // ---- BrowserCore::new ---------------------------------------------------

    #[test]
    fn new_core_defaults() {
        let core = BrowserCore::new("myhost");
        assert_eq!(core.host, "myhost");
        assert_eq!(core.remote_path, ".");
        assert!(core.remote_entries.is_empty());
        assert_eq!(core.focus, BrowserFocus::Local);
        assert!(core.confirm_delete.is_none());
        assert!(core.drive_picker.is_none());
        assert_eq!(core.local_scroll_x, 0);
        assert_eq!(core.remote_scroll_x, 0);
        assert_eq!(core.prompt_stable, 0);
        assert_eq!(core.status_msg, "Connecting…");
    }
}
