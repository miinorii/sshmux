use std::time::Instant;

use anyhow::Result;
use log::{debug, error, info, warn};
use portable_pty::CommandBuilder;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};

use super::common::{BrowserCore, BrowserFocus, TransferDirection, TransferStatus};
use super::parse::{
    parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress, shell_quote, strip_ansi,
};
use crate::terminal::EmbeddedTerminal;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SshBrowserState {
    Connecting,
    SettingPrompt,
    WaitingPwd,
    WaitingLs,
    WaitingDelete,
    Transferring,
    Idle,
}

// ---------------------------------------------------------------------------
// SshBrowser
// ---------------------------------------------------------------------------

pub struct SshBrowser {
    pub core: BrowserCore,
    pub ssh: EmbeddedTerminal,
    pub scp_pty: Option<EmbeddedTerminal>,
    pub ssh_state: SshBrowserState,

    pub saved_password: Option<String>,
    pub password_buf: String,
    pub waiting_password: bool,
    pub password_prompts_seen: usize,
}

impl SshBrowser {
    pub fn new(host: &str) -> Result<Self> {
        let ssh = EmbeddedTerminal::ssh_shell(host)?;
        Ok(SshBrowser {
            core: BrowserCore::new(host),
            ssh,
            scp_pty: None,
            ssh_state: SshBrowserState::Connecting,
            saved_password: None,
            password_buf: String::new(),
            waiting_password: false,
            password_prompts_seen: 0,
        })
    }

    pub fn tick(&mut self) {
        self.core.check_paste_deadline();
        // --- SCP transfer monitoring ---
        if self.ssh_state == SshBrowserState::Transferring {
            if self.waiting_password {
                return;
            }
            if let Some(ref mut scp) = self.scp_pty {
                let exited = scp.process_exited();
                let lines = scp.raw_lines();

                // Detect password prompt
                let pw_count = lines
                    .iter()
                    .filter(|l| l.trim().to_lowercase().ends_with("password:"))
                    .count();
                if pw_count > self.password_prompts_seen {
                    self.password_prompts_seen = pw_count;
                    if let Some(ref pw) = self.saved_password {
                        if pw_count > 1 {
                            warn!("SCP password rejected, re-prompting");
                            self.saved_password = None;
                            self.password_buf.clear();
                            self.waiting_password = true;
                            self.core.status_msg = "Wrong password, try again".to_string();
                            self.core.status_color = Color::Red;
                            self.core.needs_redraw = true;
                        } else {
                            debug!("SCP auto-sending saved password");
                            scp.send_str(&format!("{}\r\n", pw));
                            if let Some(ref t) = self.core.last_transfer {
                                let verb = match t.direction {
                                    TransferDirection::Download => "Downloading",
                                    TransferDirection::Upload => "Uploading",
                                };
                                self.core.status_msg = format!("{}...", verb);
                            }
                            self.core.status_color = Color::Yellow;
                            self.core.needs_redraw = true;
                        }
                    } else {
                        info!("SCP password prompt detected");
                        self.waiting_password = true;
                        self.password_buf.clear();
                        self.core.status_msg = "SCP requires password".to_string();
                        self.core.status_color = Color::Yellow;
                        self.core.needs_redraw = true;
                    }
                    return;
                }

                let pct = scrape_transfer_progress(&lines);

                if let Some(ref mut t) = self.core.last_transfer
                    && let Some(ref pct) = pct
                    && *pct != t.progress
                {
                    t.progress = pct.clone();
                    self.core.needs_redraw = true;
                }

                if exited {
                    info!("SCP transfer done (process exited)");
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
                    self.scp_pty = None;
                    self.core.local_entries = read_local_dir(&self.core.local_path);
                    info!("SCP transfer complete");
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.core.prompt_stable = 0;
                    self.send_ls();
                    self.ssh_state = SshBrowserState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            return;
        }

        // --- SSH connection password detection ---
        if self.ssh_state == SshBrowserState::Connecting && !self.waiting_password {
            let lines = self.ssh.raw_lines();
            let pw_count = lines
                .iter()
                .filter(|l| l.trim().to_lowercase().ends_with("password:"))
                .count();
            if pw_count > self.password_prompts_seen {
                self.password_prompts_seen = pw_count;
                if let Some(ref pw) = self.saved_password {
                    if pw_count > 1 {
                        warn!("SSH password rejected, re-prompting");
                        self.saved_password = None;
                        self.password_buf.clear();
                        self.waiting_password = true;
                        self.core.status_msg = "Wrong password, try again".to_string();
                        self.core.status_color = Color::Red;
                    } else {
                        debug!("SSH auto-sending saved password");
                        self.ssh.send_str(&format!("{}\r\n", pw));
                        self.core.status_msg = "Authenticating...".to_string();
                        self.core.status_color = Color::Yellow;
                    }
                } else {
                    info!("SSH password prompt detected");
                    self.waiting_password = true;
                    self.password_buf.clear();
                    self.core.status_msg = "SSH requires password".to_string();
                    self.core.status_color = Color::Yellow;
                }
                self.core.needs_redraw = true;
                return;
            }
        }

        if self.ssh_state == SshBrowserState::Connecting && self.waiting_password {
            return;
        }

        // --- SSH prompt stability ---
        let cur_len = self.ssh.raw_len();
        if cur_len != self.core.prev_raw_len {
            self.core.prompt_stable = 0;
            self.core.prev_raw_len = cur_len;
        } else {
            let has_prompt = match self.ssh_state {
                SshBrowserState::Connecting => self.shell_prompt_detected(),
                _ => self.prompt_ends_with_sshmux(),
            };
            if has_prompt {
                self.core.prompt_stable = self.core.prompt_stable.saturating_add(1);
            } else {
                self.core.prompt_stable = 0;
            }
        }

        const STABLE_NEEDED: u8 = 2;
        let prompt_ready = self.core.prompt_stable >= STABLE_NEEDED;

        if matches!(
            self.ssh_state,
            SshBrowserState::Connecting | SshBrowserState::SettingPrompt
        ) {
            self.core.raw_snapshot = self.ssh.raw_lines();
        }

        match self.ssh_state {
            SshBrowserState::Connecting => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.ssh
                        .send_str("PS1='SSHMUX> '; PS2=''; unset PROMPT_COMMAND 2>/dev/null\r\n");
                    self.ssh_state = SshBrowserState::SettingPrompt;
                    self.password_prompts_seen = 0;
                    self.core.status_msg = format!("Setting prompt on {}", self.core.host);
                    self.core.status_color = Color::Yellow;
                    info!("SSH shell detected on {}, setting PS1", self.core.host);
                    self.core.needs_redraw = true;
                }
            }
            SshBrowserState::SettingPrompt => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.core.cmd_start = Some(Instant::now());
                    self.ssh.send_str("pwd\r\n");
                    self.ssh_state = SshBrowserState::WaitingPwd;
                    self.core.status_msg = format!("Connected to {}", self.core.host);
                    self.core.status_color = Color::Green;
                    info!("SSH prompt set on {}", self.core.host);
                    self.core.needs_redraw = true;
                }
            }
            SshBrowserState::WaitingPwd => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.ssh.raw_lines();
                    self.core.remote_path =
                        parse_pwd(&lines).unwrap_or_else(|| self.core.remote_path.clone());
                    debug!("SSH pwd => {}, sending ls -la", self.core.remote_path);
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.send_ls();
                    self.ssh_state = SshBrowserState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            SshBrowserState::WaitingLs => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.ssh.raw_lines();
                    if let Some(p) = parse_pwd(&lines) {
                        self.core.remote_path = p;
                    }
                    let parsed = parse_ls(&lines);
                    debug!("SSH ls done: {} entries", parsed.len());
                    self.core.remote_entries = parsed;
                    self.core.raw_snapshot.clear();
                    let max = self.core.remote_entries.len().saturating_sub(1);
                    let cur = self.core.remote_sel.selected().unwrap_or(0);
                    self.core.remote_sel.select(Some(cur.min(max)));
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.ssh_state = SshBrowserState::Idle;
                    self.core.stop_timer();
                    if self.core.status_color == Color::Yellow {
                        self.core.status_color = Color::Green;
                    }
                    self.core.needs_redraw = true;
                    // Chain next queued drop-upload if any
                    if !self.core.pending_uploads.is_empty() {
                        self.upload_pending_paths();
                    }
                }
            }
            SshBrowserState::WaitingDelete => {
                if prompt_ready {
                    self.core.prompt_stable = 0;
                    let lines = self.ssh.raw_lines();
                    for (i, line) in lines.iter().enumerate() {
                        debug!("SSH delete line[{}]: {:?}", i, line);
                    }
                    let output_lines = if lines.len() > 1 {
                        &lines[1..]
                    } else {
                        &lines[..]
                    };
                    let has_error = output_lines.iter().any(|l| {
                        let t = l.to_lowercase();
                        t.contains("cannot remove")
                            || t.contains("no such file")
                            || t.contains("permission denied")
                            || t.contains("not empty")
                            || t.contains("directory not empty")
                    });
                    if let Some(name) = self.core.pending_delete_name.take() {
                        if has_error {
                            warn!("SSH delete failed: {}", name);
                            self.core.status_msg = format!("Delete failed: {}", name);
                            self.core.status_color = Color::Red;
                        } else {
                            self.core.status_msg = format!("Deleted remote: {}", name);
                            self.core.status_color = Color::Green;
                        }
                    }
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.send_ls();
                    self.ssh_state = SshBrowserState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            SshBrowserState::Transferring => {} // handled above
            SshBrowserState::Idle => {}
        }
    }

    fn prompt_ends_with_sshmux(&self) -> bool {
        let Ok(rb) = self.ssh.raw_output.lock() else {
            return false;
        };
        let start = rb.len().saturating_sub(64);
        let tail = strip_ansi(&rb[start..]);
        tail.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| {
                let t = l.trim();
                t == "SSHMUX>" || t == "SSHMUX> "
            })
            .unwrap_or(false)
    }

    fn shell_prompt_detected(&self) -> bool {
        let Ok(rb) = self.ssh.raw_output.lock() else {
            return false;
        };
        if rb.is_empty() {
            return false;
        }
        let start = rb.len().saturating_sub(128);
        let tail = strip_ansi(&rb[start..]);
        tail.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| {
                let t = l.trim_end();
                t.ends_with('$') || t.ends_with('#') || t.ends_with('%')
            })
            .unwrap_or(false)
    }

    // ---- navigation (delegates to core for local, handles remote) ----------

    pub fn enter(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => self.core.local_enter(),
            BrowserFocus::Remote => {
                if self.ssh_state != SshBrowserState::Idle {
                    return;
                }
                if let Some(i) = self.core.remote_sel.selected()
                    && let Some(entry) = self.core.remote_entries.get(i).cloned()
                {
                    if entry.is_dir {
                        self.core.apply_cd(&entry.name);
                        self.core.status_msg = format!("Remote: {}", self.core.remote_path);
                        self.core.status_color = Color::Yellow;
                        self.ssh.drain_raw();
                        self.core.prev_raw_len = 0;
                        self.core.prompt_stable = 0;
                        self.send_ls();
                        self.ssh_state = SshBrowserState::WaitingLs;
                        debug!("SSH ls {}", self.core.remote_path);
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
                if self.ssh_state != SshBrowserState::Idle {
                    return;
                }
                self.core.apply_cd("..");
                self.core.status_msg = format!("Remote: {}", self.core.remote_path);
                self.core.status_color = Color::Yellow;
                self.ssh.drain_raw();
                self.core.prev_raw_len = 0;
                self.core.prompt_stable = 0;
                self.send_ls();
                self.ssh_state = SshBrowserState::WaitingLs;
                debug!("SSH ls {}", self.core.remote_path);
            }
        }
    }

    fn send_ls(&mut self) {
        let cmd = format!(
            "ls -la --quoting-style=literal {}\r\n",
            shell_quote(&self.core.remote_path)
        );
        self.core.cmd_start = Some(Instant::now());
        self.ssh.send_str(&cmd);
    }

    // ---- transfers ---------------------------------------------------------

    pub fn download(&mut self) {
        if self.ssh_state != SshBrowserState::Idle {
            return;
        }
        if let Some(i) = self.core.remote_sel.selected() {
            let Some(entry) = self.core.remote_entries.get(i).cloned() else {
                return;
            };
            let local_dest = self.core.local_path.to_string_lossy().replace('\\', "/");
            let remote_file = format!(
                "{}/{}",
                self.core.remote_path.trim_end_matches('/'),
                entry.name
            );

            let mut cmd = CommandBuilder::new("scp");
            cmd.arg("-O");
            if entry.is_dir {
                cmd.arg("-r");
            }
            cmd.arg(format!("{}:{}", self.core.host, remote_file));
            cmd.arg(&*local_dest);
            cmd.env("TERM", "xterm");
            info!(
                "SCP download: {}:{} -> {}",
                self.core.host, remote_file, local_dest
            );

            match EmbeddedTerminal::new(24, 80, cmd, true) {
                Ok(term) => {
                    self.scp_pty = Some(term);
                    self.core.last_transfer = Some(TransferStatus {
                        filename: entry.name.clone(),
                        direction: TransferDirection::Download,
                        is_dir: entry.is_dir,
                        done: false,
                        progress: "0%".to_string(),
                        file_count: 0,
                    });
                    self.core.status_msg = format!("Downloading {}...", entry.name);
                    self.core.status_color = Color::Yellow;
                    self.ssh_state = SshBrowserState::Transferring;
                    self.password_prompts_seen = 0;
                    self.waiting_password = false;
                    info!("SCP get {} started", entry.name);
                }
                Err(e) => {
                    error!("SCP spawn failed: {}", e);
                    self.core.status_msg = format!("SCP error: {}", e);
                    self.core.status_color = Color::Red;
                }
            }
            self.core.needs_redraw = true;
        }
    }

    pub fn upload(&mut self) {
        if self.ssh_state != SshBrowserState::Idle {
            return;
        }
        if let Some(i) = self.core.local_sel.selected() {
            let Some(entry) = self.core.local_entries.get(i).cloned() else {
                return;
            };
            let local_path = self.core.local_path.join(&entry.name);
            let local_str = local_path.to_string_lossy().replace('\\', "/");

            let mut cmd = CommandBuilder::new("scp");
            cmd.arg("-O");
            if entry.is_dir {
                cmd.arg("-r");
            }
            cmd.arg(&*local_str);
            cmd.arg(format!(
                "{}:{}",
                self.core.host,
                self.core.remote_path.trim_end_matches('/')
            ));
            cmd.env("TERM", "xterm");
            info!(
                "SCP upload: {} -> {}:{}",
                local_str,
                self.core.host,
                self.core.remote_path.trim_end_matches('/')
            );

            match EmbeddedTerminal::new(24, 80, cmd, true) {
                Ok(term) => {
                    self.scp_pty = Some(term);
                    self.core.last_transfer = Some(TransferStatus {
                        filename: entry.name.clone(),
                        direction: TransferDirection::Upload,
                        is_dir: entry.is_dir,
                        done: false,
                        progress: "0%".to_string(),
                        file_count: 0,
                    });
                    self.core.status_msg = format!("Uploading {}...", entry.name);
                    self.core.status_color = Color::Yellow;
                    self.ssh_state = SshBrowserState::Transferring;
                    self.password_prompts_seen = 0;
                    self.waiting_password = false;
                    info!("SCP put {} started", local_str);
                }
                Err(e) => {
                    error!("SCP spawn failed: {}", e);
                    self.core.status_msg = format!("SCP error: {}", e);
                    self.core.status_color = Color::Red;
                }
            }
            self.core.needs_redraw = true;
        }
    }

    // ---- drop upload -------------------------------------------------------

    pub fn upload_pending_paths(&mut self) {
        if self.ssh_state != SshBrowserState::Idle {
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

        let mut cmd = CommandBuilder::new("scp");
        cmd.arg("-O");
        if is_dir {
            cmd.arg("-r");
        }
        cmd.arg(&*local_str);
        cmd.arg(format!(
            "{}:{}",
            self.core.host,
            self.core.remote_path.trim_end_matches('/')
        ));
        cmd.env("TERM", "xterm");
        info!(
            "SCP upload (drop): {} -> {}:{}",
            local_str,
            self.core.host,
            self.core.remote_path.trim_end_matches('/')
        );

        match EmbeddedTerminal::new(24, 80, cmd, true) {
            Ok(term) => {
                self.scp_pty = Some(term);
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
                self.ssh_state = SshBrowserState::Transferring;
                self.password_prompts_seen = 0;
                self.waiting_password = false;
                info!("SCP put (drop) {} started", name);
            }
            Err(e) => {
                error!("SCP spawn failed: {}", e);
                self.core.status_msg = format!("SCP error: {}", e);
                self.core.status_color = Color::Red;
            }
        }
        self.core.needs_redraw = true;
    }

    // ---- password input for SCP auth ----------------------------------------

    pub fn password_char(&mut self, c: char) {
        self.password_buf.push(c);
        self.core.needs_redraw = true;
    }

    pub fn password_backspace(&mut self) {
        self.password_buf.pop();
        self.core.needs_redraw = true;
    }

    pub fn submit_password(&mut self) {
        let pw = self.password_buf.clone();
        if self.ssh_state == SshBrowserState::Connecting {
            debug!("SSH sending user password ({} chars)", pw.len());
            self.ssh.send_str(&format!("{}\r\n", pw));
            self.core.status_msg = "Authenticating...".to_string();
        } else if let Some(ref mut scp) = self.scp_pty {
            debug!("SCP sending user password ({} chars)", pw.len());
            scp.send_str(&format!("{}\r\n", pw));
            if let Some(ref t) = self.core.last_transfer {
                let verb = match t.direction {
                    TransferDirection::Download => "Downloading",
                    TransferDirection::Upload => "Uploading",
                };
                self.core.status_msg = format!("{}...", verb);
            }
        }
        self.saved_password = Some(pw);
        self.password_buf.clear();
        self.waiting_password = false;
        self.core.status_color = Color::Yellow;
        self.core.needs_redraw = true;
    }

    // ---- delete ------------------------------------------------------------

    pub fn delete_focused(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => self.core.local_delete_focused(),
            BrowserFocus::Remote => {
                if let Some(i) = self.core.remote_sel.selected() {
                    let Some(entry) = self.core.remote_entries.get(i).cloned() else {
                        return;
                    };
                    if entry.name == ".." || self.ssh_state != SshBrowserState::Idle {
                        return;
                    }
                    let full_path = format!(
                        "{}/{}",
                        self.core.remote_path.trim_end_matches('/'),
                        entry.name
                    );
                    let kind = if entry.is_dir { "dir" } else { "file" };
                    self.core.confirm_delete = Some(format!("remote:{}:{}", kind, full_path));
                    self.core.needs_redraw = true;
                }
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
            let full_path = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
            info!("SSH remote delete: {} is_dir={}", full_path, is_dir);
            let cmd = if is_dir {
                format!("rm -rf -- {}\r\n", shell_quote(full_path))
            } else {
                format!("rm -- {}\r\n", shell_quote(full_path))
            };
            self.ssh.send_str(&cmd);
            self.ssh_state = SshBrowserState::WaitingDelete;
            self.core.status_msg = format!("Deleting {}...", full_path);
            self.core.status_color = Color::Yellow;
            self.core.pending_delete_name = Some(full_path.to_string());
            self.ssh.drain_raw();
            self.core.prev_raw_len = 0;
            self.core.prompt_stable = 0;
            self.core.needs_redraw = true;
        }
    }

    // ---- render ------------------------------------------------------------

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool, leaf_count: usize) {
        let title = format!(" scp: {} ", self.core.host);
        let status_area = self
            .core
            .render_panels(area, buf, is_focus, leaf_count, &title);
        if !self.core.render_confirm_delete(status_area, buf) {
            if self.waiting_password {
                self.render_password_status(status_area, buf);
            } else {
                let (label, color) = self.state_label();
                let progress = self.progress_suffix();
                self.core
                    .render_normal_status(status_area, buf, label, color, &progress);
            }
        }
        self.core.render_upload_confirm(area, buf);
    }

    fn render_password_status(&self, area: Rect, buf: &mut Buffer) {
        let stars = "*".repeat(self.password_buf.len());
        let text = format!("  Password: {}\u{2588}", stars);
        let pad = (area.width as usize).saturating_sub(text.chars().count());
        let msg = format!("{}{}", text, " ".repeat(pad));
        let span = Span::styled(
            msg,
            Style::default()
                .fg(Color::White)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_span(area.x, area.y, &span, area.width);
    }

    fn state_label(&self) -> (&str, Color) {
        match self.ssh_state {
            SshBrowserState::Connecting | SshBrowserState::SettingPrompt => {
                ("connecting", Color::Yellow)
            }
            SshBrowserState::WaitingPwd | SshBrowserState::WaitingLs => ("loading", Color::Yellow),
            SshBrowserState::Idle => ("idle", self.core.status_color),
            SshBrowserState::WaitingDelete => ("deleting", Color::Yellow),
            SshBrowserState::Transferring => ("transfer", Color::Green),
        }
    }

    fn progress_suffix(&self) -> String {
        if let Some(ref t) = self.core.last_transfer {
            if !t.done {
                format!(" {}", t.progress)
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }
}
