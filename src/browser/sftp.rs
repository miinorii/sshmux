use std::time::Instant;

use anyhow::Result;
use log::{debug, info, warn};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use super::common::{Browser, BrowserCore, BrowserFocus, TransferDirection, TransferStatus};
use super::parse::{
    parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress, shell_quote, strip_ansi,
};
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SftpState {
    Connecting,
    Idle,
    WaitingPwd,
    WaitingLs,
    WaitingDelete,
    Transferring,
}

// ---------------------------------------------------------------------------
// FileBrowser
// ---------------------------------------------------------------------------

pub struct FileBrowser {
    pub core: BrowserCore,
    pub sftp: EmbeddedTerminal,
    pub sftp_state: SftpState,
}

impl FileBrowser {
    pub fn new(host: &str) -> Result<Self> {
        let sftp = EmbeddedTerminal::sftp(host)?;
        Ok(FileBrowser {
            core: BrowserCore::new(host),
            sftp,
            sftp_state: SftpState::Connecting,
        })
    }

    pub fn tick(&mut self) {
        self.core.check_paste_deadline();
        let cur_len = self.sftp.raw_len();
        if cur_len != self.core.prev_raw_len {
            self.core.prompt_stable = 0;
            self.core.prev_raw_len = cur_len;
        } else if self.prompt_raw_ends_with_prompt() {
            self.core.prompt_stable = self.core.prompt_stable.saturating_add(1);
        } else {
            self.core.prompt_stable = 0;
        }

        const STABLE_NEEDED: u8 = 2;
        let prompt_ready = self.core.prompt_stable >= STABLE_NEEDED;

        if matches!(self.sftp_state, SftpState::Connecting) {
            self.core.raw_snapshot = self.sftp.raw_lines();
        }

        match self.sftp_state {
            SftpState::Connecting => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    self.sftp.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.core.cmd_start = Some(Instant::now());
                    self.sftp.send_str("pwd\r\n");
                    self.sftp_state = SftpState::WaitingPwd;
                    self.core.status_msg = format!("Connected to {}", self.core.host);
                    self.core.status_color = Color::Green;
                    info!("SFTP connected to {}", self.core.host);
                    self.core.needs_redraw = true;
                }
            }
            SftpState::WaitingPwd => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    self.core.remote_path =
                        parse_pwd(&lines).unwrap_or_else(|| self.core.remote_path.clone());
                    debug!("SFTP pwd => {}, sending ls -la", self.core.remote_path);
                    self.sftp.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            SftpState::WaitingLs => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    if let Some(p) = parse_pwd(&lines) {
                        self.core.remote_path = p;
                    }
                    let parsed = parse_ls(&lines);
                    debug!("SFTP ls done: {} entries", parsed.len());
                    self.core.remote_entries = parsed;
                    self.core.raw_snapshot.clear();
                    let max = self.core.remote_entries.len().saturating_sub(1);
                    let cur = self.core.remote_sel.selected().unwrap_or(0);
                    self.core.remote_sel.select(Some(cur.min(max)));
                    self.sftp.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.sftp_state = SftpState::Idle;
                    self.core.stop_timer();
                    if self.core.status_color == Color::Yellow {
                        self.core.status_color = Color::Green;
                    }
                    self.core.needs_redraw = true;
                    // Chain next queued transfer if any
                    if !self.core.pending_uploads.is_empty() {
                        self.upload_pending_paths();
                    } else if !self.core.pending_transfers.is_empty() {
                        let direction = self
                            .core
                            .last_transfer
                            .as_ref()
                            .map(|t| t.direction)
                            .unwrap_or(TransferDirection::Upload);
                        match direction {
                            TransferDirection::Upload => self.upload(),
                            TransferDirection::Download => self.download(),
                        }
                    } else if self.core.pop_pending_delete() {
                        self.confirm_delete_yes();
                    }
                }
            }
            SftpState::Transferring => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let completion_msg = self.core.last_transfer.as_mut().map(|t| {
                        t.done = true;
                        t.progress = "100%".to_string();
                        let verb = match t.direction {
                            TransferDirection::Download => "Downloaded",
                            TransferDirection::Upload => "Uploaded",
                        };
                        format!("{}: {}", verb, t.filename)
                    });
                    if let Some(msg) = completion_msg {
                        self.core.status_msg = msg;
                        self.core.status_color = Color::Green;
                    }
                    self.core.local_entries = read_local_dir(&self.core.local_path);
                    info!("SFTP transfer complete");
                    self.sftp.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                } else {
                    let lines = self.sftp.raw_lines();
                    if let Some(ref mut t) = self.core.last_transfer {
                        if t.is_dir {
                            let count = lines
                                .iter()
                                .filter(|l| l.contains("Fetching ") || l.contains("Uploading "))
                                .count();
                            if count != t.file_count {
                                t.file_count = count;
                                self.core.needs_redraw = true;
                            }
                        } else if let Some(pct) = scrape_transfer_progress(&lines) {
                            t.progress = pct;
                            self.core.needs_redraw = true;
                        }
                    }
                }
            }
            SftpState::WaitingDelete => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    let has_error = lines.iter().any(|l| {
                        let t = l.to_lowercase();
                        t.contains("failure")
                            || t.contains("couldn't")
                            || t.contains("not empty")
                            || t.contains("permission denied")
                    });
                    if let Some(name) = self.core.pending_delete_name.take() {
                        if has_error {
                            warn!("SFTP delete failed: {}", name);
                            self.core.status_msg = format!("Delete failed: {}", name);
                            self.core.status_color = Color::Red;
                            self.core.pending_deletes.clear();
                        } else {
                            self.core.status_msg = format!("Deleted remote: {}", name);
                            self.core.status_color = Color::Green;
                        }
                    }
                    self.sftp.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            SftpState::Idle => {}
        }
    }

    fn prompt_raw_ends_with_prompt(&self) -> bool {
        let Ok(rb) = self.sftp.raw_output.lock() else {
            return false;
        };
        let start = rb.len().saturating_sub(64);
        let tail = strip_ansi(&rb[start..]);
        tail.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.contains("sftp>"))
            .unwrap_or(false)
    }

    // ---- navigation (delegates to core for local, handles remote) ----------

    pub fn enter(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => self.core.local_enter(),
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                if let Some(i) = self.core.remote_sel.selected()
                    && let Some(entry) = self.core.remote_entries.get(i).cloned()
                {
                    if entry.is_dir {
                        self.core.apply_cd(&entry.name);
                        self.core.status_msg = format!("Remote: {}", self.core.remote_path);
                        self.core.status_color = Color::Yellow;
                        self.sftp.drain_raw();
                        self.core.prev_raw_len = 0;
                        self.core.prompt_stable = 0;
                        self.send_ls();
                        self.sftp_state = SftpState::WaitingLs;
                        debug!("SFTP ls {}", self.core.remote_path);
                    } else {
                        self.download();
                    }
                }
            }
        }
    }

    pub fn go_up(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => self.core.local_go_up(),
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                self.core.apply_cd("..");
                self.core.status_msg = format!("Remote: {}", self.core.remote_path);
                self.core.status_color = Color::Yellow;
                self.sftp.drain_raw();
                self.core.prev_raw_len = 0;
                self.core.prompt_stable = 0;
                self.send_ls();
                self.sftp_state = SftpState::WaitingLs;
                debug!("SFTP ls {}", self.core.remote_path);
            }
        }
    }

    fn send_ls(&mut self) {
        let cmd = format!("ls -la {}\r\n", shell_quote(&self.core.remote_path));
        self.core.cmd_start = Some(Instant::now());
        self.sftp.send_str(&cmd);
    }

    // ---- transfers ---------------------------------------------------------

    pub fn download(&mut self) {
        if self.sftp_state != SftpState::Idle {
            return;
        }
        let idx = if !self.core.pending_transfers.is_empty() {
            let Some(i) = self.core.pop_pending_transfer() else {
                return;
            };
            i
        } else if let Some(i) = self.core.remote_sel.selected() {
            i
        } else {
            return;
        };
        let Some(entry) = self.core.remote_entries.get(idx).cloned() else {
            return;
        };
        if entry.name == ".." {
            return;
        }
        let local_dest = self.core.local_path.to_string_lossy().replace('\\', "/");
        let flag = if entry.is_dir { "-r " } else { "" };
        let remote_file = format!(
            "{}/{}",
            self.core.remote_path.trim_end_matches('/'),
            entry.name
        );
        let remaining = self.core.pending_transfers.len();
        let suffix = if remaining > 0 {
            format!(" ({} more queued)", remaining)
        } else {
            String::new()
        };
        let cmd = format!(
            "get {}{} {}/\r\n",
            flag,
            shell_quote(&remote_file),
            local_dest
        );
        self.core.last_transfer = Some(TransferStatus {
            filename: entry.name.clone(),
            direction: TransferDirection::Download,
            is_dir: entry.is_dir,
            done: false,
            progress: "0%".to_string(),
            file_count: 0,
        });
        self.core.status_msg = format!("Downloading {}...{}", entry.name, suffix);
        self.core.status_color = Color::Yellow;
        info!("SFTP get {} -> {}", entry.name, local_dest);
        self.sftp.send_str(&cmd);
        self.sftp_state = SftpState::Transferring;
    }

    pub fn upload(&mut self) {
        if self.sftp_state != SftpState::Idle {
            return;
        }
        let idx = if !self.core.pending_transfers.is_empty() {
            let Some(i) = self.core.pop_pending_transfer() else {
                return;
            };
            i
        } else if let Some(i) = self.core.local_sel.selected() {
            i
        } else {
            return;
        };
        let Some(entry) = self.core.local_entries.get(idx).cloned() else {
            return;
        };
        if entry.name == ".." {
            return;
        }
        let local_path = self.core.local_path.join(&entry.name);
        let local_str = local_path.to_string_lossy().replace('\\', "/");
        let flag = if entry.is_dir { "-r " } else { "" };
        let remaining = self.core.pending_transfers.len();
        let suffix = if remaining > 0 {
            format!(" ({} more queued)", remaining)
        } else {
            String::new()
        };
        let cmd = format!(
            "put {}{} {}/\r\n",
            flag,
            shell_quote(&local_str),
            shell_quote(&self.core.remote_path)
        );
        self.core.last_transfer = Some(TransferStatus {
            filename: entry.name.clone(),
            direction: TransferDirection::Upload,
            is_dir: entry.is_dir,
            done: false,
            progress: "0%".to_string(),
            file_count: 0,
        });
        self.core.status_msg = format!("Uploading {}...{}", entry.name, suffix);
        self.core.status_color = Color::Yellow;
        info!("SFTP put {}", local_str);
        self.sftp.send_str(&cmd);
        self.sftp_state = SftpState::Transferring;
    }

    // ---- drop upload -------------------------------------------------------

    pub fn upload_pending_paths(&mut self) {
        if self.sftp_state != SftpState::Idle {
            return;
        }
        let Some(path) = self.core.pending_uploads.first().cloned() else {
            return;
        };
        self.core.pending_uploads.remove(0);
        self.core.upload_scroll_x = 0;
        self.core.upload_scroll_y = 0;
        let remaining = self.core.pending_uploads.len();

        let is_dir = path.is_dir();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let local_str = path.to_string_lossy().replace('\\', "/");
        let flag = if is_dir { "-r " } else { "" };
        let cmd = format!(
            "put {}{} {}/\r\n",
            flag,
            shell_quote(&local_str),
            shell_quote(&self.core.remote_path)
        );
        self.core.last_transfer = Some(TransferStatus {
            filename: name.clone(),
            direction: TransferDirection::Upload,
            is_dir,
            done: false,
            progress: "0%".to_string(),
            file_count: 0,
        });
        let suffix = if remaining > 0 {
            format!(" (+{} queued)", remaining)
        } else {
            String::new()
        };
        self.core.status_msg = format!("Uploading {}...{}", name, suffix);
        self.core.status_color = Color::Yellow;
        self.core.upload_scroll_x = 0;
        self.core.upload_scroll_y = 0;
        info!("SFTP put (drop) {}", local_str);
        self.sftp.send_str(&cmd);
        self.sftp_state = SftpState::Transferring;
    }

    // ---- delete ------------------------------------------------------------

    pub fn delete_focused(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => {
                let indices = self.core.selected_indices();
                if indices.len() > 1 {
                    self.core.local_delete_selected();
                } else {
                    self.core.clear_selection();
                    self.core.local_delete_focused();
                }
            }
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                self.core.remote_delete_focused();
            }
        }
    }

    pub fn confirm_delete_yes(&mut self) {
        if self.core.local_confirm_delete() {
            return;
        }
        if let Some(tagged) = self.core.confirm_delete.take()
            && let Some(rest) = tagged.strip_prefix("remote:")
        {
            let is_dir = rest.starts_with("dir:");
            let name = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
            info!("SFTP remote delete: {}", name);
            let cmd = if is_dir {
                format!("rmdir {}\r\n", shell_quote(name))
            } else {
                format!("rm {}\r\n", shell_quote(name))
            };
            self.sftp.drain_raw();
            self.core.prev_raw_len = 0;
            self.core.prompt_stable = 0;
            self.sftp.send_str(&cmd);
            self.sftp_state = SftpState::WaitingDelete;
            self.core.status_msg = format!("Deleting {}...", name);
            self.core.status_color = Color::Yellow;
            self.core.pending_delete_name = Some(name.to_string());
            self.core.needs_redraw = true;
        }
    }

    // ---- render ------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool, leaf_count: usize) {
        let title = format!(" sftp: {} ", self.core.host);
        let status_area = self
            .core
            .render_panels(area, buf, is_focus, leaf_count, &title);
        if !self.core.render_confirm_delete(status_area, buf) {
            let (label, color) = self.state_label();
            let progress = self.progress_suffix();
            self.core
                .render_normal_status(status_area, buf, label, color, &progress);
        }
        self.core.render_upload_confirm(area, buf);
        self.core.render_drag_arrow(area, buf, leaf_count);
        self.core.render_drag_ghost(buf);
    }

    fn state_label(&self) -> (&str, Color) {
        match self.sftp_state {
            SftpState::Connecting => ("connecting", Color::Yellow),
            SftpState::WaitingPwd | SftpState::WaitingLs => ("loading", Color::Yellow),
            SftpState::Idle => ("idle", self.core.status_color),
            SftpState::WaitingDelete => ("deleting", Color::Yellow),
            SftpState::Transferring => ("transfer", Color::Green),
        }
    }

    fn progress_suffix(&self) -> String {
        if let Some(ref t) = self.core.last_transfer {
            if !t.done {
                if t.is_dir {
                    format!(" ({} files)", t.file_count)
                } else {
                    format!(" {}", t.progress)
                }
            } else if t.is_dir {
                format!(" ({} files)", t.file_count)
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }
}

impl Browser for FileBrowser {
    fn core_mut(&mut self) -> &mut BrowserCore {
        &mut self.core
    }
    fn upload(&mut self) {
        self.upload();
    }
    fn download(&mut self) {
        self.download();
    }
}
