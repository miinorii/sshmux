use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::log;
use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, StatefulWidget, Widget},
};
use std::io::Write;

use crate::sftp_parse::{
    FsEntry, local_root, parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress,
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
    WaitingCd,
    WaitingDelete,
    Transferring,
}

#[derive(Clone)]
pub struct TransferStatus {
    pub filename: String,
    pub done: bool,
    pub progress: String,
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
    pub log: Option<Arc<Mutex<std::fs::File>>>,
}

impl FileBrowser {
    pub fn new(host: &str, log: Option<Arc<Mutex<std::fs::File>>>) -> Result<Self> {
        let sftp = EmbeddedTerminal::sftp(host, log.clone())?;
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
            log: log.clone(),
        })
    }

    pub fn tick(&mut self) {
        if !matches!(self.sftp_state, SftpState::Idle) {
            self.raw_snapshot = self.sftp.raw_lines();
        }

        let cur_len = self.sftp.raw_len();
        if cur_len != self.prev_raw_len {
            self.prompt_stable = 0;
            self.prev_raw_len = cur_len;
        } else if self.prompt_raw_ends_with_prompt() {
            self.prompt_stable = self.prompt_stable.saturating_add(1);
        } else {
            self.prompt_stable = 0;
        }

        const STABLE_NEEDED: u8 = 3;
        let prompt_ready = self.prompt_stable >= STABLE_NEEDED;

        match self.sftp_state {
            SftpState::Connecting => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp.send_str("pwd\r\n");
                    self.sftp_state = SftpState::WaitingPwd;
                    self.status_msg = format!("Connected to {}", self.host);
                    log!(self.log, "SFTP connected to {}, sent pwd", self.host);
                    self.needs_redraw = true;
                }
            }
            SftpState::WaitingCd => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp.send_str("pwd\r\n");
                    self.sftp_state = SftpState::WaitingPwd;
                }
            }
            SftpState::WaitingPwd => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    let lines = self.sftp.raw_lines();
                    self.remote_path =
                        parse_pwd(&lines).unwrap_or_else(|| self.remote_path.clone());
                    log!(self.log, "SFTP pwd => {}, sending ls -la", self.remote_path);
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp.send_str("ls -la\r\n");
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
                    log!(self.log, "SFTP ls done: {} entries", parsed.len());
                    if parsed.len() > 1 {
                        self.remote_entries = parsed;
                        self.raw_snapshot.clear();
                        let max = self.remote_entries.len().saturating_sub(1);
                        let cur = self.remote_sel.selected().unwrap_or(0);
                        self.remote_sel.select(Some(cur.min(max)));
                    }
                    if self.remote_sel.selected().is_none() {
                        self.remote_sel.select_first();
                    }
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp_state = SftpState::Idle;
                    self.needs_redraw = true;
                }
            }
            SftpState::Transferring => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    if let Some(ref mut t) = self.last_transfer {
                        t.done = true;
                        t.progress = "100%".to_string();
                    }
                    self.local_entries = read_local_dir(&self.local_path);
                    log!(self.log, "SFTP transfer complete");
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp.send_str("ls -la\r\n");
                    self.sftp_state = SftpState::WaitingLs;
                } else {
                    let lines = self.sftp.raw_lines();
                    if let Some(pct) = scrape_transfer_progress(&lines) {
                        if let Some(ref mut t) = self.last_transfer {
                            t.progress = pct;
                            self.needs_redraw = true;
                        }
                    }
                }
            }
            SftpState::WaitingDelete => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    log!(self.log, "SFTP WaitingDelete complete");
                    if let Some(name) = self.pending_delete_name.take() {
                        self.status_msg = format!("Deleted remote: {}", name);
                    }
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp.send_str("ls -la\r\n");
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
        let text = strip_ansi(&rb);
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.contains("sftp>"))
            .unwrap_or(false)
    }

    // ---- navigation --------------------------------------------------------

    pub fn nav_up(&mut self) {
        match self.focus {
            BrowserFocus::Local => self.local_sel.select_previous(),
            BrowserFocus::Remote => self.remote_sel.select_previous(),
        }
    }

    pub fn nav_down(&mut self) {
        match self.focus {
            BrowserFocus::Local => self.local_sel.select_next(),
            BrowserFocus::Remote => self.remote_sel.select_next(),
        }
    }

    pub fn enter(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
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
                            self.local_path = PathBuf::from(local_root());
                        }
                    } else if entry.is_dir {
                        self.local_path.push(&entry.name);
                    } else {
                        return;
                    }
                    self.local_entries = read_local_dir(&self.local_path);
                    self.local_sel.select_first();
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
                            self.sftp
                                .send_str(&format!("cd {}\r\n", shell_quote(&entry.name)));
                            self.sftp_state = SftpState::WaitingCd;
                            log!(self.log, "SFTP cd {}", entry.name);
                        } else {
                            self.download();
                        }
                    }
                }
            }
        }
    }

    pub fn go_up(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
                if let Some(p) = self.local_path.parent() {
                    self.local_path = p.to_path_buf();
                } else {
                    self.local_path = PathBuf::from(local_root());
                }
                self.local_entries = read_local_dir(&self.local_path);
                self.local_sel.select_first();
            }
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                self.sftp.send_str("cd ..\r\n");
                self.sftp_state = SftpState::WaitingCd;
                log!(self.log, "SFTP cd ..");
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
            if entry.is_dir {
                return;
            }
            let local_dest = self.local_path.to_string_lossy().replace('\\', "/");
            let cmd = format!("get {} {}/\r\n", shell_quote(&entry.name), local_dest);
            self.last_transfer = Some(TransferStatus {
                filename: entry.name.clone(),
                done: false,
                progress: "0%".to_string(),
            });
            self.status_msg = format!("Downloading {}...", entry.name);
            log!(self.log, "SFTP get {} -> {}", entry.name, local_dest);
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
            if entry.is_dir {
                return;
            }
            let local_path = self.local_path.join(&entry.name);
            let local_str = local_path.to_string_lossy().replace('\\', "/");
            let cmd = format!("put {}\r\n", shell_quote(&local_str));
            self.last_transfer = Some(TransferStatus {
                filename: entry.name.clone(),
                done: false,
                progress: "0%".to_string(),
            });
            self.status_msg = format!("Uploading {}...", entry.name);
            log!(self.log, "SFTP put {}", local_str);
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
                    self.confirm_delete = Some(format!("local:{}", entry.name));
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
                    self.confirm_delete = Some(format!("remote:{}", entry.name));
                    self.needs_redraw = true;
                }
            }
        }
    }

    pub fn confirm_delete_yes(&mut self) {
        if let Some(tagged) = self.confirm_delete.take() {
            if let Some(name) = tagged.strip_prefix("local:") {
                let path = self.local_path.join(name);
                if let Err(e) = std::fs::remove_file(&path) {
                    self.status_msg = format!("Delete failed: {}", e);
                } else {
                    self.status_msg = format!("Deleted local: {}", name);
                    self.local_entries = read_local_dir(&self.local_path);
                }
                self.needs_redraw = true;
            } else if let Some(name) = tagged.strip_prefix("remote:") {
                let cmd = format!("rm {}\r\n", shell_quote(name));
                self.sftp.send_str(&cmd);
                self.sftp_state = SftpState::WaitingDelete;
                self.status_msg = format!("Deleting {}...", name);
                self.pending_delete_name = Some(name.to_string());
                self.needs_redraw = true;
            }
        }
    }

    pub fn confirm_delete_no(&mut self) {
        self.confirm_delete = None;
        self.status_msg = String::from("Deletion cancelled.");
        self.needs_redraw = true;
    }

    pub fn drag_local_to_remote(&mut self) {
        self.upload();
    }
    pub fn drag_remote_to_local(&mut self) {
        self.download();
    }

    // ---- render ------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool, leaf_count: usize) {
        let inner = if leaf_count > 1 {
            let border_style = if is_focus {
                Style::default().fg(Color::Blue)
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
            height: inner.height.saturating_sub(status_h + 1),
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
        let (title, path_str, entries, list_state) = match side {
            BrowserFocus::Local => (
                " local ",
                self.local_path.to_string_lossy().to_string(),
                &self.local_entries,
                &mut self.local_sel,
            ),
            BrowserFocus::Remote => (
                " remote ",
                self.remote_path.clone(),
                &self.remote_entries,
                &mut self.remote_sel,
            ),
        };

        let border_col = if is_active {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_col))
            .title(Span::styled(title, Style::default().fg(Color::Yellow)))
            .title_bottom(Span::styled(
                format!(" {} ", path_str),
                Style::default().fg(Color::DarkGray),
            ));
        let inner = block.inner(area);
        block.render(area, buf);

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

        const W_SIZE: usize = 9;
        const W_MOD: usize = 16;
        const W_PERMS: usize = 10;
        const W_GAPS: usize = 3;
        let w_name = (inner.width as usize).saturating_sub(W_SIZE + W_MOD + W_PERMS + W_GAPS);

        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let name_col = if e.is_dir { Color::Cyan } else { Color::White };
                let display_name = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };
                let name_trunc: String = display_name.chars().take(w_name).collect();
                let name_padded = format!("{:<width$}", name_trunc, width = w_name);
                let line = Line::from(vec![
                    Span::styled(name_padded, Style::default().fg(name_col)),
                    Span::styled(
                        format!(" {:>W_SIZE$}", e.size),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!(" {:<W_MOD$}", e.modified),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!(" {:<W_PERMS$}", e.perms),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(if is_active {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        StatefulWidget::render(list, inner, buf, list_state);
    }

    fn render_status(&self, area: Rect, buf: &mut Buffer) {
        if let Some(ref tagged) = self.confirm_delete {
            let name = tagged
                .strip_prefix("local:")
                .or_else(|| tagged.strip_prefix("remote:"))
                .unwrap_or(tagged);
            let side = if tagged.starts_with("local:") {
                "local"
            } else {
                "remote"
            };
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
            SftpState::WaitingCd => ("[cd…]", Color::Yellow),
            SftpState::WaitingPwd => ("[pwd…]", Color::Yellow),
            SftpState::Idle => ("[idle]", Color::DarkGray),
            SftpState::WaitingLs => ("[ls…]", Color::Yellow),
            SftpState::WaitingDelete => ("[deleting…]", Color::Red),
            SftpState::Transferring => ("[xfer…]", Color::Green),
        };

        let transfer_str = if let Some(ref t) = self.last_transfer {
            if t.done {
                format!("✓ {}  ", t.filename)
            } else {
                format!("⟳ {} {}  ", t.filename, t.progress)
            }
        } else {
            String::new()
        };

        let parse_hint = if self.remote_entries.len() <= 1 && !self.raw_snapshot.is_empty() {
            self.raw_snapshot
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| format!(" | {}", l.trim().chars().take(40).collect::<String>()))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let help = "Tab:switch  Spc/Enter:cd  Bksp:up  F5:Download  F6:Upload  Del:rm";
        buf.set_line(
            area.x,
            area.y,
            &Line::from(vec![
                Span::styled(state_label, Style::default().fg(state_col)),
                Span::styled(
                    format!(
                        " {}{}  {}  {}",
                        self.status_msg, parse_hint, transfer_str, help
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            area.width,
        );
    }
}
