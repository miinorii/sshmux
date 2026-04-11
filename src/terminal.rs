use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use anyhow::Result;
use log::{debug, error, info};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::browser::parse::strip_ansi;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use vt100::{MouseProtocolMode, Parser};

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
    fn drain_raw(&self);
    fn send_str(&mut self, s: &str);
    fn send_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.send_str(s);
    }
    fn process_exited(&self) -> bool;
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

/// Resize a `vt100::Parser` to `(new_rows, new_cols)`, preserving content near
/// the cursor when shrinking height on the main screen.
///
/// `vt100::Screen::set_size()` naively truncates rows from the bottom of the
/// screen grid, which destroys content at the cursor position. Real terminals
/// push top rows into the scrollback buffer so the cursor stays visible. This
/// function replicates that behaviour: it pre-scrolls by exactly as many lines
/// as needed to bring the cursor within the new bounds, then calls `set_size`.
/// On alternate-screen (vim, htop, …) the app redraws itself after SIGWINCH, so
/// the default truncation is fine and no pre-scroll is applied.
fn resize_parser(p: &mut Parser, new_rows: u16, new_cols: u16) {
    let (old_rows, _) = p.screen().size();
    if new_rows < old_rows && !p.screen().alternate_screen() {
        let cursor_row = p.screen().cursor_position().0;
        if cursor_row >= new_rows {
            let scroll_by = cursor_row - new_rows + 1;
            p.process(format!("\x1b[{}S", scroll_by).as_bytes());
        }
    }
    p.screen_mut().set_size(new_rows, new_cols);
}

/// A single pseudo-terminal session driven by an arbitrary command.
pub struct EmbeddedTerminal {
    pub parser: Arc<Mutex<Parser>>,
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub dirty: Arc<AtomicBool>,
    pub rows: u16,
    pub cols: u16,
    pub raw_output: Arc<Mutex<Vec<u8>>>,
    pub exited: Arc<AtomicBool>,
    pub child: Option<Arc<Mutex<Box<dyn Child + Send + Sync>>>>,
    pub scroll_offset: usize,
}

impl EmbeddedTerminal {
    pub fn new(rows: u16, cols: u16, cmd: CommandBuilder, capture_raw: bool) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let child_handle = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // drop slave so PTY EOF is signalled on Windows when child exits

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 1000)));
        let dirty = Arc::new(AtomicBool::new(false));
        let raw_output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let exited = Arc::new(AtomicBool::new(false));

        let parser_c = Arc::clone(&parser);
        let writer_c = Arc::clone(&writer);
        let dirty_c = Arc::clone(&dirty);
        let raw_output_c = Arc::clone(&raw_output);
        let exited_c = Arc::clone(&exited);

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
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
                        }
                        dirty_c.store(true, Ordering::Release);

                        // Reply to DSR probes
                        let dsr_count = count_dsr(data);
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
                                    let _ = w.write_all(reply.as_bytes());
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
        for arg in args.split_whitespace() {
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
        cmd.arg(host);
        cmd.arg("-t");
        cmd.env("TERM", "dumb");
        info!("SSH shell host={}", host);
        Self::new(200, 220, cmd, true)
    }

    pub fn send_str(&mut self, s: &str) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(s.as_bytes());
        }
    }

    pub fn send_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.send_str(c.encode_utf8(&mut buf));
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        let pty_ok = if let Ok(m) = self.master.lock() {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok()
        } else {
            false
        };
        if pty_ok {
            if let Ok(mut p) = self.parser.lock() {
                resize_parser(&mut p, rows, cols);
            }
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
        }
    }

    pub fn raw_len(&self) -> usize {
        self.raw_output.lock().map(|rb| rb.len()).unwrap_or(0)
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
}

impl PtyChannel for EmbeddedTerminal {
    fn raw_len(&self) -> usize {
        self.raw_len()
    }
    fn raw_lines(&self) -> Vec<String> {
        self.raw_lines()
    }
    fn drain_raw(&self) {
        self.drain_raw()
    }
    fn send_str(&mut self, s: &str) {
        self.send_str(s);
    }
    fn send_char(&mut self, c: char) {
        self.send_char(c);
    }
    fn process_exited(&self) -> bool {
        self.process_exited()
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
                    let _ = c.kill();
                    let _ = c.wait();
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
    sent: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    exited: std::rc::Rc<std::cell::Cell<bool>>,
}

#[cfg(test)]
impl MockPtyHandle {
    /// Append data to the mock's raw buffer (simulates PTY output arriving).
    pub fn feed(&self, data: &[u8]) {
        self.raw.borrow_mut().extend_from_slice(data);
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
}

/// Mock PTY for testing browser state machines without a real process.
#[cfg(test)]
pub struct MockPty {
    raw: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    sent: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    exited: std::rc::Rc<std::cell::Cell<bool>>,
    pub dirty: bool,
}

#[cfg(test)]
impl MockPty {
    pub fn new() -> (Self, MockPtyHandle) {
        let raw = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let exited = std::rc::Rc::new(std::cell::Cell::new(false));
        let handle = MockPtyHandle {
            raw: raw.clone(),
            sent: sent.clone(),
            exited: exited.clone(),
        };
        let mock = MockPty {
            raw,
            sent,
            exited,
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
    fn drain_raw(&self) {
        self.raw.borrow_mut().clear();
    }
    fn send_str(&mut self, s: &str) {
        self.sent.borrow_mut().push(s.to_string());
    }
    fn process_exited(&self) -> bool {
        self.exited.get()
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

    // ---- resize_parser -------------------------------------------------------

    /// Read the non-blank text content of a screen row, trimmed of trailing spaces.
    fn row_text(p: &Parser, row: u16) -> String {
        let (_, cols) = p.screen().size();
        let mut s = String::new();
        for c in 0..cols {
            if let Some(cell) = p.screen().cell(row, c) {
                let contents = cell.contents();
                s.push_str(if contents.is_empty() { " " } else { contents });
            }
        }
        s.trim_end().to_string()
    }

    /// Fill a parser with N labelled rows: "R00\r\nR01\r\n…\r\nR{N-1}"
    /// (no trailing newline so the cursor lands at the end of the last row).
    fn fill_rows(p: &mut Parser, n: u8) {
        let mut input = String::new();
        for i in 0..n {
            if i > 0 {
                input.push_str("\r\n");
            }
            input.push_str(&format!("R{:02}", i));
        }
        p.process(input.as_bytes());
    }

    // Shrinking when the cursor is at the very bottom row must push top rows
    // into scrollback so the prompt stays visible.
    #[test]
    fn resize_shrink_cursor_at_bottom_preserves_prompt() {
        let mut p = Parser::new(10, 10, 1000);
        fill_rows(&mut p, 10); // R00..R09, cursor at (9, 3)

        assert_eq!(p.screen().cursor_position(), (9, 3));

        resize_parser(&mut p, 5, 10);

        assert_eq!(p.screen().size(), (5, 10));
        // Top rows pushed into scrollback; the bottom 5 originals now fill the screen.
        assert_eq!(row_text(&p, 0), "R05");
        assert_eq!(row_text(&p, 4), "R09");
        // Cursor clamped to the last visible row.
        assert_eq!(p.screen().cursor_position().0, 4);
    }

    // Expanding then shrinking back must not lose content: after the expand the
    // cursor is well above the new bottom, so zero pre-scroll is needed and only
    // the blank rows added during the expand are removed.
    #[test]
    fn resize_expand_then_shrink_preserves_all_content() {
        let mut p = Parser::new(10, 10, 1000);
        fill_rows(&mut p, 10); // cursor at (9, 3)

        resize_parser(&mut p, 20, 10); // expand: 10 blank rows appended, cursor stays at (9, 3)
        assert_eq!(p.screen().cursor_position(), (9, 3));

        resize_parser(&mut p, 10, 10); // shrink back: cursor (9) < new_rows (10), no pre-scroll
        assert_eq!(p.screen().size(), (10, 10));
        // All original rows must still be present.
        assert_eq!(row_text(&p, 0), "R00");
        assert_eq!(row_text(&p, 9), "R09");
        assert_eq!(p.screen().cursor_position(), (9, 3));
    }

    // When the cursor is already within the new height no pre-scroll should
    // happen; set_size only drops the blank rows below the cursor.
    #[test]
    fn resize_shrink_cursor_already_in_bounds_no_scroll() {
        let mut p = Parser::new(20, 10, 1000);
        fill_rows(&mut p, 5); // rows 0..4 filled, rows 5..19 blank, cursor at (4, 3)

        resize_parser(&mut p, 10, 10);

        assert_eq!(p.screen().size(), (10, 10));
        // Rows 0..4 intact, rows 5..9 blank.
        assert_eq!(row_text(&p, 0), "R00");
        assert_eq!(row_text(&p, 4), "R04");
        assert_eq!(row_text(&p, 5), "");
        assert_eq!(p.screen().cursor_position(), (4, 3));
    }

    // Alternate screen (vim/htop) must NOT trigger the pre-scroll: those apps
    // redraw the whole screen after SIGWINCH, so the default set_size truncation
    // is correct and we must not disturb it.
    #[test]
    fn resize_alternate_screen_skips_prescroll() {
        let mut p = Parser::new(10, 10, 1000);
        p.process(b"\x1b[?1049h"); // enter alternate screen
        fill_rows(&mut p, 10); // cursor at (9, 3) on alternate screen

        resize_parser(&mut p, 5, 10);

        assert_eq!(p.screen().size(), (5, 10));
        // No pre-scroll: top rows are visible, bottom rows were truncated.
        assert_eq!(row_text(&p, 0), "R00");
        assert_eq!(row_text(&p, 4), "R04");
    }

    // Cursor sitting exactly one row below the new bottom boundary: scroll by
    // exactly 1 to bring it into view.
    #[test]
    fn resize_shrink_cursor_one_past_boundary() {
        let mut p = Parser::new(10, 10, 1000);
        fill_rows(&mut p, 6); // rows 0..5 filled, cursor at (5, 3)

        resize_parser(&mut p, 5, 10); // new_rows=5, cursor_row=5 → scroll_by=1

        assert_eq!(p.screen().size(), (5, 10));
        assert_eq!(row_text(&p, 0), "R01"); // R00 pushed to scrollback
        assert_eq!(row_text(&p, 4), "R05");
        assert_eq!(p.screen().cursor_position().0, 4);
    }
}
