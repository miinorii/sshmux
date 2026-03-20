use std::{
    path::PathBuf,
    time::Instant,
};

use anyhow::Result;
use log::debug;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, StatefulWidget, Widget},
};

use super::parse::{
    FsEntry, list_drives, parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress,
    shell_quote, strip_ansi,
};
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BrowserFocus {
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SftpState {
    Connecting,
    Idle,
    WaitingPwd,
    WaitingLs,
    WaitingDelete,
    Transferring,
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

// ---------------------------------------------------------------------------
// FileBrowser
// ---------------------------------------------------------------------------

pub struct FileBrowser {
    pub host: String,
    pub sftp: EmbeddedTerminal,
    pub sftp_state: SftpState,

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
    pub prompt_stable: u8,
    pub prev_raw_len: usize,
    pub needs_redraw: bool,
    pub confirm_delete: Option<String>,
    pub pending_delete_name: Option<String>,
    pub drive_picker: Option<(Vec<PathBuf>, ListState)>,
    pub status_color: Color,
    pub cmd_start: Option<Instant>,
    pub last_duration: Option<std::time::Duration>,
    pub local_scroll_x: usize,
    pub remote_scroll_x: usize,
}

impl FileBrowser {
    pub fn new(host: &str) -> Result<Self> {
        let sftp = EmbeddedTerminal::sftp(host)?;
        let local_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local_entries = read_local_dir(&local_path);
        let mut local_sel = ListState::default();
        local_sel.select_first();
        let mut remote_sel = ListState::default();
        remote_sel.select_first();

        Ok(FileBrowser {
            host: host.to_string(),
            sftp,
            sftp_state: SftpState::Connecting,
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
            prompt_stable: 0,
            prev_raw_len: 0,
            needs_redraw: false,
            confirm_delete: None,
            pending_delete_name: None,
            drive_picker: None,
            status_color: Color::Yellow,
            cmd_start: None,
            last_duration: None,
            local_scroll_x: 0,
            remote_scroll_x: 0,
        })
    }

    pub fn tick(&mut self) {
        let cur_len = self.sftp.raw_len();
        if cur_len != self.prev_raw_len {
            self.prompt_stable = 0;
            self.prev_raw_len = cur_len;
        } else if self.prompt_raw_ends_with_prompt() {
            self.prompt_stable = self.prompt_stable.saturating_add(1);
        } else {
            self.prompt_stable = 0;
        }

        const STABLE_NEEDED: u8 = 2;
        let prompt_ready = self.prompt_stable >= STABLE_NEEDED;

        // Only snapshot raw output for display during connecting
        if matches!(self.sftp_state, SftpState::Connecting) {
            self.raw_snapshot = self.sftp.raw_lines();
        }

        match self.sftp_state {
            SftpState::Connecting => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.cmd_start = Some(Instant::now());
                    self.sftp.send_str("pwd\r\n");
                    self.sftp_state = SftpState::WaitingPwd;
                    self.status_msg = format!("Connected to {}", self.host);
                    self.status_color = Color::Green;
                    debug!("SFTP connected to {}, sent pwd", self.host);
                    self.needs_redraw = true;
                }
            }
            SftpState::WaitingPwd => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    self.remote_path =
                        parse_pwd(&lines).unwrap_or_else(|| self.remote_path.clone());
                    debug!("SFTP pwd => {}, sending ls -la", self.remote_path);
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                    self.needs_redraw = true;
                }
            }
            SftpState::WaitingLs => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    if let Some(p) = parse_pwd(&lines) {
                        self.remote_path = p;
                    }
                    let parsed = parse_ls(&lines);
                    debug!("SFTP ls done: {} entries", parsed.len());
                    self.remote_entries = parsed;
                    self.raw_snapshot.clear();
                    let max = self.remote_entries.len().saturating_sub(1);
                    let cur = self.remote_sel.selected().unwrap_or(0);
                    self.remote_sel.select(Some(cur.min(max)));
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp_state = SftpState::Idle;
                    self.stop_timer();
                    if self.status_color == Color::Yellow {
                        self.status_color = Color::Green;
                    }
                    self.needs_redraw = true;
                }
            }
            SftpState::Transferring => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    let completion_msg = self.last_transfer.as_mut().map(|t| {
                        t.done = true;
                        t.progress = "100%".to_string();
                        let verb = match t.direction {
                            TransferDirection::Download => "Downloaded",
                            TransferDirection::Upload => "Uploaded",
                        };
                        format!("{}: {}", verb, t.filename)
                    });
                    if let Some(msg) = completion_msg {
                        self.status_msg = msg;
                        self.status_color = Color::Green;
                    }
                    self.local_entries = read_local_dir(&self.local_path);
                    debug!("SFTP transfer complete");
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                } else {
                    let lines = self.sftp.raw_lines();
                    if let Some(ref mut t) = self.last_transfer {
                        if t.is_dir {
                            let count = lines.iter().filter(|l| {
                                l.contains("Fetching ") || l.contains("Uploading ")
                            }).count();
                            if count != t.file_count {
                                t.file_count = count;
                                self.needs_redraw = true;
                            }
                        } else if let Some(pct) = scrape_transfer_progress(&lines) {
                            t.progress = pct;
                            self.needs_redraw = true;
                        }
                    }
                }
            }
            SftpState::WaitingDelete => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    let has_error = lines.iter().any(|l| {
                        let t = l.to_lowercase();
                        t.contains("failure") || t.contains("couldn't") || t.contains("not empty") || t.contains("permission denied")
                    });
                    debug!("SFTP WaitingDelete complete, error={}", has_error);
                    if let Some(name) = self.pending_delete_name.take() {
                        if has_error {
                            self.status_msg = format!("Delete failed: {}", name);
                            self.status_color = Color::Red;
                        } else {
                            self.status_msg = format!("Deleted remote: {}", name);
                            self.status_color = Color::Green;
                        }
                    }
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                    self.needs_redraw = true;
                }
            }
            SftpState::Idle => {}
        }
    }

    fn prompt_raw_ends_with_prompt(&self) -> bool {
        let Ok(rb) = self.sftp.raw_output.lock() else {
            return false;
        };
        // Only check the last 64 bytes instead of stripping ANSI from the entire buffer
        let start = rb.len().saturating_sub(64);
        let tail = strip_ansi(&rb[start..]);
        tail.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.contains("sftp>"))
            .unwrap_or(false)
    }

    // ---- navigation --------------------------------------------------------

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

    pub fn enter(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
                // Drive picker active: Enter confirms the selected drive.
                if self.drive_picker.is_some() {
                    if let Some((drives, sel)) = self.drive_picker.take() {
                        if let Some(i) = sel.selected() {
                            if let Some(drive) = drives.get(i).cloned() {
                                self.local_path = drive;
                                self.local_entries = read_local_dir(&self.local_path);
                                self.local_sel.select_first();
                            }
                        }
                    }
                    self.needs_redraw = true;
                    return;
                }

                if let Some(i) = self.local_sel.selected() {
                    let entry = if let Some(e) = self.local_entries.get(i).cloned() {
                        e
                    } else {
                        return;
                    };
                    if entry.name == ".." {
                        if let Some(p) = self.local_path.parent() {
                            self.local_path = p.to_path_buf();
                        } else {
                            // At a filesystem root: show drive picker.
                            let drives = list_drives();
                            let mut drive_sel = ListState::default();
                            drive_sel.select_first();
                            self.drive_picker = Some((drives, drive_sel));
                            self.needs_redraw = true;
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
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                if let Some(i) = self.remote_sel.selected() {
                    if let Some(entry) = self.remote_entries.get(i).cloned() {
                        if entry.is_dir {
                            self.apply_cd(&entry.name);
                            self.status_msg = format!("Remote: {}", self.remote_path);
                            self.status_color = Color::Yellow;
                            self.sftp.drain_raw();
                            self.prev_raw_len = 0;
                            self.prompt_stable = 0;
                            self.send_ls();
                            self.sftp_state = SftpState::WaitingLs;
                            debug!("SFTP ls {}", self.remote_path);
                        } else {
                            self.download();
                        }
                    }
                }
            }
        }
    }

    /// Send `ls -la <remote_path>` using the client-tracked absolute path.
    fn send_ls(&mut self) {
        let cmd = format!("ls -la {}\r\n", shell_quote(&self.remote_path));
        self.cmd_start = Some(Instant::now());
        self.sftp.send_str(&cmd);
    }

    fn stop_timer(&mut self) {
        if let Some(start) = self.cmd_start.take() {
            self.last_duration = Some(start.elapsed());
        }
    }

    fn format_duration(d: std::time::Duration) -> String {
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

    fn apply_cd(&mut self, name: &str) {
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

    pub fn go_up(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
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
                    // At a filesystem root: show drive picker.
                    let drives = list_drives();
                    let mut drive_sel = ListState::default();
                    drive_sel.select_first();
                    self.drive_picker = Some((drives, drive_sel));
                    self.needs_redraw = true;
                }
            }
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                self.apply_cd("..");
                self.status_msg = format!("Remote: {}", self.remote_path);
                self.status_color = Color::Yellow;
                self.sftp.drain_raw();
                self.prev_raw_len = 0;
                self.prompt_stable = 0;
                self.send_ls();
                self.sftp_state = SftpState::WaitingLs;
                debug!("SFTP ls {}", self.remote_path);
            }
        }
    }

    // ---- transfers ---------------------------------------------------------

    pub fn download(&mut self) {
        if self.sftp_state != SftpState::Idle {
            return;
        }
        if let Some(i) = self.remote_sel.selected() {
            let entry = if let Some(e) = self.remote_entries.get(i).cloned() {
                e
            } else {
                return;
            };
            let local_dest = self.local_path.to_string_lossy().replace('\\', "/");
            let flag = if entry.is_dir { "-r " } else { "" };
            let remote_file = format!("{}/{}", self.remote_path.trim_end_matches('/'), entry.name);
            let cmd = format!("get {}{} {}/\r\n", flag, shell_quote(&remote_file), local_dest);
            self.last_transfer = Some(TransferStatus {
                filename: entry.name.clone(),
                direction: TransferDirection::Download,
                is_dir: entry.is_dir,
                done: false,
                progress: "0%".to_string(),
                file_count: 0,
            });
            self.status_msg = format!("Downloading {}...", entry.name);
            self.status_color = Color::Yellow;
            debug!("SFTP get {} -> {}", entry.name, local_dest);
            self.sftp.send_str(&cmd);
            self.sftp_state = SftpState::Transferring;
        }
    }

    pub fn upload(&mut self) {
        if self.sftp_state != SftpState::Idle {
            return;
        }
        if let Some(i) = self.local_sel.selected() {
            let entry = if let Some(e) = self.local_entries.get(i).cloned() {
                e
            } else {
                return;
            };
            let local_path = self.local_path.join(&entry.name);
            let local_str = local_path.to_string_lossy().replace('\\', "/");
            let flag = if entry.is_dir { "-r " } else { "" };
            let cmd = format!("put {}{} {}/\r\n", flag, shell_quote(&local_str), shell_quote(&self.remote_path));
            self.last_transfer = Some(TransferStatus {
                filename: entry.name.clone(),
                direction: TransferDirection::Upload,
                is_dir: entry.is_dir,
                done: false,
                progress: "0%".to_string(),
                file_count: 0,
            });
            self.status_msg = format!("Uploading {}...", entry.name);
            self.status_color = Color::Yellow;
            debug!("SFTP put {}", local_str);
            self.sftp.send_str(&cmd);
            self.sftp_state = SftpState::Transferring;
        }
    }

    pub fn delete_focused(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
                if let Some(i) = self.local_sel.selected() {
                    let entry = if let Some(e) = self.local_entries.get(i).cloned() {
                        e
                    } else {
                        return;
                    };
                    if entry.name == ".." {
                        return;
                    }
                    let kind = if entry.is_dir { "dir" } else { "file" };
                    self.confirm_delete = Some(format!("local:{}:{}", kind, entry.name));
                    self.needs_redraw = true;
                }
            }
            BrowserFocus::Remote => {
                if let Some(i) = self.remote_sel.selected() {
                    let entry = if let Some(e) = self.remote_entries.get(i).cloned() {
                        e
                    } else {
                        return;
                    };
                    if entry.name == ".." || self.sftp_state != SftpState::Idle {
                        return;
                    }
                    let kind = if entry.is_dir { "dir" } else { "file" };
                    self.confirm_delete = Some(format!("remote:{}:{}", kind, entry.name));
                    self.needs_redraw = true;
                }
            }
        }
    }

    pub fn confirm_delete_yes(&mut self) {
        if let Some(tagged) = self.confirm_delete.take() {
            if let Some(rest) = tagged.strip_prefix("local:") {
                let is_dir = rest.starts_with("dir:");
                let name = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
                let path = self.local_path.join(name);
                let result = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(e) = result {
                    self.status_msg = format!("Delete failed: {}", e);
                    self.status_color = Color::Red;
                } else {
                    self.status_msg = format!("Deleted local: {}", name);
                    self.status_color = Color::Green;
                    self.local_entries = read_local_dir(&self.local_path);
                }
                self.last_duration = None;
                self.needs_redraw = true;
            } else if let Some(rest) = tagged.strip_prefix("remote:") {
                let is_dir = rest.starts_with("dir:");
                let name = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
                let remote_file = format!("{}/{}", self.remote_path.trim_end_matches('/'), name);
                let cmd = if is_dir {
                    format!("rmdir {}\r\n", shell_quote(&remote_file))
                } else {
                    format!("rm {}\r\n", shell_quote(&remote_file))
                };
                self.sftp.send_str(&cmd);
                self.sftp_state = SftpState::WaitingDelete;
                self.status_msg = format!("Deleting {}...", name);
                self.status_color = Color::Yellow;
                self.pending_delete_name = Some(name.to_string());
                self.needs_redraw = true;
            }
        }
    }

    pub fn confirm_delete_no(&mut self) {
        self.confirm_delete = None;
        self.status_msg = String::from("Deletion cancelled.");
        self.status_color = Color::Yellow;
        self.needs_redraw = true;
    }

    pub fn drag_local_to_remote(&mut self) {
        self.upload();
    }
    pub fn drag_remote_to_local(&mut self) {
        self.download();
    }

    /// Select the list item under the mouse click, mirroring the render layout.
    pub fn click_select(&mut self, col: u16, row: u16, pane_area: Rect, leaf_count: usize) {
        // Replicate the outer block from render()
        let outer_inner = if leaf_count > 1 {
            Rect {
                x: pane_area.x + 1,
                y: pane_area.y + 1,
                width: pane_area.width.saturating_sub(2),
                height: pane_area.height.saturating_sub(2),
            }
        } else {
            pane_area
        };

        let panels_area = Rect {
            height: outer_inner.height.saturating_sub(2), // status bar + gap
            ..outer_inner
        };

        let half = panels_area.width / 2;
        let in_remote = col >= panels_area.x + half;
        let panel_area = if in_remote {
            Rect {
                x: panels_area.x + half,
                width: panels_area.width - half,
                ..panels_area
            }
        } else {
            Rect {
                width: half,
                ..panels_area
            }
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

    // ---- render ------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool, leaf_count: usize) {
        let inner = if leaf_count > 1 {
            let border_style = if is_focus {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let title = format!(" sftp: {} ", self.host);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title.as_str());
            let inner = block.inner(area);
            block.render(area, buf);
            inner
        } else {
            area
        };

        let status_h = 1u16;
        let panels_area = Rect {
            height: inner.height.saturating_sub(status_h),
            ..inner
        };
        let status_area = Rect {
            y: inner.y + inner.height.saturating_sub(status_h),
            height: status_h,
            ..inner
        };

        let half = panels_area.width / 2;
        let local_area = Rect {
            width: half,
            ..panels_area
        };
        let remote_area = Rect {
            x: panels_area.x + half,
            width: panels_area.width - half,
            ..panels_area
        };

        self.render_panel(local_area, buf, BrowserFocus::Local, is_focus);
        self.render_panel(remote_area, buf, BrowserFocus::Remote, is_focus);
        self.render_status(status_area, buf);
    }

    fn render_panel(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        side: BrowserFocus,
        pane_focused: bool,
    ) {
        let is_active = self.focus == side && pane_focused;

        // Title and path are computed separately so we can check drive_picker
        // before borrowing entries/list_state.
        let title = match side {
            BrowserFocus::Local if self.drive_picker.is_some() => " select drive ",
            BrowserFocus::Local => " local ",
            BrowserFocus::Remote => " remote ",
        };
        let path_str = match side {
            BrowserFocus::Local => self.local_path.to_string_lossy().to_string(),
            BrowserFocus::Remote => self.remote_path.clone(),
        };

        let border_col = if is_active { Color::Cyan } else { Color::DarkGray };
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
        if side == BrowserFocus::Local {
            if let Some((drives, drive_sel)) = &mut self.drive_picker {
                let items: Vec<String> = drives
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                let list = List::new(items)
                    .highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    );
                StatefulWidget::render(list, inner, buf, drive_sel);
                return;
            }
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
        let meta_width: usize = 9 + 1 + 16 + 1 + 10; // size + gap + modified + gap + perms

        // Find the longest display name to align metadata columns
        let max_name_len = entries.iter().map(|e| {
            if e.is_dir { e.name.len() + 1 } else { e.name.len() }
        }).max().unwrap_or(0);

        // Virtual row width: at least panel width so metadata is right-aligned
        let virtual_width = (max_name_len + 1 + meta_width).max(w);

        // Clamp scroll so the end of metadata aligns with the right edge
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
                let full = format!(
                    "{}{:gap$}{}",
                    display_name,
                    "",
                    meta,
                    gap = gap,
                );

                let scrolled: String = full.chars().skip(sx).take(w).collect();
                let padded = format!("{:<width$}", scrolled, width = w);

                // Color: name portion vs metadata
                let visible_name_chars = if sx < name_len {
                    (name_len - sx).min(w)
                } else {
                    0
                };

                if visible_name_chars == 0 {
                    let line = Line::from(Span::styled(padded, Style::default().fg(Color::DarkGray)));
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

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(if is_active { Color::Cyan } else { Color::DarkGray })
                    .add_modifier(Modifier::BOLD),
            );
        StatefulWidget::render(list, inner, buf, list_state);
    }

    fn render_status(&self, area: Rect, buf: &mut Buffer) {
        if let Some(ref tagged) = self.confirm_delete {
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
            return;
        }

        let (state_label, state_col) = match self.sftp_state {
            SftpState::Connecting => ("[connecting]", Color::Yellow),
            SftpState::WaitingPwd | SftpState::WaitingLs => {
                ("[loading]", Color::Yellow)
            }
            SftpState::Idle => ("[idle]", self.status_color),
            SftpState::WaitingDelete => ("[deleting]", Color::Yellow),
            SftpState::Transferring => ("[transfer]", Color::Green),
        };

        let progress_suffix = if let Some(ref t) = self.last_transfer {
            if !t.done {
                if t.is_dir {
                    format!(" ({} files)", t.file_count)
                } else {
                    format!(" {}", t.progress)
                }
            } else {
                if t.is_dir {
                    format!(" ({} files)", t.file_count)
                } else {
                    String::new()
                }
            }
        } else {
            String::new()
        };

        let help = " [T]xfer [Del]rm ";
        let help_len = help.chars().count() as u16;
        let help_x = area.x + area.width.saturating_sub(help_len);

        // When busy, message color follows the state badge.
        // When idle, both use status_color to reflect the last operation result.
        let msg_color = state_col;

        let duration_suffix = if let Some(d) = self.last_duration {
            format!(" ({})", Self::format_duration(d))
        } else {
            String::new()
        };

        // Left: [state] message (duration)
        let left_line = Line::from(vec![
            Span::styled(
                format!("[{}]", state_label.trim_matches(|c| c == '[' || c == ']')),
                Style::default().fg(state_col).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}{}", self.status_msg, progress_suffix),
                Style::default().fg(msg_color),
            ),
            Span::styled(
                duration_suffix,
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        buf.set_line(area.x, area.y, &left_line, help_x.saturating_sub(area.x));

        // Right: shortcuts
        buf.set_span(
            help_x,
            area.y,
            &Span::styled(help, Style::default().fg(Color::DarkGray)),
            area.width.saturating_sub(help_x.saturating_sub(area.x)),
        );
    }
}
