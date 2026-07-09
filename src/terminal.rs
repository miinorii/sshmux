use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use anyhow::Result;
use log::{debug, error, info, warn};

use crate::browser::parse::strip_ansi;
use crate::pty::{self, CommandBuilder};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use vt100::{MouseProtocolMode, Parser};

/// Split a manual-connect argument string into ssh arguments, honouring
/// single and double quotes so values like
/// `-o "ProxyCommand=ssh -W %h:%p jump"` stay a single argument.
/// Quote characters are stripped; there is no backslash escaping.
pub fn split_ssh_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Count the number of Device Status Report sequences (`ESC [ 6 n`) in `data`.
pub fn count_dsr(data: &[u8]) -> usize {
    const DSR: &[u8] = b"\x1b[6n";
    let mut count = 0;
    let mut i = 0;
    while i + DSR.len() <= data.len() {
        if data[i..].starts_with(DSR) {
            count += 1;
            i += DSR.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Trait abstracting the raw-output interface of a pseudo-terminal.
///
/// Browsers interact with the PTY exclusively through these methods.
/// `EmbeddedTerminal` implements this for production; tests can substitute
/// a `MockPty` to drive state machines without a real PTY.
pub trait PtyChannel {
    fn raw_len(&self) -> usize;
    fn raw_lines(&self) -> Vec<String>;
    /// Monotonic sequence number, bumped on every raw-buffer mutation
    /// (reader-thread appends and drains). Lets consumers detect "new data
    /// since I last looked" without hashing or re-scanning the buffer.
    fn raw_seq(&self) -> u64;
    fn drain_raw(&self);
    /// Drop all but the last `keep` bytes of the raw output buffer. Used to
    /// bound memory/CPU while scraping long-running transfer output.
    fn drain_raw_keep(&self, keep: usize);
    fn send_str(&mut self, s: &str);
    fn send_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.send_str(s);
    }
    fn process_exited(&self) -> bool;
    /// Exit code of the child process, or `None` while it is still running
    /// (or when the code cannot be determined).
    fn exit_code(&self) -> Option<u32>;
    /// Return the last `n` bytes of the raw output buffer (or fewer if shorter).
    fn raw_tail(&self, n: usize) -> Vec<u8>;
    /// Atomically swap the dirty flag, returning the previous value.
    fn take_dirty(&mut self) -> bool;
}

/// Map a `vt100::Color` to a ratatui `Color`.
pub(crate) fn vc(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        vt100::Color::Idx(i) => Color::Indexed(i),
        _ => Color::Reset,
    }
}

/// A single pseudo-terminal session driven by an arbitrary command.
pub struct EmbeddedTerminal {
    /// Public so integration tests can inspect the emulated screen; all
    /// in-crate access goes through methods.
    pub parser: Arc<Mutex<Parser>>,
    master: Arc<Mutex<pty::PtyMaster>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    dirty: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
    raw_output: Arc<Mutex<Vec<u8>>>,
    /// Bumped on every `raw_output` mutation; see `PtyChannel::raw_seq`.
    raw_seq: Arc<AtomicU64>,
    exited: Arc<AtomicBool>,
    child: Option<Arc<Mutex<pty::PtyChild>>>,
    scroll_offset: usize,
}

impl EmbeddedTerminal {
    pub fn new(rows: u16, cols: u16, cmd: CommandBuilder, capture_raw: bool) -> Result<Self> {
        let pair = pty::openpty(rows, cols)?;

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let child_handle = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // drop slave so PTY EOF is signalled on Windows when child exits

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 1000)));
        let dirty = Arc::new(AtomicBool::new(false));
        let raw_output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let raw_seq = Arc::new(AtomicU64::new(0));
        let exited = Arc::new(AtomicBool::new(false));

        let parser_c = Arc::clone(&parser);
        let writer_c = Arc::clone(&writer);
        let dirty_c = Arc::clone(&dirty);
        let raw_output_c = Arc::clone(&raw_output);
        let raw_seq_c = Arc::clone(&raw_seq);
        let exited_c = Arc::clone(&exited);

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Carry the last few bytes of the previous chunk so a DSR probe
            // split across two reads is still detected. Kept strictly shorter
            // than the DSR sequence (4 bytes) so a probe counted in the
            // previous chunk can never be counted twice.
            let mut dsr_carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        info!("PTY EOF");
                        exited_c.store(true, Ordering::Release);
                        break;
                    }
                    Ok(n) => {
                        let data = &buf[..n];

                        if let Ok(mut p) = parser_c.lock() {
                            p.process(data);
                        }
                        if capture_raw && let Ok(mut rb) = raw_output_c.lock() {
                            rb.extend_from_slice(data);
                            raw_seq_c.fetch_add(1, Ordering::Release);
                        }
                        dirty_c.store(true, Ordering::Release);

                        // Reply to DSR probes (scanning carry + chunk for splits)
                        let mut scan = std::mem::take(&mut dsr_carry);
                        scan.extend_from_slice(data);
                        let dsr_count = count_dsr(&scan);
                        dsr_carry = scan[scan.len().saturating_sub(3)..].to_vec();
                        if dsr_count > 0 {
                            let (row, col) = if let Ok(p) = parser_c.lock() {
                                let pos = p.screen().cursor_position();
                                (pos.0 + 1, pos.1 + 1)
                            } else {
                                (1, 1)
                            };
                            let reply = format!("\x1b[{};{}R", row, col);
                            if let Ok(mut w) = writer_c.lock() {
                                for _ in 0..dsr_count {
                                    if let Err(e) = w.write_all(reply.as_bytes()) {
                                        debug!("DSR reply write failed: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("PTY read error: {}", e);
                        exited_c.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        });

        let master = Arc::new(Mutex::new(pair.master));
        Ok(Self {
            parser,
            master,
            writer,
            dirty,
            rows,
            cols,
            raw_output,
            raw_seq,
            exited,
            child: Some(Arc::new(Mutex::new(child_handle))),
            scroll_offset: 0,
        })
    }

    /// Spawn an SSH interactive session with the given arguments.
    /// Can be a plain hostname or full args (e.g. "-o StrictHostKeyChecking=no user@ip").
    pub fn ssh_raw(rows: u16, cols: u16, args: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg("-t");
        for arg in split_ssh_args(args) {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        info!("SSH raw session {}x{} args={}", cols, rows, args);
        Self::new(rows, cols, cmd, false)
    }

    /// Spawn an SFTP subsession to `host` (small fixed size, never rendered).
    pub fn sftp(host: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("sftp");
        cmd.arg(host);
        cmd.env("TERM", "dumb");
        info!("SFTP session host={}", host);
        Self::new(200, 220, cmd, true)
    }

    /// Spawn an SSH shell to `host` for browsing (fixed size, parsed not rendered).
    pub fn ssh_shell(host: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("ssh");
        // `-t` must precede the destination: on non-permuting getopt platforms
        // (BSD/macOS) anything after the host is treated as the remote command.
        cmd.arg("-t");
        cmd.arg(host);
        cmd.env("TERM", "dumb");
        info!("SSH shell host={}", host);
        Self::new(200, 220, cmd, true)
    }

    pub fn send_str(&mut self, s: &str) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut w) = self.writer.lock()
            && let Err(e) = w.write_all(s.as_bytes())
        {
            warn!("PTY write failed ({} bytes): {}", s.len(), e);
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        // Hold the parser lock across the whole transaction so the reader
        // thread cannot interleave PTY output (e.g. SIGWINCH redraw replies)
        // between the master resize and our parser resize.
        let Ok(mut p) = self.parser.lock() else {
            return;
        };
        let pty_ok = if let Ok(m) = self.master.lock() {
            m.resize(rows, cols).is_ok()
        } else {
            false
        };
        if pty_ok {
            // ConPTY's RESIZE_QUIRK (and PASSTHROUGH on Win11 22621+) keep the
            // post-resize byte stream well-behaved, so the child shell's own
            // SIGWINCH redraw covers reflow.
            p.screen_mut().set_size(rows, cols);
            self.rows = rows;
            self.cols = cols;
        }
    }

    pub fn mouse_active(&self) -> bool {
        let Ok(p) = self.parser.try_lock() else {
            return false;
        };
        !matches!(p.screen().mouse_protocol_mode(), MouseProtocolMode::None)
    }

    pub fn mouse_wants_motion(&self) -> bool {
        let Ok(p) = self.parser.try_lock() else {
            return false;
        };
        matches!(
            p.screen().mouse_protocol_mode(),
            MouseProtocolMode::AnyMotion
        )
    }

    pub fn app_cursor(&self) -> bool {
        let Ok(p) = self.parser.try_lock() else {
            return false;
        };
        p.screen().application_cursor()
    }

    pub fn alternate_screen(&self) -> bool {
        let Ok(p) = self.parser.try_lock() else {
            return false;
        };
        p.screen().alternate_screen()
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        if let Ok(mut p) = self.parser.lock() {
            let screen = p.screen_mut();
            screen.set_scrollback(self.scroll_offset);
            self.scroll_offset = screen.scrollback();
        }
        self.dirty.store(true, Ordering::Release);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if let Ok(mut p) = self.parser.lock() {
            let screen = p.screen_mut();
            screen.set_scrollback(self.scroll_offset);
            self.scroll_offset = screen.scrollback();
        }
        self.dirty.store(true, Ordering::Release);
    }

    pub fn reset_scroll(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = 0;
            if let Ok(mut p) = self.parser.lock() {
                p.screen_mut().set_scrollback(0);
            }
            self.dirty.store(true, Ordering::Release);
        }
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        let Ok(parser) = self.parser.try_lock() else {
            return;
        };
        let screen = parser.screen();

        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = screen.cell(y, x) {
                    let s = cell.contents();
                    let sym = if s.is_empty() { " " } else { s };
                    if let Some(bc) = buf.cell_mut((area.x + x, area.y + y)) {
                        bc.set_symbol(sym);
                        let mut style = Style::default()
                            .fg(vc(cell.fgcolor()))
                            .bg(vc(cell.bgcolor()));
                        if cell.bold() {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.dim() {
                            style = style.add_modifier(Modifier::DIM);
                        }
                        if cell.italic() {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        if cell.underline() {
                            style = style.add_modifier(Modifier::UNDERLINED);
                        }
                        if cell.inverse() {
                            style = style.add_modifier(Modifier::REVERSED);
                        }
                        bc.set_style(style);
                    }
                }
            }
        }

        if self.scroll_offset == 0 && !screen.hide_cursor() {
            let (cy, cx) = screen.cursor_position();
            let sx = area.x + cx;
            let sy = area.y + cy;
            if sx < area.x + area.width
                && sy < area.y + area.height
                && let Some(bc) = buf.cell_mut((sx, sy))
            {
                let style = bc.style().add_modifier(Modifier::REVERSED);
                bc.set_style(style);
            }
        }
    }

    pub fn cursor_pos(&self) -> Option<(u16, u16)> {
        if self.scroll_offset > 0 {
            return None;
        }
        let Ok(parser) = self.parser.try_lock() else {
            return None;
        };
        if parser.screen().hide_cursor() {
            return None;
        }
        let (cy, cx) = parser.screen().cursor_position();
        Some((cx, cy))
    }

    pub fn raw_lines(&self) -> Vec<String> {
        let Ok(rb) = self.raw_output.lock() else {
            return vec![];
        };
        strip_ansi(&rb)
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    pub fn drain_raw(&self) {
        if let Ok(mut rb) = self.raw_output.lock() {
            rb.clear();
            self.raw_seq.fetch_add(1, Ordering::Release);
        }
    }

    pub fn drain_raw_keep(&self, keep: usize) {
        if let Ok(mut rb) = self.raw_output.lock() {
            let len = rb.len();
            if len > keep {
                rb.drain(..len - keep);
                self.raw_seq.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub fn raw_len(&self) -> usize {
        self.raw_output.lock().map(|rb| rb.len()).unwrap_or(0)
    }

    pub fn raw_seq(&self) -> u64 {
        self.raw_seq.load(Ordering::Acquire)
    }

    /// Non-blocking check whether the child process has exited.
    pub fn process_exited(&self) -> bool {
        if self.exited.load(Ordering::Acquire) {
            return true;
        }
        if let Some(ref child) = self.child
            && let Ok(mut c) = child.lock()
            && let Ok(Some(status)) = c.try_wait()
        {
            debug!("child process exited: {:?}", status);
            self.exited.store(true, Ordering::Release);
            return true;
        }
        false
    }

    pub fn raw_tail(&self, n: usize) -> Vec<u8> {
        let Ok(rb) = self.raw_output.lock() else {
            return vec![];
        };
        let start = rb.len().saturating_sub(n);
        rb[start..].to_vec()
    }

    /// Exit code of the child, if it has exited. `None` while still running.
    pub fn exit_code(&self) -> Option<u32> {
        if let Some(ref child) = self.child
            && let Ok(mut c) = child.lock()
            && let Ok(Some(status)) = c.try_wait()
        {
            return Some(status.code);
        }
        None
    }
}

impl PtyChannel for EmbeddedTerminal {
    fn raw_len(&self) -> usize {
        self.raw_len()
    }
    fn raw_lines(&self) -> Vec<String> {
        self.raw_lines()
    }
    fn raw_seq(&self) -> u64 {
        self.raw_seq()
    }
    fn drain_raw(&self) {
        self.drain_raw()
    }
    fn drain_raw_keep(&self, keep: usize) {
        self.drain_raw_keep(keep)
    }
    fn send_str(&mut self, s: &str) {
        self.send_str(s);
    }
    fn process_exited(&self) -> bool {
        self.process_exited()
    }
    fn exit_code(&self) -> Option<u32> {
        self.exit_code()
    }
    fn raw_tail(&self, n: usize) -> Vec<u8> {
        self.raw_tail(n)
    }
    fn take_dirty(&mut self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        // Move the child handle to a background thread so that the blocking
        // wait() call (from portable_pty's Child drop) doesn't freeze the UI.
        if let Some(child) = self.child.take() {
            thread::spawn(move || {
                if let Ok(mut c) = child.lock() {
                    if let Err(e) = c.kill() {
                        debug!("child kill failed (likely already exited): {}", e);
                    }
                    if let Err(e) = c.wait() {
                        debug!("child wait failed: {}", e);
                    }
                }
                debug!("background child cleanup finished");
            });
        }
    }
}

/// Shared handle for interacting with a `MockPty` from test code.
/// Provides read access to sent commands and write access to the raw buffer.
#[cfg(test)]
#[derive(Clone)]
pub struct MockPtyHandle {
    raw: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    raw_seq: std::rc::Rc<std::cell::Cell<u64>>,
    sent: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    exited: std::rc::Rc<std::cell::Cell<bool>>,
    exit_code: std::rc::Rc<std::cell::Cell<Option<u32>>>,
}

#[cfg(test)]
impl MockPtyHandle {
    /// Append data to the mock's raw buffer (simulates PTY output arriving).
    pub fn feed(&self, data: &[u8]) {
        self.raw.borrow_mut().extend_from_slice(data);
        self.raw_seq.set(self.raw_seq.get() + 1);
    }

    /// Return a snapshot of all commands sent to the PTY so far.
    pub fn sent(&self) -> Vec<String> {
        self.sent.borrow().clone()
    }

    /// Clear the recorded sent commands.
    pub fn clear_sent(&self) {
        self.sent.borrow_mut().clear();
    }

    /// Mark the mock process as exited.
    pub fn set_exited(&self, v: bool) {
        self.exited.set(v);
    }

    /// Set the exit code reported by `PtyChannel::exit_code`.
    pub fn set_exit_code(&self, code: Option<u32>) {
        self.exit_code.set(code);
    }
}

/// Mock PTY for testing browser state machines without a real process.
#[cfg(test)]
pub struct MockPty {
    raw: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    raw_seq: std::rc::Rc<std::cell::Cell<u64>>,
    sent: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    exited: std::rc::Rc<std::cell::Cell<bool>>,
    exit_code: std::rc::Rc<std::cell::Cell<Option<u32>>>,
    pub dirty: bool,
}

#[cfg(test)]
impl MockPty {
    pub fn new() -> (Self, MockPtyHandle) {
        let raw = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let raw_seq = std::rc::Rc::new(std::cell::Cell::new(0));
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let exited = std::rc::Rc::new(std::cell::Cell::new(false));
        let exit_code = std::rc::Rc::new(std::cell::Cell::new(None));
        let handle = MockPtyHandle {
            raw: raw.clone(),
            raw_seq: raw_seq.clone(),
            sent: sent.clone(),
            exited: exited.clone(),
            exit_code: exit_code.clone(),
        };
        let mock = MockPty {
            raw,
            raw_seq,
            sent,
            exited,
            exit_code,
            dirty: false,
        };
        (mock, handle)
    }
}

#[cfg(test)]
impl PtyChannel for MockPty {
    fn raw_len(&self) -> usize {
        self.raw.borrow().len()
    }
    fn raw_lines(&self) -> Vec<String> {
        strip_ansi(&self.raw.borrow())
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }
    fn raw_seq(&self) -> u64 {
        self.raw_seq.get()
    }
    fn drain_raw(&self) {
        self.raw.borrow_mut().clear();
        self.raw_seq.set(self.raw_seq.get() + 1);
    }
    fn drain_raw_keep(&self, keep: usize) {
        let mut rb = self.raw.borrow_mut();
        let len = rb.len();
        if len > keep {
            rb.drain(..len - keep);
            self.raw_seq.set(self.raw_seq.get() + 1);
        }
    }
    fn send_str(&mut self, s: &str) {
        self.sent.borrow_mut().push(s.to_string());
    }
    fn process_exited(&self) -> bool {
        self.exited.get()
    }
    fn exit_code(&self) -> Option<u32> {
        self.exit_code.get()
    }
    fn raw_tail(&self, n: usize) -> Vec<u8> {
        let rb = self.raw.borrow();
        let start = rb.len().saturating_sub(n);
        rb[start..].to_vec()
    }
    fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsr_empty_input() {
        assert_eq!(count_dsr(b""), 0);
    }

    #[test]
    fn dsr_no_match() {
        assert_eq!(count_dsr(b"hello world"), 0);
    }

    #[test]
    fn dsr_single() {
        assert_eq!(count_dsr(b"\x1b[6n"), 1);
    }

    #[test]
    fn dsr_multiple() {
        assert_eq!(count_dsr(b"\x1b[6n\x1b[6n"), 2);
    }

    #[test]
    fn dsr_surrounded_by_text() {
        assert_eq!(count_dsr(b"abc\x1b[6ndef\x1b[6nghi"), 2);
    }

    #[test]
    fn dsr_partial_sequence_not_counted() {
        assert_eq!(count_dsr(b"\x1b[6"), 0);
        assert_eq!(count_dsr(b"\x1b["), 0);
        assert_eq!(count_dsr(b"\x1b"), 0);
    }

    #[test]
    fn dsr_overlapping_bytes() {
        // ESC [ 6 n immediately followed by another ESC — should count the first
        assert_eq!(count_dsr(b"\x1b[6n\x1b"), 1);
    }

    #[test]
    fn dsr_three_back_to_back() {
        assert_eq!(count_dsr(b"\x1b[6n\x1b[6n\x1b[6n"), 3);
    }

    #[test]
    fn dsr_wrong_final_byte() {
        // ESC [ 6 m is NOT a DSR
        assert_eq!(count_dsr(b"\x1b[6m"), 0);
    }

    #[test]
    fn vc_all_indexed_stay_indexed() {
        // vc() always returns Color::Indexed for Idx — the SmartBackend
        // handles the basic-ANSI vs 256-colour distinction at draw time.
        assert_eq!(vc(vt100::Color::Idx(0)), Color::Indexed(0));
        assert_eq!(vc(vt100::Color::Idx(4)), Color::Indexed(4));
        assert_eq!(vc(vt100::Color::Idx(15)), Color::Indexed(15));
        assert_eq!(vc(vt100::Color::Idx(16)), Color::Indexed(16));
        assert_eq!(vc(vt100::Color::Idx(231)), Color::Indexed(231));
        assert_eq!(vc(vt100::Color::Idx(255)), Color::Indexed(255));
    }

    #[test]
    fn vc_rgb_passthrough() {
        assert_eq!(vc(vt100::Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        assert_eq!(
            vc(vt100::Color::Rgb(255, 255, 255)),
            Color::Rgb(255, 255, 255)
        );
    }

    #[test]
    fn vc_default_is_reset() {
        assert_eq!(vc(vt100::Color::Default), Color::Reset);
    }

    // ---- split_ssh_args ----

    #[test]
    fn split_args_plain() {
        assert_eq!(split_ssh_args("user@host"), vec!["user@host"]);
    }

    #[test]
    fn split_args_multiple() {
        assert_eq!(
            split_ssh_args("-o StrictHostKeyChecking=no user@host"),
            vec!["-o", "StrictHostKeyChecking=no", "user@host"]
        );
    }

    #[test]
    fn split_args_double_quoted_value_stays_one_arg() {
        assert_eq!(
            split_ssh_args(r#"-o "ProxyCommand=ssh -W %h:%p jump" host"#),
            vec!["-o", "ProxyCommand=ssh -W %h:%p jump", "host"]
        );
    }

    #[test]
    fn split_args_single_quoted() {
        assert_eq!(
            split_ssh_args("-o 'User=my user' host"),
            vec!["-o", "User=my user", "host"]
        );
    }

    #[test]
    fn split_args_empty_and_whitespace() {
        assert!(split_ssh_args("").is_empty());
        assert!(split_ssh_args("   ").is_empty());
    }

    #[test]
    fn split_args_unterminated_quote_takes_rest() {
        assert_eq!(split_ssh_args("\"a b"), vec!["a b"]);
    }
}
