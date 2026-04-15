use std::time::Instant;

use anyhow::Result;
use crossterm::event::KeyCode;
use log::{debug, error, info, warn};
use portable_pty::CommandBuilder;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};

use super::common::{
    Browser, BrowserCore, BrowserFocus, COMMAND_TIMEOUT_SECS, DeleteLocation, PROMPT_TAIL_BYTES,
    TransferDirection, TransferStatus,
};
use super::parse::{
    parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress, shell_quote, strip_ansi,
};
use crate::keybindings::BrowserBindings;
use crate::terminal::{EmbeddedTerminal, PtyChannel};

#[cfg(test)]
use crate::terminal::{MockPty, MockPtyHandle};

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

/// Larger tail for shell prompt detection (PS1 can be longer than sftp>).
const PROMPT_TAIL_BYTES_LONG: usize = 128;

// ---------------------------------------------------------------------------
// SshBrowser
// ---------------------------------------------------------------------------

pub struct SshBrowser {
    pub core: BrowserCore,
    pub ssh: Box<dyn PtyChannel>,
    pub scp_pty: Option<Box<dyn PtyChannel>>,
    pub ssh_state: SshBrowserState,

    pub saved_password: Option<String>,
    pub password_buf: String,
    pub waiting_password: bool,
    pub password_prompts_seen: usize,
}

fn count_password_prompts(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| l.trim().to_lowercase().ends_with("password:"))
        .count()
}

impl SshBrowser {
    pub fn new(host: &str) -> Result<Self> {
        let ssh = EmbeddedTerminal::ssh_shell(host)?;
        Ok(SshBrowser {
            core: BrowserCore::new(host),
            ssh: Box::new(ssh),
            scp_pty: None,
            ssh_state: SshBrowserState::Connecting,
            saved_password: None,
            password_buf: String::new(),
            waiting_password: false,
            password_prompts_seen: 0,
        })
    }

    #[cfg(test)]
    pub fn with_mock() -> (Self, MockPtyHandle) {
        let (mock, handle) = MockPty::new();
        let browser = SshBrowser {
            core: BrowserCore::new("test-host"),
            ssh: Box::new(mock),
            scp_pty: None,
            ssh_state: SshBrowserState::Connecting,
            saved_password: None,
            password_buf: String::new(),
            waiting_password: false,
            password_prompts_seen: 0,
        };
        (browser, handle)
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
                let pw_count = count_password_prompts(&lines);
                if pw_count > self.password_prompts_seen {
                    self.password_prompts_seen = pw_count;
                    if let Some(ref pw) = self.saved_password {
                        if pw_count > 1 {
                            warn!("SCP password rejected, re-prompting");
                            self.scp_pty = None;
                            self.saved_password = None;
                            self.password_buf.clear();
                            self.password_prompts_seen = 0;
                            self.waiting_password = true;
                            self.core.status_msg = "Wrong password, try again".to_string();
                            self.core.status_color = Color::Red;
                            self.core.needs_redraw = true;
                        } else {
                            debug!("SCP auto-sending saved password");
                            scp.send_str(&format!("{}\r\n", pw));
                            if let Some(ref t) = self.core.transfer.last {
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

                if let Some(ref mut t) = self.core.transfer.last
                    && let Some(pct) = pct
                    && pct != t.progress
                {
                    t.progress = pct;
                    self.core.needs_redraw = true;
                }

                if exited {
                    info!("SCP transfer done (process exited)");
                    let completion_msg = self.core.transfer.last.as_mut().map(|t| {
                        t.done = true;
                        t.progress = 100;
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
                    self.core.transfer.batch_done += 1;
                    self.core.local.entries = read_local_dir(&self.core.local.path);
                    info!("SCP transfer complete");
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.core.prompt_stable = 0;
                    // Batch complete — reset counters before the final ls refresh.
                    if self.core.transfer.pending.is_empty() {
                        self.core.transfer.start = None;
                        self.core.transfer.batch_done = 0;
                        self.core.transfer.batch_total = 0;
                    }
                    self.send_ls();
                    self.ssh_state = SshBrowserState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            } else {
                // scp_pty is None while state is Transferring — recover.
                warn!("SCP transfer state with no process, recovering");
                self.core.drop_confirm = None;
                self.core.transfer.pending.clear();
                self.core.delete.pending.clear();
                self.core.transfer.batch_done = 0;
                self.core.transfer.batch_total = 0;
                self.core.transfer.start = None;
                self.ssh_state = SshBrowserState::Idle;
                self.core.status_msg = "Transfer failed".to_string();
                self.core.status_color = Color::Red;
                self.core.needs_redraw = true;
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
                        self.password_prompts_seen = 0;
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

        // Command timeout: waiting states that should not stall indefinitely.
        if matches!(
            self.ssh_state,
            SshBrowserState::WaitingPwd
                | SshBrowserState::WaitingLs
                | SshBrowserState::WaitingDelete
        ) && let Some(start) = self.core.cmd_start
            && start.elapsed().as_secs() >= COMMAND_TIMEOUT_SECS
        {
            warn!("SCP command timed out in state {:?}", self.ssh_state);
            self.core.cmd_start = None;
            self.ssh_state = SshBrowserState::Idle;
            self.core.status_msg = "Command timed out".to_string();
            self.core.status_color = Color::Red;
            self.core.needs_redraw = true;
            return;
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
                    self.core.remote.path =
                        parse_pwd(&lines).unwrap_or_else(|| self.core.remote.path.clone());
                    debug!("SSH pwd => {}, sending ls -la", self.core.remote.path);
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
                        self.core.remote.path = p;
                    }
                    let parsed = parse_ls(&lines);
                    debug!("SSH ls done: {} entries", parsed.len());
                    self.core.remote.entries = parsed;
                    self.core.raw_snapshot.clear();
                    let max = self.core.remote.entries.len().saturating_sub(1);
                    let cur = self.core.remote.sel.selected().unwrap_or(0);
                    self.core.remote.sel.select(Some(cur.min(max)));
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    self.ssh_state = SshBrowserState::Idle;
                    self.core.stop_timer();
                    if self.core.status_color == Color::Yellow {
                        self.core.status_color = Color::Green;
                    }
                    self.core.needs_redraw = true;
                    // Chain next queued transfer if any
                    if !self.core.transfer.pending.is_empty() {
                        match self.core.last_transfer_direction() {
                            TransferDirection::Upload => self.upload(),
                            TransferDirection::Download => self.download(),
                        }
                    } else if self.core.pop_pending_delete() {
                        self.confirm_delete_yes();
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
                    if let Some(name) = self.core.delete.pending_name.take() {
                        if has_error {
                            warn!("SSH delete failed: {}", name);
                            self.core.status_msg = format!("Delete failed: {}", name);
                            self.core.status_color = Color::Red;
                            self.core.delete.pending.clear();
                        } else {
                            self.core.status_msg = format!("Deleted remote: {}", name);
                            self.core.status_color = Color::Green;
                        }
                    }
                    self.ssh.drain_raw();
                    self.core.prev_raw_len = 0;
                    // Skip the ls round-trip when more deletes are queued —
                    // chain directly to the next delete instead.
                    if !has_error && self.core.pop_pending_delete() {
                        self.confirm_delete_yes();
                    } else {
                        self.send_ls();
                        self.ssh_state = SshBrowserState::WaitingLs;
                    }
                    self.core.needs_redraw = true;
                }
            }
            SshBrowserState::Transferring => {} // handled above
            SshBrowserState::Idle => {}
        }
    }

    fn prompt_ends_with_sshmux(&self) -> bool {
        let tail_bytes = self.ssh.raw_tail(PROMPT_TAIL_BYTES);
        if tail_bytes.is_empty() {
            return false;
        }
        let tail = strip_ansi(&tail_bytes);
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
        let tail_bytes = self.ssh.raw_tail(PROMPT_TAIL_BYTES_LONG);
        if tail_bytes.is_empty() {
            return false;
        }
        let tail = strip_ansi(&tail_bytes);
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
                if let Some(i) = self.core.remote.sel.selected()
                    && let Some(entry) = self.core.remote.entries.get(i).cloned()
                {
                    if entry.is_dir {
                        self.core.apply_cd(&entry.name);
                        self.core.status_msg = format!("Remote: {}", self.core.remote.path);
                        self.core.status_color = Color::Yellow;
                        self.ssh.drain_raw();
                        self.core.prev_raw_len = 0;
                        self.core.prompt_stable = 0;
                        self.send_ls();
                        self.ssh_state = SshBrowserState::WaitingLs;
                        debug!("SSH ls {}", self.core.remote.path);
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
                self.core.status_msg = format!("Remote: {}", self.core.remote.path);
                self.core.status_color = Color::Yellow;
                self.ssh.drain_raw();
                self.core.prev_raw_len = 0;
                self.core.prompt_stable = 0;
                self.send_ls();
                self.ssh_state = SshBrowserState::WaitingLs;
                debug!("SSH ls {}", self.core.remote.path);
            }
        }
    }

    fn send_ls(&mut self) {
        let cmd = format!("ls -la {}\r\n", shell_quote(&self.core.remote.path));
        self.core.cmd_start = Some(Instant::now());
        self.ssh.send_str(&cmd);
    }

    // ---- transfers ---------------------------------------------------------

    pub fn download(&mut self) {
        if self.ssh_state != SshBrowserState::Idle {
            return;
        }
        let (remote_file, name, is_dir) = if let Some(t) = self.core.pop_pending() {
            (t.path, t.name, t.is_dir)
        } else if let Some(i) = self.core.remote.sel.selected() {
            let Some(entry) = self.core.remote.entries.get(i) else {
                return;
            };
            if entry.name == ".." {
                return;
            }
            let path = format!(
                "{}/{}",
                self.core.remote.path.trim_end_matches('/'),
                entry.name
            );
            (path, entry.name.clone(), entry.is_dir)
        } else {
            return;
        };
        let local_dest = self.core.local.path.to_string_lossy().replace('\\', "/");
        let mut cmd = CommandBuilder::new("scp");
        cmd.arg("-O");
        if is_dir {
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
                self.scp_pty = Some(Box::new(term));
                if self.core.transfer.batch_done == 0 {
                    self.core.transfer.batch_total = self.core.transfer.pending.len() + 1;
                }
                self.core.transfer.start = Some(Instant::now());
                self.core.transfer.last = Some(TransferStatus {
                    filename: name.clone(),
                    direction: TransferDirection::Download,
                    is_dir,
                    done: false,
                    progress: 0,
                    file_count: 0,
                });
                self.core.status_msg = format!("Downloading {}...", name);
                self.core.status_color = Color::Yellow;
                self.ssh_state = SshBrowserState::Transferring;
                self.password_prompts_seen = 0;
                self.waiting_password = false;
                info!("SCP get {} started", name);
            }
            Err(e) => {
                error!("SCP spawn failed: {}", e);
                self.core.status_msg = format!("SCP error: {}", e);
                self.core.status_color = Color::Red;
            }
        }
        self.core.needs_redraw = true;
    }

    pub fn upload(&mut self) {
        if self.ssh_state != SshBrowserState::Idle {
            return;
        }
        let (local_str, name, is_dir) = if let Some(t) = self.core.pop_pending() {
            (t.path, t.name, t.is_dir)
        } else if let Some(i) = self.core.local.sel.selected() {
            let Some(entry) = self.core.local.entries.get(i) else {
                return;
            };
            if entry.name == ".." {
                return;
            }
            let path = self
                .core
                .local
                .path
                .join(&entry.name)
                .to_string_lossy()
                .replace('\\', "/");
            (path, entry.name.clone(), entry.is_dir)
        } else {
            return;
        };

        let mut cmd = CommandBuilder::new("scp");
        cmd.arg("-O");
        if is_dir {
            cmd.arg("-r");
        }
        cmd.arg(&*local_str);
        cmd.arg(format!(
            "{}:{}",
            self.core.host,
            self.core.remote.path.trim_end_matches('/')
        ));
        cmd.env("TERM", "xterm");
        info!(
            "SCP upload: {} -> {}:{}",
            local_str,
            self.core.host,
            self.core.remote.path.trim_end_matches('/')
        );

        match EmbeddedTerminal::new(24, 80, cmd, true) {
            Ok(term) => {
                self.scp_pty = Some(Box::new(term));
                if self.core.transfer.batch_done == 0 {
                    self.core.transfer.batch_total = self.core.transfer.pending.len() + 1;
                }
                self.core.transfer.start = Some(Instant::now());
                self.core.transfer.last = Some(TransferStatus {
                    filename: name.clone(),
                    direction: TransferDirection::Upload,
                    is_dir,
                    done: false,
                    progress: 0,
                    file_count: 0,
                });
                self.core.status_msg = format!("Uploading {}...", name);
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
            if let Some(ref t) = self.core.transfer.last {
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
                if self.ssh_state != SshBrowserState::Idle {
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
        if let Some(target) = self.core.delete.confirm.take() {
            if target.location != DeleteLocation::Remote {
                return;
            }
            info!(
                "SSH remote delete: {} is_dir={}",
                target.path,
                target.is_dir()
            );
            let cmd = if target.is_dir() {
                format!("rm -rf -- {}\r\n", shell_quote(&target.path))
            } else {
                format!("rm -- {}\r\n", shell_quote(&target.path))
            };
            self.ssh.send_str(&cmd);
            self.ssh_state = SshBrowserState::WaitingDelete;
            self.core.status_msg = format!("Deleting {}...", target.path);
            self.core.status_color = Color::Yellow;
            self.core.delete.pending_name = Some(target.path);
            self.ssh.drain_raw();
            self.core.prev_raw_len = 0;
            self.core.prompt_stable = 0;
            self.core.needs_redraw = true;
        }
    }

    // ---- render ------------------------------------------------------------

    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        is_focus: bool,
        leaf_count: usize,
        bindings: &BrowserBindings,
    ) {
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
                    .render_normal_status(status_area, buf, label, color, &progress, bindings);
            }
        }
        self.core.render_upload_confirm(area, buf);
        self.core
            .render_transfer_progress(area, buf, self.core.transfer.start.is_some());
        self.core.render_drag_arrow(area, buf, leaf_count);
        self.core.render_drag_ghost(buf);
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
        if let Some(ref t) = self.core.transfer.last {
            if !t.done {
                format!(" {}%", t.progress)
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }
}

impl Browser for SshBrowser {
    fn core(&self) -> &BrowserCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut BrowserCore {
        &mut self.core
    }
    fn upload(&mut self) {
        self.upload();
    }
    fn download(&mut self) {
        self.download();
    }
    fn enter(&mut self) {
        self.enter();
    }
    fn go_up(&mut self) {
        self.go_up();
    }
    fn delete_focused(&mut self) {
        self.delete_focused();
    }
    fn confirm_delete_yes(&mut self) {
        self.confirm_delete_yes();
    }
    fn is_connecting(&self) -> bool {
        matches!(
            self.ssh_state,
            SshBrowserState::Connecting | SshBrowserState::SettingPrompt
        )
    }
    fn send_connect_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.ssh.send_char(c),
            KeyCode::Enter => self.ssh.send_str("\r\n"),
            KeyCode::Backspace => self.ssh.send_str("\x7f"),
            _ => {}
        }
    }
    fn process_exited(&self) -> bool {
        self.ssh.process_exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::common::{DeleteKind, DeleteTarget};
    use crate::browser::parse::FsEntry;

    fn tick_until_stable(browser: &mut SshBrowser) {
        browser.tick();
        browser.tick();
        browser.tick();
    }

    fn make_ssh() -> (SshBrowser, MockPtyHandle) {
        SshBrowser::with_mock()
    }

    /// Create a mock SCP pty and return its handle for feeding data.
    fn attach_scp_pty(browser: &mut SshBrowser) -> MockPtyHandle {
        let (mock, handle) = MockPty::new();
        browser.scp_pty = Some(Box::new(mock));
        handle
    }

    // ---- count_password_prompts helper ----

    #[test]
    fn count_password_prompts_basic() {
        let lines = vec![
            "user@host's password:".to_string(),
            "some output".to_string(),
        ];
        assert_eq!(count_password_prompts(&lines), 1);
    }

    #[test]
    fn count_password_prompts_multiple() {
        let lines = vec![
            "user@host's password:".to_string(),
            "Permission denied, please try again.".to_string(),
            "user@host's password:".to_string(),
        ];
        assert_eq!(count_password_prompts(&lines), 2);
    }

    #[test]
    fn count_password_prompts_none() {
        let lines = vec!["Welcome to server".to_string()];
        assert_eq!(count_password_prompts(&lines), 0);
    }

    // ---- Connecting state ----

    #[test]
    fn connecting_detects_shell_prompt_and_sets_ps1() {
        let (mut sb, h) = make_ssh();
        assert_eq!(sb.ssh_state, SshBrowserState::Connecting);

        h.feed(b"Last login: Mon Jan 1 12:00\nuser@host:~$ ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::SettingPrompt);
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.contains("PS1=")),
            "should send PS1 command, got: {:?}",
            sent
        );
    }

    #[test]
    fn connecting_stays_without_shell_prompt() {
        let (mut sb, h) = make_ssh();
        h.feed(b"SSH negotiating...\n");
        tick_until_stable(&mut sb);
        assert_eq!(sb.ssh_state, SshBrowserState::Connecting);
    }

    #[test]
    fn connecting_detects_hash_prompt() {
        let (mut sb, h) = make_ssh();
        h.feed(b"root@host:~# ");
        tick_until_stable(&mut sb);
        assert_eq!(sb.ssh_state, SshBrowserState::SettingPrompt);
    }

    #[test]
    fn connecting_detects_percent_prompt() {
        let (mut sb, h) = make_ssh();
        h.feed(b"user@host% ");
        tick_until_stable(&mut sb);
        assert_eq!(sb.ssh_state, SshBrowserState::SettingPrompt);
    }

    // ---- SSH password detection ----

    #[test]
    fn connecting_detects_password_prompt() {
        let (mut sb, h) = make_ssh();
        h.feed(b"user@host's password:");
        sb.tick();

        assert!(sb.waiting_password);
        assert_eq!(sb.core.status_msg, "SSH requires password");
        assert!(sb.password_buf.is_empty());
    }

    #[test]
    fn connecting_auto_sends_saved_password() {
        let (mut sb, h) = make_ssh();
        sb.saved_password = Some("secret".to_string());
        h.feed(b"user@host's password:");
        sb.tick();

        assert!(!sb.waiting_password, "should auto-send, not prompt");
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.contains("secret")),
            "should send password, got: {:?}",
            sent
        );
        assert!(sb.core.status_msg.contains("Authenticating"));
    }

    #[test]
    fn connecting_rejects_saved_password_on_second_prompt() {
        let (mut sb, h) = make_ssh();
        sb.saved_password = Some("wrong".to_string());
        sb.password_prompts_seen = 1;
        h.feed(b"user@host's password:\nPermission denied\nuser@host's password:");
        sb.tick();

        assert!(sb.waiting_password, "should re-prompt after rejection");
        assert!(
            sb.saved_password.is_none(),
            "saved password should be cleared"
        );
        assert!(sb.core.status_msg.contains("Wrong password"));
    }

    #[test]
    fn connecting_blocks_while_waiting_password() {
        let (mut sb, h) = make_ssh();
        sb.waiting_password = true;
        h.feed(b"user@host:~$ ");
        tick_until_stable(&mut sb);
        assert_eq!(
            sb.ssh_state,
            SshBrowserState::Connecting,
            "should not progress while waiting for password"
        );
    }

    // ---- Password input methods ----

    #[test]
    fn password_char_and_backspace() {
        let (mut sb, _h) = make_ssh();
        sb.password_char('a');
        sb.password_char('b');
        sb.password_char('c');
        assert_eq!(sb.password_buf, "abc");
        sb.password_backspace();
        assert_eq!(sb.password_buf, "ab");
    }

    #[test]
    fn submit_password_in_connecting_sends_to_ssh() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Connecting;
        sb.waiting_password = true;
        sb.password_buf = "mypass".to_string();
        h.clear_sent();

        sb.submit_password();

        assert!(!sb.waiting_password);
        assert_eq!(sb.saved_password, Some("mypass".to_string()));
        assert!(sb.password_buf.is_empty());
        let sent = h.sent();
        assert!(sent.iter().any(|s| s.contains("mypass")));
    }

    #[test]
    fn submit_password_in_transferring_sends_to_scp() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.waiting_password = true;
        sb.password_buf = "scppass".to_string();
        sb.core.transfer.last = Some(TransferStatus {
            filename: "file.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        sb.submit_password();

        assert!(!sb.waiting_password);
        assert_eq!(sb.saved_password, Some("scppass".to_string()));
        let sent = scp_h.sent();
        assert!(sent.iter().any(|s| s.contains("scppass")));
    }

    // ---- SettingPrompt state ----

    #[test]
    fn setting_prompt_transitions_to_waiting_pwd() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::SettingPrompt;

        h.feed(b"SSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingPwd);
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s == "pwd\r\n"),
            "should send pwd, got: {:?}",
            sent
        );
    }

    #[test]
    fn setting_prompt_captures_raw_snapshot() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::SettingPrompt;
        h.feed(b"setting up...\nSSHMUX> ");
        sb.tick();
        assert!(!sb.core.raw_snapshot.is_empty());
    }

    // ---- WaitingPwd state ----

    #[test]
    fn waiting_pwd_transitions_to_waiting_ls() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingPwd;

        h.feed(b"/home/testuser\nSSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert_eq!(sb.core.remote.path, "/home/testuser");
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.starts_with("ls ")),
            "should send ls, got: {:?}",
            sent
        );
    }

    // ---- WaitingLs state ----

    #[test]
    fn waiting_ls_populates_entries_and_goes_idle() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingLs;

        h.feed(b"drwxr-xr-x  2 user user  4096 Jan  1 12:00 subdir\n-rw-r--r--  1 user user  1234 Jan  1 12:00 file.txt\nSSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
        assert!(sb.core.remote.entries.len() >= 2);
        assert!(sb.core.raw_snapshot.is_empty());
    }

    #[test]
    fn waiting_ls_chains_pending_delete() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingLs;
        sb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/todelete.txt".to_string(),
        });

        h.feed(b"-rw-r--r-- 1 u u 10 Jan 1 12:00 a.txt\nSSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingDelete);
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.starts_with("rm ")),
            "should send rm command, got: {:?}",
            sent
        );
    }

    // ---- Transferring state (SCP monitoring) ----

    #[test]
    fn transferring_completes_on_scp_exit() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.core.transfer.last = Some(TransferStatus {
            filename: "test.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);
        h.clear_sent();

        scp_h.set_exited(true);
        scp_h.feed(b"test.txt  100%  1234  1.2KB/s  00:00");
        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert!(sb.scp_pty.is_none(), "scp_pty should be dropped after exit");
        assert!(sb.core.transfer.last.as_ref().unwrap().done);
        assert_eq!(sb.core.transfer.last.as_ref().unwrap().progress, 100);
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.starts_with("ls ")),
            "should send ls after transfer, got: {:?}",
            sent
        );
    }

    #[test]
    fn transferring_scrapes_scp_progress() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.core.transfer.last = Some(TransferStatus {
            filename: "big.bin".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        scp_h.feed(b"big.bin  50%  512KB  256.0KB/s  00:01");
        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::Transferring);
        assert_eq!(sb.core.transfer.last.as_ref().unwrap().progress, 50);
    }

    #[test]
    fn transferring_detects_scp_password_prompt() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.core.transfer.last = Some(TransferStatus {
            filename: "file.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        scp_h.feed(b"user@host's password:");
        sb.tick();

        assert!(sb.waiting_password);
        assert!(sb.core.status_msg.contains("SCP requires password"));
    }

    #[test]
    fn transferring_auto_sends_saved_scp_password() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.saved_password = Some("secret".to_string());
        sb.core.transfer.last = Some(TransferStatus {
            filename: "file.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        scp_h.feed(b"user@host's password:");
        sb.tick();

        assert!(!sb.waiting_password);
        let sent = scp_h.sent();
        assert!(
            sent.iter().any(|s| s.contains("secret")),
            "should auto-send password, got: {:?}",
            sent
        );
    }

    #[test]
    fn transferring_rejects_scp_password_on_second_prompt() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.saved_password = Some("wrong".to_string());
        sb.password_prompts_seen = 1;
        sb.core.transfer.last = Some(TransferStatus {
            filename: "file.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        scp_h.feed(b"user@host's password:\nPermission denied\nuser@host's password:");
        sb.tick();

        assert!(sb.waiting_password);
        assert!(sb.saved_password.is_none());
        assert!(
            sb.scp_pty.is_none(),
            "scp_pty should be dropped on rejection"
        );
    }

    #[test]
    fn transferring_recovers_when_scp_pty_missing() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.scp_pty = None; // no SCP process

        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
        assert_eq!(sb.core.status_color, Color::Red);
        assert!(sb.core.status_msg.contains("Transfer failed"));
    }

    #[test]
    fn transferring_blocks_while_waiting_password() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.waiting_password = true;
        let _scp_h = attach_scp_pty(&mut sb);

        sb.tick();

        // Should return early without processing
        assert_eq!(sb.ssh_state, SshBrowserState::Transferring);
    }

    #[test]
    fn transferring_resets_batch_on_last_transfer() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Transferring;
        sb.core.transfer.batch_done = 0;
        sb.core.transfer.batch_total = 1;
        sb.core.transfer.start = Some(Instant::now());
        sb.core.transfer.last = Some(TransferStatus {
            filename: "only.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        let scp_h = attach_scp_pty(&mut sb);

        scp_h.set_exited(true);
        sb.tick();

        assert_eq!(
            sb.core.transfer.batch_done, 0,
            "batch should reset after last transfer"
        );
        assert_eq!(sb.core.transfer.batch_total, 0);
        assert!(sb.core.transfer.start.is_none());
    }

    // ---- WaitingDelete state ----

    #[test]
    fn waiting_delete_success_sends_ls() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingDelete;
        sb.core.delete.pending_name = Some("removed.txt".to_string());
        h.clear_sent();

        h.feed(b"SSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert_eq!(sb.core.status_color, Color::Green);
        assert!(sb.core.status_msg.contains("Deleted remote: removed.txt"));
        let sent = h.sent();
        assert!(sent.iter().any(|s| s.starts_with("ls ")));
    }

    #[test]
    fn waiting_delete_failure_shows_error() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingDelete;
        sb.core.delete.pending_name = Some("protected.txt".to_string());

        // First line is the command echo, error is on subsequent lines
        h.feed(
            b"rm -- protected.txt\nrm: cannot remove 'protected.txt': Permission denied\nSSHMUX> ",
        );
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert_eq!(sb.core.status_color, Color::Red);
        assert!(sb.core.status_msg.contains("Delete failed: protected.txt"));
    }

    #[test]
    fn waiting_delete_chains_next_delete() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingDelete;
        sb.core.delete.pending_name = Some("first.txt".to_string());
        sb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/second.txt".to_string(),
        });
        h.clear_sent();

        h.feed(b"SSHMUX> ");
        tick_until_stable(&mut sb);

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingDelete);
        assert!(sb.core.delete.pending.is_empty());
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.starts_with("rm ")),
            "should chain to next delete, got: {:?}",
            sent
        );
    }

    #[test]
    fn waiting_delete_uses_rm_rf_for_dirs() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Idle;
        sb.core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::Dir,
            path: "/remote/somedir".to_string(),
        });
        h.clear_sent();

        sb.confirm_delete_yes();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingDelete);
        let sent = h.sent();
        assert!(
            sent.iter().any(|s| s.contains("rm -rf")),
            "should use rm -rf for dirs, got: {:?}",
            sent
        );
    }

    #[test]
    fn waiting_delete_uses_rm_for_files() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Idle;
        sb.core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/somefile.txt".to_string(),
        });
        h.clear_sent();

        sb.confirm_delete_yes();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingDelete);
        let sent = h.sent();
        let rm_cmds: Vec<_> = sent
            .iter()
            .filter(|s| s.starts_with("rm "))
            .cloned()
            .collect();
        assert!(!rm_cmds.is_empty());
        assert!(
            !rm_cmds.iter().any(|s| s.contains("-rf")),
            "should not use -rf for files, got: {:?}",
            rm_cmds
        );
    }

    // ---- Navigation ----

    #[test]
    fn enter_on_remote_dir_sends_ls() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Idle;
        sb.core.remote.path = "/home".to_string();
        sb.core.focus = BrowserFocus::Remote;
        sb.core.remote.entries.push(FsEntry {
            name: "subdir".to_string(),
            is_dir: true,
            size: "4096".to_string(),
            modified: "Jan 1 12:00".to_string(),
            perms: "drwxr-xr-x".to_string(),
        });
        sb.core.remote.sel.select(Some(0));
        h.clear_sent();

        sb.enter();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert_eq!(sb.core.remote.path, "/home/subdir");
        let sent = h.sent();
        assert!(sent.iter().any(|s| s.starts_with("ls ")));
    }

    #[test]
    fn go_up_remote_sends_ls() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::Idle;
        sb.core.remote.path = "/home/user".to_string();
        sb.core.focus = BrowserFocus::Remote;
        h.clear_sent();

        sb.go_up();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert_eq!(sb.core.remote.path, "/home");
        let sent = h.sent();
        assert!(sent.iter().any(|s| s.starts_with("ls ")));
    }

    #[test]
    fn enter_ignored_when_not_idle() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingLs;
        sb.core.focus = BrowserFocus::Remote;
        sb.core.remote.entries.push(FsEntry {
            name: "dir".to_string(),
            is_dir: true,
            size: "4096".to_string(),
            modified: "Jan 1 12:00".to_string(),
            perms: "drwxr-xr-x".to_string(),
        });
        sb.core.remote.sel.select(Some(0));
        h.clear_sent();

        sb.enter();

        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
        assert!(h.sent().is_empty());
    }

    // ---- send_connect_key ----

    #[test]
    fn send_connect_key_forwards_chars() {
        let (mut sb, h) = make_ssh();
        h.clear_sent();

        sb.send_connect_key(KeyCode::Char('y'));
        sb.send_connect_key(KeyCode::Enter);
        sb.send_connect_key(KeyCode::Backspace);

        let s = h.sent();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "y");
        assert_eq!(s[1], "\r\n");
        assert_eq!(s[2], "\x7f");
    }

    // ---- State label ----

    #[test]
    fn state_labels() {
        let (mut sb, _h) = make_ssh();

        let cases = [
            (SshBrowserState::Connecting, "connecting"),
            (SshBrowserState::SettingPrompt, "connecting"),
            (SshBrowserState::WaitingPwd, "loading"),
            (SshBrowserState::WaitingLs, "loading"),
            (SshBrowserState::Idle, "idle"),
            (SshBrowserState::WaitingDelete, "deleting"),
            (SshBrowserState::Transferring, "transfer"),
        ];
        for (state, expected) in cases {
            sb.ssh_state = state;
            assert_eq!(sb.state_label().0, expected, "state_label for {:?}", state);
        }
    }

    // ---- Progress suffix ----

    #[test]
    fn progress_suffix_in_progress() {
        let (mut sb, _h) = make_ssh();
        sb.core.transfer.last = Some(TransferStatus {
            filename: "f.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 42,
            file_count: 0,
        });
        assert_eq!(sb.progress_suffix(), " 42%");
    }

    #[test]
    fn progress_suffix_done() {
        let (mut sb, _h) = make_ssh();
        sb.core.transfer.last = Some(TransferStatus {
            filename: "f.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: true,
            progress: 100,
            file_count: 0,
        });
        assert_eq!(sb.progress_suffix(), "");
    }

    // ---- Idle state ----

    #[test]
    fn idle_tick_is_noop() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::Idle;
        sb.core.status_msg = "idle".to_string();

        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
        assert_eq!(sb.core.status_msg, "idle");
    }

    // ---- Prompt stability ----

    #[test]
    fn no_transition_before_stable() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::SettingPrompt;
        h.feed(b"SSHMUX> ");

        sb.tick();
        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::SettingPrompt);
    }

    #[test]
    fn changing_raw_len_resets_stability() {
        let (mut sb, h) = make_ssh();
        sb.ssh_state = SshBrowserState::SettingPrompt;
        h.feed(b"SSHMUX> ");
        sb.tick();
        sb.tick();

        h.feed(b" ");
        sb.tick();

        assert_eq!(sb.ssh_state, SshBrowserState::SettingPrompt);
    }

    // ---- Render tests ----

    fn buf_line_text(buf: &Buffer, y: u16) -> String {
        let w = buf.area().width;
        (0..w)
            .map(|x| {
                buf.cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect::<String>()
    }

    #[test]
    fn render_password_status_shows_stars() {
        let (mut sb, _h) = make_ssh();
        sb.password_buf = "abc".to_string();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);

        sb.render_password_status(area, &mut buf);

        let text = buf_line_text(&buf, 0);
        assert!(text.contains("Password:"), "should contain label: {}", text);
        assert!(text.contains("***"), "should mask with stars: {}", text);
        assert!(
            !text.contains("abc"),
            "should not reveal password: {}",
            text
        );
    }

    #[test]
    fn render_password_status_empty_password() {
        let (mut sb, _h) = make_ssh();
        sb.password_buf.clear();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);

        sb.render_password_status(area, &mut buf);

        let text = buf_line_text(&buf, 0);
        assert!(text.contains("Password:"), "should contain label: {}", text);
        assert!(
            !text.contains('*'),
            "should have no stars for empty password: {}",
            text
        );
    }

    // ---- Command timeout ----

    #[test]
    fn waiting_ls_times_out_to_idle() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingLs;
        sb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        sb.tick();
        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
        assert_eq!(sb.core.status_color, Color::Red);
        assert!(sb.core.status_msg.contains("timed out"));
        assert!(sb.core.cmd_start.is_none());
    }

    #[test]
    fn waiting_pwd_times_out_to_idle() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingPwd;
        sb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        sb.tick();
        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
        assert_eq!(sb.core.status_color, Color::Red);
    }

    #[test]
    fn waiting_delete_times_out_to_idle() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingDelete;
        sb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        sb.tick();
        assert_eq!(sb.ssh_state, SshBrowserState::Idle);
    }

    #[test]
    fn no_timeout_without_cmd_start() {
        let (mut sb, _h) = make_ssh();
        sb.ssh_state = SshBrowserState::WaitingLs;
        sb.core.cmd_start = None;
        sb.tick();
        assert_eq!(sb.ssh_state, SshBrowserState::WaitingLs);
    }
}
