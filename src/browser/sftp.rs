use std::time::Instant;

use anyhow::Result;
use crossterm::event::KeyCode;
use log::{debug, info, warn};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use super::common::{
    Browser, BrowserCore, BrowserFocus, COMMAND_TIMEOUT_SECS, DeleteLocation, LinkProbe,
    PROMPT_TAIL_BYTES, PendingTransfer, TransferDirection, TransferStatus,
};
use super::parse::{
    contains_any_error, parse_ls, parse_pwd, read_local_dir, scrape_transfer_progress, shell_quote,
    strip_ansi,
};
use crate::keybindings::BrowserBindings;
use crate::terminal::{EmbeddedTerminal, PtyChannel};

#[cfg(test)]
use crate::terminal::{MockPty, MockPtyHandle};

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
// Prompt detection
// ---------------------------------------------------------------------------

/// True when the last non-empty output line contains the `sftp>` prompt.
fn sftp_prompt_at_tail(pty: &dyn PtyChannel) -> bool {
    let tail_bytes = pty.raw_tail(PROMPT_TAIL_BYTES);
    if tail_bytes.is_empty() {
        return false;
    }
    let tail = strip_ansi(&tail_bytes);
    tail.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.contains("sftp>"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// FileBrowser
// ---------------------------------------------------------------------------

pub struct FileBrowser {
    pub core: BrowserCore,
    pub sftp: Box<dyn PtyChannel>,
    pub sftp_state: SftpState,
}

impl FileBrowser {
    pub fn new(host: &str) -> Result<Self> {
        let sftp = EmbeddedTerminal::sftp(host)?;
        Ok(FileBrowser {
            core: BrowserCore::new(host),
            sftp: Box::new(sftp),
            sftp_state: SftpState::Connecting,
        })
    }

    #[cfg(test)]
    pub fn with_mock() -> (Self, MockPtyHandle) {
        let (mock, handle) = MockPty::new();
        let browser = FileBrowser {
            core: BrowserCore::new("test-host"),
            sftp: Box::new(mock),
            sftp_state: SftpState::Connecting,
        };
        (browser, handle)
    }

    pub fn tick(&mut self) {
        self.core.check_paste_deadline();
        let prompt_ready = self.core.response.ready(self.sftp.raw_seq(), || {
            sftp_prompt_at_tail(self.sftp.as_ref())
        });

        if matches!(self.sftp_state, SftpState::Connecting) {
            self.core.raw_snapshot = self.sftp.raw_lines();
        }

        // If the SFTP process died mid-operation, recover immediately.
        if self.sftp_state != SftpState::Idle
            && self.sftp_state != SftpState::Connecting
            && self.sftp.process_exited()
        {
            warn!(
                "SFTP process died in state {:?}, recovering",
                self.sftp_state
            );
            self.core.drop_confirm = None;
            self.core.link_probe = None;
            self.core.transfer.pending.clear();
            self.core.delete.pending.clear();
            self.core.transfer.batch_done = 0;
            self.core.transfer.batch_total = 0;
            self.core.transfer.start = None;
            self.sftp_state = SftpState::Idle;
            self.core.status_msg = "SFTP connection lost".to_string();
            self.core.status_color = Color::Red;
            self.core.needs_redraw = true;
            return;
        }

        // Command timeout: waiting states that should not stall indefinitely.
        if matches!(
            self.sftp_state,
            SftpState::WaitingPwd | SftpState::WaitingLs | SftpState::WaitingDelete
        ) && let Some(start) = self.core.cmd_start
            && start.elapsed().as_secs() >= COMMAND_TIMEOUT_SECS
        {
            warn!("SFTP command timed out in state {:?}", self.sftp_state);
            self.core.cmd_start = None;
            self.core.link_probe = None;
            self.sftp_state = SftpState::Idle;
            self.core.status_msg = "Command timed out".to_string();
            self.core.status_color = Color::Red;
            self.core.needs_redraw = true;
            return;
        }

        match self.sftp_state {
            SftpState::Connecting => {
                if prompt_ready {
                    self.sftp.drain_raw();
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
                    let lines = self.sftp.raw_lines();
                    self.core.remote.path =
                        parse_pwd(&lines).unwrap_or_else(|| self.core.remote.path.clone());
                    debug!("SFTP pwd => {}, sending ls -la", self.core.remote.path);
                    self.sftp.drain_raw();
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                    self.core.needs_redraw = true;
                }
            }
            SftpState::WaitingLs => {
                if prompt_ready {
                    let lines = self.sftp.raw_lines();
                    let parsed = parse_ls(&lines);
                    // Symlink probe: sftp `ls` of a link-to-file echoes a
                    // single dereferenced entry named like the link itself;
                    // a dangling link echoes its own lrwx entry.
                    if let Some(link_name) = self.core.link_probe.take() {
                        let probe = if parsed.len() == 2 && parsed[1].name == link_name {
                            if parsed[1].is_link {
                                LinkProbe::Broken
                            } else if !parsed[1].is_dir {
                                LinkProbe::File
                            } else {
                                LinkProbe::Dir
                            }
                        } else {
                            LinkProbe::Dir
                        };
                        if self.core.apply_link_probe(&link_name, probe) {
                            self.sftp.drain_raw();
                            self.send_ls();
                            return; // re-list the reverted path
                        }
                    }
                    debug!("SFTP ls done: {} entries", parsed.len());
                    self.core.remote.entries = parsed;
                    self.core.raw_snapshot.clear();
                    let max = self.core.remote.entries.len().saturating_sub(1);
                    let cur = self.core.remote.sel.selected().unwrap_or(0);
                    self.core.remote.sel.select(Some(cur.min(max)));
                    self.sftp.drain_raw();
                    self.sftp_state = SftpState::Idle;
                    self.core.stop_timer();
                    if self.core.status_color == Color::Yellow {
                        self.core.status_color = Color::Green;
                    }
                    self.core.needs_redraw = true;
                    debug!(
                        "SFTP WaitingLs done: pending_transfers={}, pending_deletes={}",
                        self.core.transfer.pending.len(),
                        self.core.delete.pending.len(),
                    );
                    self.chain_next_queued();
                }
            }
            SftpState::Transferring => {
                if prompt_ready {
                    // sftp prints errors ("Couldn't ...", "... Permission denied")
                    // and still returns to the prompt — check before declaring
                    // success.
                    let lines = self.sftp.raw_lines();
                    let failed = contains_any_error(
                        &lines,
                        &["permission denied", "no such file", "couldn't"],
                    );
                    let completion_msg = self.core.transfer.last.as_mut().map(|t| {
                        t.done = true;
                        t.progress = 100;
                        let verb = match (failed, t.direction) {
                            (true, _) => "Transfer failed",
                            (false, TransferDirection::Download) => "Downloaded",
                            (false, TransferDirection::Upload) => "Uploaded",
                        };
                        format!("{}: {}", verb, t.filename)
                    });
                    if let Some(msg) = completion_msg {
                        self.core.status_msg = msg;
                        self.core.status_color = if failed { Color::Red } else { Color::Green };
                    }
                    if failed {
                        warn!("SFTP transfer failed, cancelling batch");
                        self.core.transfer.pending.clear();
                    }
                    self.core.transfer.current = None;
                    self.core.transfer.batch_done += 1;
                    self.core.local.entries = read_local_dir(&self.core.local.path);
                    info!(
                        "SFTP transfer complete (failed={}), pending_transfers={}",
                        failed,
                        self.core.transfer.pending.len(),
                    );
                    self.sftp.drain_raw();
                    // Skip the ls round-trip when more transfers are queued.
                    // sftp get/put does not remove the source file, so the
                    // existing entries are still valid for chaining.
                    if !self.core.transfer.pending.is_empty() {
                        self.sftp_state = SftpState::Idle;
                        match self.core.pending_direction() {
                            TransferDirection::Upload => self.upload(),
                            TransferDirection::Download => self.download(),
                        }
                        return;
                    }
                    // Batch complete — reset counters before the final ls refresh.
                    self.core.transfer.start = None;
                    self.core.transfer.batch_done = 0;
                    self.core.transfer.batch_total = 0;
                    self.send_ls();
                    self.sftp_state = SftpState::WaitingLs;
                } else {
                    let lines = self.sftp.raw_lines();
                    if let Some(ref mut t) = self.core.transfer.last {
                        if t.is_dir {
                            let count = lines
                                .iter()
                                .filter(|l| l.contains("Fetching ") || l.contains("Uploading "))
                                .count();
                            if count != t.file_count {
                                t.file_count = count;
                                self.core.needs_redraw = true;
                            }
                        } else {
                            if let Some(pct) = scrape_transfer_progress(&lines) {
                                t.progress = pct;
                                self.core.needs_redraw = true;
                            }
                            // Single-file progress rewrites accumulate without
                            // bound on long transfers; keep only a tail (which
                            // still contains the newest progress, any error
                            // text, and the prompt when it arrives).
                            if self.sftp.raw_len() > 16 * 1024 {
                                self.sftp.drain_raw_keep(1024);
                            }
                        }
                    }
                }
            }
            SftpState::WaitingDelete => {
                if prompt_ready {
                    let lines = self.sftp.raw_lines();
                    let has_error = contains_any_error(
                        &lines,
                        &["failure", "couldn't", "not empty", "permission denied"],
                    );
                    if let Some(name) = self.core.delete.pending_name.take() {
                        if has_error {
                            warn!("SFTP delete failed: {}", name);
                            self.core.status_msg = format!("Delete failed: {}", name);
                            self.core.status_color = Color::Red;
                            self.core.delete.pending.clear();
                        } else {
                            self.core.status_msg = format!("Deleted remote: {}", name);
                            self.core.status_color = Color::Green;
                        }
                    }
                    self.sftp.drain_raw();
                    // Skip the ls round-trip when more deletes are queued —
                    // chain directly to the next delete instead.
                    if !has_error && self.core.pop_pending_delete() {
                        self.confirm_delete_yes();
                    } else {
                        self.send_ls();
                        self.sftp_state = SftpState::WaitingLs;
                    }
                    self.core.needs_redraw = true;
                }
            }
            SftpState::Idle => {}
        }
    }

    // ---- navigation (delegates to core for local, handles remote) ----------

    pub fn enter(&mut self) {
        match self.core.focus {
            BrowserFocus::Local => self.core.local_enter(),
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle {
                    return;
                }
                if let Some(i) = self.core.remote.sel.selected()
                    && let Some(entry) = self.core.remote.entries.get(i).cloned()
                {
                    if entry.is_dir {
                        if entry.is_link {
                            self.core.link_probe = Some(entry.name.clone());
                        }
                        self.core.apply_cd(&entry.name);
                        self.core.status_msg = format!("Remote: {}", self.core.remote.path);
                        self.core.status_color = Color::Yellow;
                        self.sftp.drain_raw();
                        self.send_ls();
                        self.sftp_state = SftpState::WaitingLs;
                        debug!("SFTP ls {}", self.core.remote.path);
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
                self.core.status_msg = format!("Remote: {}", self.core.remote.path);
                self.core.status_color = Color::Yellow;
                self.sftp.drain_raw();
                self.send_ls();
                self.sftp_state = SftpState::WaitingLs;
                debug!("SFTP ls {}", self.core.remote.path);
            }
        }
    }

    fn send_ls(&mut self) {
        let cmd = format!("ls -la {}\r\n", shell_quote(&self.core.remote.path));
        self.core.cmd_start = Some(Instant::now());
        self.sftp.send_str(&cmd);
    }

    // ---- transfers ---------------------------------------------------------

    pub fn download(&mut self) {
        if self.sftp_state != SftpState::Idle {
            debug!("SFTP download: skipped, state={:?}", self.sftp_state);
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
        let flag = if is_dir { "-r " } else { "" };
        // Quote the local destination too — Windows home paths often contain
        // spaces and would otherwise split into two sftp arguments.
        let cmd = format!(
            "get {}{} {}\r\n",
            flag,
            shell_quote(&remote_file),
            shell_quote(&format!("{}/", local_dest))
        );
        if self.core.transfer.batch_done == 0 {
            self.core.transfer.batch_total = self.core.transfer.pending.len() + 1;
        }
        self.core.transfer.start = Some(Instant::now());
        self.core.transfer.current = Some(PendingTransfer {
            path: remote_file.clone(),
            name: name.clone(),
            is_dir,
        });
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
        info!("SFTP get {} -> {}", name, local_dest);
        self.sftp.send_str(&cmd);
        self.sftp_state = SftpState::Transferring;
    }

    pub fn upload(&mut self) {
        if self.sftp_state != SftpState::Idle {
            debug!("SFTP upload: skipped, state={:?}", self.sftp_state);
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
        let flag = if is_dir { "-r " } else { "" };
        let cmd = format!(
            "put {}{} {}\r\n",
            flag,
            shell_quote(&local_str),
            shell_quote(&format!("{}/", self.core.remote.path.trim_end_matches('/')))
        );
        if self.core.transfer.batch_done == 0 {
            self.core.transfer.batch_total = self.core.transfer.pending.len() + 1;
        }
        self.core.transfer.start = Some(Instant::now());
        self.core.transfer.current = Some(PendingTransfer {
            path: local_str.clone(),
            name: name.clone(),
            is_dir,
        });
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
        info!("SFTP put {}", local_str);
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
        if let Some(target) = self.core.delete.confirm.take() {
            if target.location != DeleteLocation::Remote {
                return;
            }
            info!("SFTP remote delete: {}", target.path);
            // SFTP protocol only supports rmdir (non-recursive). Non-empty dirs will fail.
            // The SSH/SCP browser uses `rm -rf` via shell, which handles non-empty dirs.
            let cmd = if target.is_dir() {
                format!("rmdir {}\r\n", shell_quote(&target.path))
            } else {
                format!("rm {}\r\n", shell_quote(&target.path))
            };
            self.sftp.drain_raw();
            self.sftp.send_str(&cmd);
            self.sftp_state = SftpState::WaitingDelete;
            self.core.status_msg = format!("Deleting {}...", target.path);
            self.core.status_color = Color::Yellow;
            self.core.delete.pending_name = Some(target.path);
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
        let title = format!(" sftp: {} ", self.core.host);
        let status_area = self
            .core
            .render_panels(area, buf, is_focus, leaf_count, &title);
        if !self.core.render_confirm_delete(status_area, buf) {
            let (label, color) = self.state_label();
            let progress = self.progress_suffix();
            self.core
                .render_normal_status(status_area, buf, label, color, &progress, bindings);
        }
        self.core.render_upload_confirm(area, buf);
        self.core
            .render_transfer_progress(area, buf, self.core.transfer.start.is_some());
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
        if let Some(ref t) = self.core.transfer.last {
            if t.is_dir && !t.done {
                format!(" ({} files)", t.file_count)
            } else if !t.is_dir && !t.done {
                format!(" {}%", t.progress)
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }
}

impl Browser for FileBrowser {
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
        self.sftp_state == SftpState::Connecting
    }
    fn send_connect_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.sftp.send_char(c),
            KeyCode::Enter => self.sftp.send_str("\r\n"),
            KeyCode::Backspace => self.sftp.send_str("\x7f"),
            _ => {}
        }
    }
    fn process_exited(&self) -> bool {
        self.sftp.process_exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::common::{DeleteKind, DeleteTarget, dummy_entry};
    use crate::browser::parse::FsEntry;

    /// Run tick() enough times for the response watch to trigger a transition:
    /// the first tick observes the new raw_seq (reset), the next two are quiet
    /// ticks with the prompt at the tail (PROMPT_STABLE_TICKS = 2).
    fn tick_until_stable(browser: &mut FileBrowser) {
        browser.tick(); // seq changed → reset
        browser.tick(); // quiet + prompt → 1
        browser.tick(); // quiet + prompt → 2 → transition
    }

    fn make_sftp() -> (FileBrowser, MockPtyHandle) {
        FileBrowser::with_mock()
    }

    // ---- Connecting state ----

    #[test]
    fn connecting_transitions_to_waiting_pwd_and_sends_pwd() {
        let (mut fb, h) = make_sftp();
        assert_eq!(fb.sftp_state, SftpState::Connecting);

        h.feed(b"Connected to host.\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingPwd);
        assert_eq!(fb.sftp.raw_len(), 0, "drain_raw should clear buffer");
        assert!(
            h.sent().iter().any(|s| s == "pwd\r\n"),
            "should send pwd command, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn connecting_stays_without_prompt() {
        let (mut fb, h) = make_sftp();
        h.feed(b"Connecting to host...\n");
        tick_until_stable(&mut fb);
        assert_eq!(fb.sftp_state, SftpState::Connecting);
        assert!(h.sent().is_empty());
    }

    #[test]
    fn connecting_captures_raw_snapshot() {
        let (mut fb, h) = make_sftp();
        h.feed(b"Connecting to host...\nsftp> ");
        fb.tick();
        assert!(!fb.core.raw_snapshot.is_empty());
    }

    #[test]
    fn connecting_sets_status_on_connect() {
        let (mut fb, h) = make_sftp();
        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);
        assert_eq!(fb.core.status_color, Color::Green);
        assert!(fb.core.status_msg.contains("Connected to test-host"));
    }

    // ---- WaitingPwd state ----

    #[test]
    fn waiting_pwd_transitions_to_waiting_ls_and_sends_ls() {
        let (mut fb, h) = make_sftp();
        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);
        assert_eq!(fb.sftp_state, SftpState::WaitingPwd);
        h.clear_sent();

        h.feed(b"Remote working directory: /home/user\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.remote.path, "/home/user");
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(
            !ls_cmds.is_empty(),
            "should send ls command, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_pwd_keeps_old_path_on_parse_failure() {
        let (mut fb, h) = make_sftp();
        fb.core.remote.path = "/original".to_string();
        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);
        assert_eq!(fb.sftp_state, SftpState::WaitingPwd);

        h.feed(b"some noise\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.remote.path, "/original");
    }

    // ---- WaitingLs state ----

    #[test]
    fn waiting_ls_populates_entries_and_goes_idle() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;

        h.feed(b"drwxr-xr-x  2 user user  4096 Jan  1 12:00 subdir\n-rw-r--r--  1 user user  1234 Jan  1 12:00 file.txt\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert!(fb.core.remote.entries.len() >= 2);
        assert!(
            fb.core.raw_snapshot.is_empty(),
            "raw_snapshot should be cleared"
        );
    }

    #[test]
    fn waiting_ls_clamps_selection() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.remote.sel.select(Some(50));

        h.feed(b"-rw-r--r--  1 user user  1234 Jan  1 12:00 file.txt\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::Idle);
        let sel = fb.core.remote.sel.selected().unwrap_or(999);
        assert!(sel < fb.core.remote.entries.len());
    }

    #[test]
    fn waiting_ls_chains_pending_transfer() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "prev.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: true,
            progress: 100,
            file_count: 0,
        });
        fb.core.transfer.pending.push(PendingTransfer {
            path: "/remote/next.txt".to_string(),
            name: "next.txt".to_string(),
            is_dir: false,
        });

        h.feed(b"-rw-r--r-- 1 u u 10 Jan 1 12:00 a.txt\nsftp> ");
        tick_until_stable(&mut fb);

        // download() should have been called, sending a get command
        assert_eq!(fb.sftp_state, SftpState::Transferring);
        assert!(fb.core.transfer.pending.is_empty());
        let get_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("get "))
            .cloned()
            .collect();
        assert!(
            !get_cmds.is_empty(),
            "should send get command, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_ls_chains_pending_delete() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/todelete.txt".to_string(),
        });
        // pop_pending_delete moves from pending_deletes to confirm_delete
        // then confirm_delete_yes processes it

        h.feed(b"-rw-r--r-- 1 u u 10 Jan 1 12:00 a.txt\nsftp> ");
        tick_until_stable(&mut fb);

        // Should chain to delete
        assert_eq!(fb.sftp_state, SftpState::WaitingDelete);
        let rm_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("rm "))
            .cloned()
            .collect();
        assert!(
            !rm_cmds.is_empty(),
            "should send rm command, got: {:?}",
            h.sent()
        );
    }

    // ---- Transferring state ----

    #[test]
    fn transferring_completes_on_prompt_and_sends_ls() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "test.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        h.clear_sent();

        h.feed(b"Fetching /remote/test.txt to test.txt\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert!(fb.core.transfer.last.as_ref().unwrap().done);
        assert_eq!(fb.core.transfer.last.as_ref().unwrap().progress, 100);
        assert_eq!(
            fb.core.transfer.batch_done, 0,
            "batch counters should reset after final transfer"
        );
        assert_eq!(fb.core.transfer.batch_total, 0);
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(
            !ls_cmds.is_empty(),
            "should send ls after transfer, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn transferring_scrapes_progress() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "big.bin".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });

        h.feed(b"big.bin                                       50%  512KB 256.0KB/s   00:01");
        fb.tick();

        assert_eq!(fb.sftp_state, SftpState::Transferring);
        assert_eq!(fb.core.transfer.last.as_ref().unwrap().progress, 50);
    }

    #[test]
    fn transferring_counts_dir_files() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "mydir".to_string(),
            direction: TransferDirection::Download,
            is_dir: true,
            done: false,
            progress: 0,
            file_count: 0,
        });

        h.feed(b"Fetching /remote/mydir/ to mydir\nFetching /remote/mydir/a.txt\nFetching /remote/mydir/b.txt\n");
        fb.tick();

        assert_eq!(fb.core.transfer.last.as_ref().unwrap().file_count, 3);
    }

    #[test]
    fn transferring_chains_next_download() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "first.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        fb.core.transfer.pending.push(PendingTransfer {
            path: "/remote/second.txt".to_string(),
            name: "second.txt".to_string(),
            is_dir: false,
        });

        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);

        // download() sets sftp_state to Transferring and sends get command
        assert_eq!(fb.sftp_state, SftpState::Transferring);
        assert!(fb.core.transfer.pending.is_empty());
        let get_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("get "))
            .cloned()
            .collect();
        assert!(
            !get_cmds.is_empty(),
            "should send get for next file, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn transferring_chains_next_upload() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "first.txt".to_string(),
            direction: TransferDirection::Upload,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        fb.core.transfer.pending.push(PendingTransfer {
            path: "/local/second.txt".to_string(),
            name: "second.txt".to_string(),
            is_dir: false,
        });

        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::Transferring);
        assert!(fb.core.transfer.pending.is_empty());
        let put_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("put "))
            .cloned()
            .collect();
        assert!(
            !put_cmds.is_empty(),
            "should send put for next file, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn transferring_increments_batch_done() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.batch_done = 0;
        fb.core.transfer.batch_total = 1;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "only.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });

        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);

        // After the last transfer, batch counters reset to 0
        assert_eq!(fb.core.transfer.batch_done, 0);
        assert_eq!(fb.core.transfer.batch_total, 0);
        assert!(fb.core.transfer.start.is_none());
    }

    #[test]
    fn download_quotes_local_destination_with_spaces() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.focus = BrowserFocus::Remote;
        fb.core.remote.path = "/home".to_string();
        fb.core.local.path = std::path::PathBuf::from("C:/Users/First Last/Downloads");
        fb.core.remote.entries = vec![dummy_entry("..", true), dummy_entry("file.txt", false)];
        fb.core.remote.sel.select(Some(1));
        h.clear_sent();

        fb.download();

        let sent = h.sent();
        let get_cmd = sent.iter().find(|s| s.starts_with("get ")).unwrap();
        assert!(
            get_cmd.contains("'C:/Users/First Last/Downloads/'"),
            "local destination must be quoted, got: {}",
            get_cmd
        );
    }

    #[test]
    fn transferring_error_output_reports_failure() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Transferring;
        fb.core.transfer.last = Some(TransferStatus {
            filename: "secret.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 0,
            file_count: 0,
        });
        fb.core.transfer.pending.push(PendingTransfer {
            path: "/remote/next.txt".to_string(),
            name: "next.txt".to_string(),
            is_dir: false,
        });

        h.feed(b"Fetching /remote/secret.txt to secret.txt\nremote open(\"/remote/secret.txt\"): Permission denied\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.core.status_color, Color::Red);
        assert!(
            fb.core.status_msg.contains("Transfer failed"),
            "expected failure status, got: {}",
            fb.core.status_msg
        );
        assert!(
            fb.core.transfer.pending.is_empty(),
            "a failed transfer must cancel the rest of the batch"
        );
        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
    }

    // ---- WaitingDelete state ----

    #[test]
    fn waiting_delete_success_sends_ls() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingDelete;
        fb.core.delete.pending_name = Some("removed.txt".to_string());
        h.clear_sent();

        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.status_color, Color::Green);
        assert!(fb.core.status_msg.contains("Deleted remote: removed.txt"));
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(
            !ls_cmds.is_empty(),
            "should send ls after delete, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_delete_failure_shows_error() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingDelete;
        fb.core.delete.pending_name = Some("protected.txt".to_string());

        h.feed(b"Couldn't remove file: permission denied\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.status_color, Color::Red);
        assert!(fb.core.status_msg.contains("Delete failed: protected.txt"));
    }

    #[test]
    fn waiting_delete_chains_next_delete() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingDelete;
        fb.core.delete.pending_name = Some("first.txt".to_string());
        fb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/second.txt".to_string(),
        });
        h.clear_sent();

        h.feed(b"sftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingDelete);
        assert!(fb.core.delete.pending.is_empty());
        let rm_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("rm "))
            .cloned()
            .collect();
        assert!(
            !rm_cmds.is_empty(),
            "should send rm for next delete, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_delete_failure_clears_pending_and_sends_ls() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingDelete;
        fb.core.delete.pending_name = Some("bad.txt".to_string());
        fb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/next.txt".to_string(),
        });
        h.clear_sent();

        h.feed(b"Couldn't remove file\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert!(fb.core.delete.pending.is_empty());
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(
            !ls_cmds.is_empty(),
            "should send ls after failed delete, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_delete_rmdir_for_directories() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::Dir,
            path: "/remote/somedir".to_string(),
        });
        h.clear_sent();

        fb.confirm_delete_yes();

        assert_eq!(fb.sftp_state, SftpState::WaitingDelete);
        let rmdir_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("rmdir "))
            .cloned()
            .collect();
        assert!(
            !rmdir_cmds.is_empty(),
            "should use rmdir for dirs, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn waiting_delete_rm_for_files() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/remote/somefile.txt".to_string(),
        });
        h.clear_sent();

        fb.confirm_delete_yes();

        assert_eq!(fb.sftp_state, SftpState::WaitingDelete);
        let rm_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("rm "))
            .cloned()
            .collect();
        assert!(
            !rm_cmds.is_empty(),
            "should use rm for files, got: {:?}",
            h.sent()
        );
        let rmdir_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("rmdir "))
            .cloned()
            .collect();
        assert!(rmdir_cmds.is_empty(), "should not use rmdir for files");
    }

    // ---- Process death recovery ----

    #[test]
    fn process_death_recovers_to_idle() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.transfer.pending.push(PendingTransfer {
            path: "/a".to_string(),
            name: "a".to_string(),
            is_dir: false,
        });
        fb.core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/b".to_string(),
        });

        h.set_exited(true);
        fb.tick();

        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(fb.core.status_color, Color::Red);
        assert!(fb.core.status_msg.contains("connection lost"));
        assert!(
            fb.core.transfer.pending.is_empty(),
            "pending_transfers should be cleared"
        );
        assert!(
            fb.core.delete.pending.is_empty(),
            "pending_deletes should be cleared"
        );
        assert!(fb.core.drop_confirm.is_none());
    }

    #[test]
    fn process_death_ignored_in_idle() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.status_msg = "fine".to_string();

        h.set_exited(true);
        fb.tick();

        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(
            fb.core.status_msg, "fine",
            "idle state should not trigger recovery"
        );
    }

    #[test]
    fn process_death_ignored_in_connecting() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Connecting;

        h.set_exited(true);
        fb.tick();

        // Connecting is excluded from the death check
        assert_eq!(fb.sftp_state, SftpState::Connecting);
    }

    // ---- Prompt stability ----

    #[test]
    fn no_transition_before_stable() {
        let (mut fb, h) = make_sftp();
        h.feed(b"sftp> ");

        // Only 2 ticks — one quiet tick with the prompt, not two
        fb.tick();
        fb.tick();

        assert_eq!(fb.sftp_state, SftpState::Connecting);
    }

    #[test]
    fn changing_raw_len_resets_stability() {
        let (mut fb, h) = make_sftp();
        h.feed(b"sftp> ");
        fb.tick(); // seq observed
        fb.tick(); // quiet tick 1

        h.feed(b" ");
        fb.tick(); // new data → watch reset

        assert_eq!(fb.sftp_state, SftpState::Connecting);
    }

    // ---- Idle state ----

    #[test]
    fn idle_tick_is_noop() {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.status_msg = "idle".to_string();

        fb.tick();

        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(fb.core.status_msg, "idle");
    }

    // ---- Navigation ----

    #[test]
    fn enter_on_remote_dir_sends_ls() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.remote.path = "/home".to_string();
        fb.core.focus = BrowserFocus::Remote;
        fb.core.remote.entries.push(FsEntry {
            name: "subdir".to_string(),
            is_dir: true,
            size: "4096".to_string(),
            modified: "Jan 1 12:00".to_string(),
            perms: "drwxr-xr-x".to_string(),
            ..FsEntry::default()
        });
        fb.core.remote.sel.select(Some(0));
        h.clear_sent();

        fb.enter();

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.remote.path, "/home/subdir");
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(!ls_cmds.is_empty(), "should send ls, got: {:?}", h.sent());
    }

    #[test]
    fn go_up_remote_sends_ls() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.remote.path = "/home/user".to_string();
        fb.core.focus = BrowserFocus::Remote;
        h.clear_sent();

        fb.go_up();

        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.remote.path, "/home");
        let ls_cmds: Vec<_> = h
            .sent()
            .iter()
            .filter(|s| s.starts_with("ls "))
            .cloned()
            .collect();
        assert!(!ls_cmds.is_empty());
    }

    #[test]
    fn enter_ignored_when_not_idle() {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.focus = BrowserFocus::Remote;
        fb.core.remote.entries.push(FsEntry {
            name: "dir".to_string(),
            is_dir: true,
            size: "4096".to_string(),
            modified: "Jan 1 12:00".to_string(),
            perms: "drwxr-xr-x".to_string(),
            ..FsEntry::default()
        });
        fb.core.remote.sel.select(Some(0));
        h.clear_sent();

        fb.enter();

        assert_eq!(
            fb.sftp_state,
            SftpState::WaitingLs,
            "should not change state"
        );
        assert!(h.sent().is_empty(), "should not send any command");
    }

    // ---- Symlink probe ----

    fn link_entry(name: &str) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            is_dir: true,
            is_link: true,
            ..FsEntry::default()
        }
    }

    fn sftp_with_link(name: &str) -> (FileBrowser, MockPtyHandle) {
        let (mut fb, h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.focus = BrowserFocus::Remote;
        fb.core.remote.path = "/home".to_string();
        fb.core.remote.entries = vec![dummy_entry("..", true), link_entry(name)];
        fb.core.remote.sel.select(Some(1));
        h.clear_sent();
        (fb, h)
    }

    #[test]
    fn enter_file_symlink_reverts_and_downloads() {
        let (mut fb, h) = sftp_with_link("filelink");

        fb.enter();
        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.remote.path, "/home/filelink");

        // sftp echoes the dereferenced single-file entry for a link-to-file.
        h.feed(b"-rw-r--r--    ? u g 6 Jan 1 12:00 /home/filelink\nsftp> ");
        tick_until_stable(&mut fb);

        // Probe: cd reverted, download queued, re-listing in flight.
        assert_eq!(fb.core.remote.path, "/home");
        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
        assert_eq!(fb.core.transfer.pending.len(), 1);
        assert_eq!(fb.core.transfer.pending[0].path, "/home/filelink");

        // Re-listing completes → chain fires the download.
        h.feed(b"drwxr-xr-x ? u g 4096 Jan 1 12:00 sub\nsftp> ");
        tick_until_stable(&mut fb);
        assert_eq!(fb.sftp_state, SftpState::Transferring);
        assert!(
            h.sent()
                .iter()
                .any(|s| s.starts_with("get ") && s.contains("/home/filelink")),
            "should download the link target, got: {:?}",
            h.sent()
        );
    }

    #[test]
    fn enter_broken_symlink_reverts_with_error() {
        let (mut fb, h) = sftp_with_link("dangling");

        fb.enter();
        // sftp lstat echo of the dangling link itself (lrwx, no arrow).
        h.feed(b"lrwxrwxrwx    ? u g 11 Jan 1 12:00 /home/dangling\nsftp> ");
        tick_until_stable(&mut fb);

        assert_eq!(fb.core.remote.path, "/home");
        assert!(fb.core.transfer.pending.is_empty());
        assert_eq!(fb.core.status_color, Color::Red);
        assert!(
            fb.core.status_msg.contains("broken"),
            "got: {}",
            fb.core.status_msg
        );
    }

    #[test]
    fn enter_dir_symlink_lists_contents() {
        let (mut fb, h) = sftp_with_link("dirlink");

        fb.enter();
        h.feed(
            b"drwxr-xr-x ? u g 4096 Jan 1 12:00 sub\n-rw-r--r-- ? u g 5 Jan 1 12:00 notes.txt\nsftp> ",
        );
        tick_until_stable(&mut fb);

        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(fb.core.remote.path, "/home/dirlink");
        assert!(fb.core.remote.entries.iter().any(|e| e.name == "notes.txt"));
        assert!(fb.core.transfer.pending.is_empty());
    }

    // ---- send_connect_key ----

    #[test]
    fn send_connect_key_forwards_chars() {
        let (mut fb, h) = make_sftp();
        h.clear_sent();

        fb.send_connect_key(KeyCode::Char('y'));
        fb.send_connect_key(KeyCode::Enter);
        fb.send_connect_key(KeyCode::Backspace);

        let s = h.sent();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "y");
        assert_eq!(s[1], "\r\n");
        assert_eq!(s[2], "\x7f");
    }

    // ---- State label ----

    #[test]
    fn state_labels() {
        let (mut fb, _h) = make_sftp();

        let cases = [
            (SftpState::Connecting, "connecting"),
            (SftpState::WaitingPwd, "loading"),
            (SftpState::WaitingLs, "loading"),
            (SftpState::Idle, "idle"),
            (SftpState::WaitingDelete, "deleting"),
            (SftpState::Transferring, "transfer"),
        ];
        for (state, expected) in cases {
            fb.sftp_state = state;
            assert_eq!(fb.state_label().0, expected, "state_label for {:?}", state);
        }
    }

    // ---- Progress suffix ----

    #[test]
    fn progress_suffix_file() {
        let (mut fb, _h) = make_sftp();
        fb.core.transfer.last = Some(TransferStatus {
            filename: "f.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 42,
            file_count: 0,
        });
        assert_eq!(fb.progress_suffix(), " 42%");
    }

    #[test]
    fn progress_suffix_dir() {
        let (mut fb, _h) = make_sftp();
        fb.core.transfer.last = Some(TransferStatus {
            filename: "d".to_string(),
            direction: TransferDirection::Download,
            is_dir: true,
            done: false,
            progress: 0,
            file_count: 5,
        });
        assert_eq!(fb.progress_suffix(), " (5 files)");
    }

    #[test]
    fn progress_suffix_done() {
        let (mut fb, _h) = make_sftp();
        fb.core.transfer.last = Some(TransferStatus {
            filename: "f.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: true,
            progress: 100,
            file_count: 0,
        });
        assert_eq!(fb.progress_suffix(), "");
    }

    #[test]
    fn progress_suffix_none() {
        let (fb, _h) = make_sftp();
        assert_eq!(fb.progress_suffix(), "");
    }

    // ---- Golden frames (behavior freeze for the widget refactor) ----------

    use crate::widgets::testing::assert_rows;

    /// A browser with fully deterministic panel content.
    fn golden_browser() -> FileBrowser {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::Idle;
        fb.core.local.path = std::path::PathBuf::from("C:/l");
        fb.core.local.entries = vec![
            dummy_entry("..", true),
            dummy_entry("a.txt", false),
            dummy_entry("dir", true),
        ];
        fb.core.local.sel.select(Some(1));
        fb.core.remote.path = "/r".to_string();
        fb.core.remote.entries = vec![dummy_entry("..", true), dummy_entry("b.txt", false)];
        fb.core.remote.sel.select(Some(0));
        fb.core.status_msg = "ready".to_string();
        fb
    }

    fn render_golden(fb: &mut FileBrowser) -> ratatui::buffer::Buffer {
        let area = Rect::new(0, 0, 56, 9);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        fb.render(area, &mut buf, true, 1, &BrowserBindings::default());
        buf
    }

    #[test]
    fn golden_browser_idle() {
        let mut fb = golden_browser();
        let buf = render_golden(&mut fb);
        assert_rows(
            &buf,
            &[
                "┌ C:/l ───────────  local  ┐┌ /r ────────────  remote  ┐",
                "│../           0 2025-01-01││../           0 2025-01-01│",
                "│a.txt         0 2025-01-01││b.txt         0 2025-01-01│",
                "│dir/          0 2025-01-01││                          │",
                "│                          ││                          │",
                "│                          ││                          │",
                "│                          ││                          │",
                "└──────────────────────────┘└──────────────────────────┘",
                "[idle] ready                         [T]xfer [Delete]rm",
            ],
        );
    }

    #[test]
    fn golden_browser_delete_confirm() {
        let mut fb = golden_browser();
        fb.core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/r/b.txt".to_string(),
        });
        let buf = render_golden(&mut fb);
        assert_rows(
            &buf,
            &[
                "┌ C:/l ───────────  local  ┐┌ /r ────────────  remote  ┐",
                "│../           0 2025-01-01││../           0 2025-01-01│",
                "│a.txt         0 2025-01-01││b.txt         0 2025-01-01│",
                "│dir/          0 2025-01-01││                          │",
                "│                          ││                          │",
                "│                          ││                          │",
                "│                          ││                          │",
                "└──────────────────────────┘└──────────────────────────┘",
                "  Delete remote '/r/b.txt'?  [y] Yes   [n] No",
            ],
        );
    }

    #[test]
    fn golden_browser_transfer_overlay() {
        let mut fb = golden_browser();
        fb.sftp_state = SftpState::Transferring;
        // A future start makes elapsed() saturate to zero — deterministic "0ms".
        fb.core.transfer.start = Some(Instant::now() + std::time::Duration::from_secs(60));
        fb.core.transfer.last = Some(TransferStatus {
            filename: "b.txt".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 40,
            file_count: 0,
        });
        fb.core.status_msg = "Downloading b.txt...".to_string();
        let buf = render_golden(&mut fb);
        assert_rows(
            &buf,
            &[
                "┌ C:/l ───────────  local  ┐┌ /r ────────────  remote  ┐",
                "│../           0 2025-01-01││../           0 2025-01-01│",
                "│a┌──────────────────── Transfer ────────────────────┐1│",
                "│d│↓  b.txt                                       0ms│ │",
                "│ │████████████████████   40%                        │ │",
                "│ └──────────────────────────────────────────────────┘ │",
                "│                          ││                          │",
                "└──────────────────────────┘└──────────────────────────┘",
                "[transfer] Downloading b.txt... 40%  [T]xfer [Delete]rm",
            ],
        );
    }

    // ---- Command timeout ----

    #[test]
    fn waiting_ls_times_out_to_idle() {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        // Simulate an expired timer (1 second past the timeout threshold)
        fb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        fb.tick();
        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(fb.core.status_color, Color::Red);
        assert!(fb.core.status_msg.contains("timed out"));
        assert!(fb.core.cmd_start.is_none());
    }

    #[test]
    fn waiting_pwd_times_out_to_idle() {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::WaitingPwd;
        fb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        fb.tick();
        assert_eq!(fb.sftp_state, SftpState::Idle);
        assert_eq!(fb.core.status_color, Color::Red);
    }

    #[test]
    fn waiting_delete_times_out_to_idle() {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::WaitingDelete;
        fb.core.cmd_start =
            Some(Instant::now() - std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS + 1));
        fb.tick();
        assert_eq!(fb.sftp_state, SftpState::Idle);
    }

    #[test]
    fn no_timeout_without_cmd_start() {
        let (mut fb, _h) = make_sftp();
        fb.sftp_state = SftpState::WaitingLs;
        fb.core.cmd_start = None;
        fb.tick();
        // Should not time out (no cmd_start means no timer running)
        assert_eq!(fb.sftp_state, SftpState::WaitingLs);
    }
}
