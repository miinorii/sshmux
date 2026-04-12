use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
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

/// Emit the SGR escape sequence that transitions from `prev` attributes to
/// those of `cell`.  If no change is needed, nothing is written.
fn emit_sgr_diff(out: &mut Vec<u8>, cell: &vt100::Cell, prev: &mut SgrState) {
    let next = SgrState::from_cell(cell);
    if next == *prev {
        return;
    }
    // Full reset + re-apply is simpler and safer than incremental diffs.
    out.extend_from_slice(b"\x1b[0");
    if next.bold {
        out.extend_from_slice(b";1");
    }
    if next.dim {
        out.extend_from_slice(b";2");
    }
    if next.italic {
        out.extend_from_slice(b";3");
    }
    if next.underline {
        out.extend_from_slice(b";4");
    }
    if next.inverse {
        out.extend_from_slice(b";7");
    }
    write_color_param(out, b";38", next.fg);
    write_color_param(out, b";48", next.bg);
    out.push(b'm');
    *prev = next;
}

/// Append the SGR sub-parameters for a foreground or background colour.
fn write_color_param(out: &mut Vec<u8>, prefix: &[u8], color: vt100::Color) {
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            out.extend_from_slice(prefix);
            let _ = write!(out, ";5;{i}");
        }
        vt100::Color::Rgb(r, g, b) => {
            out.extend_from_slice(prefix);
            let _ = write!(out, ";2;{r};{g};{b}");
        }
    }
}

/// Tracked SGR attribute state for diffing.
#[derive(Clone, PartialEq, Eq)]
struct SgrState {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    fg: vt100::Color,
    bg: vt100::Color,
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            fg: vt100::Color::Default,
            bg: vt100::Color::Default,
        }
    }
}

impl SgrState {
    fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
        }
    }
}

/// Emit a single visible row's cell contents (text + SGR) into `out`.
fn snapshot_row(
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
    out: &mut Vec<u8>,
    sgr: &mut SgrState,
) {
    // Find the last column with content.
    let mut last_col: u16 = 0;
    for col in (0..cols).rev() {
        if let Some(cell) = screen.cell(row, col)
            && cell.has_contents()
        {
            last_col = col + if cell.is_wide() { 2 } else { 1 };
            break;
        }
    }

    let mut col = 0u16;
    while col < last_col {
        if let Some(cell) = screen.cell(row, col) {
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
            emit_sgr_diff(out, cell, sgr);
            let s = cell.contents();
            if s.is_empty() {
                out.push(b' ');
            } else {
                out.extend_from_slice(s.as_bytes());
            }
            col += if cell.is_wide() { 2 } else { 1 };
        } else {
            col += 1;
        }
    }
}

/// Read the current screen contents (scrollback + visible rows up to the
/// cursor) as a byte stream of text with SGR attributes — no cursor
/// positioning sequences.  The result can be fed into a `Parser` of any
/// size and will wrap/scroll naturally.
fn snapshot_screen(p: &mut Parser) -> Vec<u8> {
    let (vis_rows, cols) = p.screen().size();
    let (cursor_row, _) = p.screen().cursor_position();
    let vis = usize::from(vis_rows);

    // screen.scrollback() returns the current scroll offset, not the
    // buffer length.  Set to MAX, let the clamp reveal the true size.
    p.screen_mut().set_scrollback(usize::MAX);
    let scrollback_len = p.screen().scrollback();
    p.screen_mut().set_scrollback(0);

    // Total rows of content: all scrollback + visible rows up to cursor.
    let total = scrollback_len + usize::from(cursor_row) + 1;

    let mut out = Vec::new();
    let mut sgr = SgrState::default();
    let mut first = true;

    // Page through scrollback using set_scrollback.  Each call to
    // set_scrollback(offset) maps `cell(0..vis_rows)` to a window:
    //   scrollback[sb_len - offset ..] ++ rows[.. vis - offset]
    // We slide this window from the oldest scrollback to the live screen.
    // `row_wrapped(r)` means row r's content continues on row r+1 without
    // a logical newline.  So we emit `\r\n` before row r only when the
    // PREVIOUS row was NOT wrapped.
    let mut prev_wrapped = false;

    if scrollback_len > 0 {
        let sb_rows_to_read = scrollback_len.min(total);

        let mut emitted: usize = 0;
        while emitted < sb_rows_to_read {
            let offset = scrollback_len - emitted;
            let page_own = offset.min(vis);
            p.screen_mut().set_scrollback(offset);

            for local_row in 0..page_own {
                if emitted + local_row >= sb_rows_to_read {
                    break;
                }
                let r = local_row as u16;
                if !first && !prev_wrapped {
                    if sgr != SgrState::default() {
                        out.extend_from_slice(b"\x1b[0m");
                        sgr = SgrState::default();
                    }
                    out.extend_from_slice(b"\r\n");
                }
                first = false;
                snapshot_row(p.screen(), r, cols, &mut out, &mut sgr);
                prev_wrapped = p.screen().row_wrapped(r);
            }
            emitted += page_own;
        }
        p.screen_mut().set_scrollback(0);
    }

    // Visible rows up to and including the cursor row.
    let visible_end = usize::from(cursor_row) + 1;
    for row in 0..visible_end {
        let r = row as u16;
        if !first && !prev_wrapped {
            if sgr != SgrState::default() {
                out.extend_from_slice(b"\x1b[0m");
                sgr = SgrState::default();
            }
            out.extend_from_slice(b"\r\n");
        }
        first = false;
        snapshot_row(p.screen(), r, cols, &mut out, &mut sgr);
        prev_wrapped = p.screen().row_wrapped(r);
    }

    if sgr != SgrState::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

/// Snapshot the current parser state and rebuild it at `(new_rows, new_cols)`.
///
/// Reads every visible cell (scrollback + screen up to the cursor) and emits
/// just text + SGR attributes into a fresh parser at the new dimensions.
/// Lines wrap naturally at the new width — no cursor-positioning escapes that
/// could become stale at a different size.
fn snapshot_resize(p: &mut Parser, new_rows: u16, new_cols: u16) -> Parser {
    let snapshot = snapshot_screen(p);
    let mut fresh = Parser::new(new_rows, new_cols, 1000);
    fresh.process(&snapshot);
    fresh
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
    /// Timestamp-based suppression of ConPTY repaint data after a
    /// resize. On Windows, ConPTY sends escape sequences to repaint the
    /// screen after a resize; these conflict with our snapshot content
    /// and cause garbling. The reader thread discards data that arrives
    /// within a short window after the resize.
    suppress_until: Arc<Mutex<Option<Instant>>>,
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
        let suppress_until = Arc::new(Mutex::new(None::<Instant>));

        let parser_c = Arc::clone(&parser);
        let writer_c = Arc::clone(&writer);
        let dirty_c = Arc::clone(&dirty);
        let raw_output_c = Arc::clone(&raw_output);
        let exited_c = Arc::clone(&exited);
        let suppress_c = Arc::clone(&suppress_until);

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

                        // After a snapshot-resize, ConPTY sends repaint
                        // data that conflicts with our snapshot content.
                        // Discard data within the suppression window.
                        if let Ok(mut guard) = suppress_c.lock()
                            && let Some(deadline) = *guard
                        {
                            if Instant::now() < deadline {
                                debug!("suppressing {} bytes of ConPTY repaint", n);
                                dirty_c.store(true, Ordering::Release);
                                continue;
                            }
                            *guard = None;
                        }

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
            suppress_until,
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
        // Hold the parser lock across the whole transaction so the reader
        // thread cannot interleave PTY output (e.g. SIGWINCH redraw replies)
        // between the master resize and our parser resize.
        let Ok(mut p) = self.parser.lock() else {
            return;
        };
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
            if !p.screen().alternate_screen() {
                // Snapshot approach: read the current screen state (text +
                // SGR attributes) and replay it into a fresh parser at the
                // new dimensions.  Lines re-wrap naturally — no stale cursor
                // positioning escapes that would break at a different width.
                *p = snapshot_resize(&mut p, rows, cols);
                // On Windows, ConPTY sends repaint escape sequences after
                // a resize.  These conflict with our snapshot content and
                // cause garbled text on repeated resizes.  Suppress
                // reader-thread processing for a short window.
                if cfg!(windows)
                    && let Ok(mut guard) = self.suppress_until.lock()
                {
                    *guard =
                        Some(Instant::now() + std::time::Duration::from_millis(100));
                }
            } else {
                // Alternate screen (vim, htop, …) — the app redraws itself
                // after SIGWINCH so plain set_size is correct.
                p.screen_mut().set_size(rows, cols);
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

    // ---- snapshot_resize -------------------------------------------------------

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

    /// Build the byte stream for N labelled rows: "R00\r\nR01\r\n…\r\nR{N-1}"
    fn row_bytes(n: u8) -> Vec<u8> {
        let mut input = String::new();
        for i in 0..n {
            if i > 0 {
                input.push_str("\r\n");
            }
            input.push_str(&format!("R{:02}", i));
        }
        input.into_bytes()
    }

    // -- Bug reproduction: set_size vs snapshot_resize --
    //
    // set_size truncates rows/columns in place, which silently loses content
    // (right side of long lines, cursor row on vertical shrink).
    // snapshot_resize reads the screen cell-by-cell and replays pure text +
    // SGR into a fresh parser, so lines wrap naturally at the new width.

    #[test]
    fn bug_set_size_horizontal_shrink_loses_wrapped_text() {
        // A line wider than the new column count is silently truncated by
        // set_size.  snapshot_resize wraps it onto multiple rows.
        let mut p = Parser::new(10, 80, 1000);
        p.process(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef");

        // Old approach — set_size truncates: only first 20 chars survive.
        let mut old = Parser::new(10, 80, 1000);
        old.process(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef");
        old.screen_mut().set_size(10, 20);
        assert_eq!(row_text(&old, 0), "ABCDEFGHIJKLMNOPQRST");
        assert_eq!(row_text(&old, 1), ""); // remainder is gone

        // Snapshot approach wraps the full content across rows.
        let fresh = snapshot_resize(&mut p, 10, 20);
        assert_eq!(row_text(&fresh, 0), "ABCDEFGHIJKLMNOPQRST");
        assert_eq!(row_text(&fresh, 1), "UVWXYZ0123456789abcd");
        assert_eq!(row_text(&fresh, 2), "ef");
    }

    #[test]
    fn bug_set_size_vertical_shrink_loses_cursor_line() {
        // 10 rows of content, cursor at bottom, shrink to 5.
        // set_size truncates from the bottom — destroying the cursor row.
        let data = row_bytes(10);

        let mut old = Parser::new(10, 10, 1000);
        old.process(&data);
        old.screen_mut().set_size(5, 10);
        // BUG: set_size keeps rows 0..4, cursor row (9) is gone.
        assert_eq!(row_text(&old, 0), "R00");
        assert_eq!(row_text(&old, 4), "R04");

        // Snapshot approach: cursor row is preserved.
        let mut p = Parser::new(10, 10, 1000);
        p.process(&data);
        let fresh = snapshot_resize(&mut p, 5, 10);
        assert_eq!(row_text(&fresh, 4), "R09");
        assert_eq!(fresh.screen().cursor_position().0, 4);
    }

    #[test]
    fn bug_set_size_prompt_duplication() {
        // Simulate an interactive session: MOTD + commands + prompts.
        // Readline escape sequences (cursor movement, line clearing) in the
        // raw byte stream are width-specific.  Replaying raw bytes at a
        // different width garbles the display.  Snapshot reads the final
        // screen state — no stale escape sequences.
        let mut p = Parser::new(16, 80, 1000);
        // Initial MOTD + prompt.
        p.process(b"Welcome to Linux\r\n");
        p.process(b"Last login: Sun Apr 12\r\n");
        p.process(b"user@host:~$ ");
        // User ran ls, got output, then a new prompt.
        p.process(b"ls -lah\r\n");
        p.process(b"drwxr-xr-x  2 user user 4.0K file1\r\n");
        p.process(b"drwxr-xr-x  2 user user 4.0K file2\r\n");
        p.process(b"user@host:~$ ");
        // User pressed Enter a few times (empty commands).
        p.process(b"\r\nuser@host:~$ ");
        p.process(b"\r\nuser@host:~$ ");

        // Horizontal shrink via snapshot: each prompt should appear exactly
        // where it was — no duplication, no stale lines.
        let fresh = snapshot_resize(&mut p, 16, 40);
        let rows: Vec<String> = (0..16).map(|r| row_text(&fresh, r)).collect();
        // 4 lines contain "user@host": the ls command line + 3 prompts.
        let prompt_count = rows.iter().filter(|r| r.contains("user@host")).count();
        assert_eq!(
            prompt_count, 4,
            "expected exactly 4 user@host lines, got: {rows:?}"
        );
    }

    // -- snapshot_resize correctness tests --

    #[test]
    fn snapshot_shrink_cursor_at_bottom_preserves_prompt() {
        let mut p = Parser::new(10, 10, 1000);
        p.process(&row_bytes(10));
        let fresh = snapshot_resize(&mut p, 5, 10);

        assert_eq!(fresh.screen().size(), (5, 10));
        assert_eq!(row_text(&fresh, 0), "R05");
        assert_eq!(row_text(&fresh, 4), "R09");
        assert_eq!(fresh.screen().cursor_position().0, 4);
    }

    #[test]
    fn snapshot_expand_then_shrink_preserves_all_content() {
        let data = row_bytes(10);
        let mut p = Parser::new(10, 10, 1000);
        p.process(&data);

        // Expand to 20 rows then back to 10 — all content preserved.
        let mut expanded = snapshot_resize(&mut p, 20, 10);
        assert_eq!(expanded.screen().cursor_position(), (9, 3));

        let shrunk = snapshot_resize(&mut expanded, 10, 10);
        assert_eq!(shrunk.screen().size(), (10, 10));
        assert_eq!(row_text(&shrunk, 0), "R00");
        assert_eq!(row_text(&shrunk, 9), "R09");
        assert_eq!(shrunk.screen().cursor_position(), (9, 3));
    }

    #[test]
    fn snapshot_shrink_cursor_already_in_bounds() {
        let mut p = Parser::new(20, 10, 1000);
        p.process(&row_bytes(5));
        let fresh = snapshot_resize(&mut p, 10, 10);

        assert_eq!(fresh.screen().size(), (10, 10));
        assert_eq!(row_text(&fresh, 0), "R00");
        assert_eq!(row_text(&fresh, 4), "R04");
        assert_eq!(row_text(&fresh, 5), "");
        assert_eq!(fresh.screen().cursor_position(), (4, 3));
    }

    #[test]
    fn snapshot_shrink_cols_no_duplication() {
        let mut p = Parser::new(10, 40, 1000);
        p.process(&row_bytes(10));
        let fresh = snapshot_resize(&mut p, 10, 20);

        assert_eq!(fresh.screen().size(), (10, 20));
        for r in 0..10u16 {
            assert_eq!(row_text(&fresh, r), format!("R{:02}", r));
        }
        assert_eq!(fresh.screen().cursor_position(), (9, 3));
    }

    #[test]
    fn snapshot_shrink_cols_wraps_long_lines() {
        let mut p = Parser::new(10, 40, 1000);
        p.process(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd");
        let fresh = snapshot_resize(&mut p, 10, 20);

        assert_eq!(row_text(&fresh, 0), "ABCDEFGHIJKLMNOPQRST");
        assert_eq!(row_text(&fresh, 1), "UVWXYZ0123456789abcd");
    }

    #[test]
    fn snapshot_preserves_colors() {
        // Bold red "hello" — verify attributes survive snapshot + replay.
        let mut p = Parser::new(5, 20, 1000);
        p.process(b"\x1b[1;31mhello\x1b[0m");
        let fresh = snapshot_resize(&mut p, 5, 20);

        let cell = fresh.screen().cell(0, 0).unwrap();
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
        assert_eq!(row_text(&fresh, 0), "hello");
    }

    #[test]
    fn snapshot_shrink_cursor_one_past_boundary() {
        let mut p = Parser::new(10, 10, 1000);
        p.process(&row_bytes(6));
        let fresh = snapshot_resize(&mut p, 5, 10);

        assert_eq!(fresh.screen().size(), (5, 10));
        assert_eq!(row_text(&fresh, 0), "R01"); // R00 in scrollback
        assert_eq!(row_text(&fresh, 4), "R05");
        assert_eq!(fresh.screen().cursor_position().0, 4);
    }

    #[test]
    fn snapshot_includes_scrollback() {
        // Fill 10 rows in a 5-row terminal → 5 rows scroll into scrollback.
        let mut p = Parser::new(5, 10, 1000);
        p.process(&row_bytes(10));
        // Visible: R05..R09.  Scrollback: R00..R04.

        // Resize to 8 rows: snapshot should pull scrollback content back
        // onto the visible screen.
        let fresh = snapshot_resize(&mut p, 8, 10);
        assert_eq!(row_text(&fresh, 0), "R02"); // R00,R01 still in scrollback
        assert_eq!(row_text(&fresh, 7), "R09");
    }

    #[test]
    fn snapshot_multiple_resizes_preserve_content() {
        // Simulate: 16-row terminal, ls output, then multiple resizes.
        let mut p = Parser::new(16, 80, 1000);
        p.process(b"ls -lah\r\n");
        for i in 0..14 {
            p.process(format!("file{i:02}\r\n").as_bytes());
        }
        p.process(b"prompt> ");

        // 1st resize: shrink to 8 rows.
        let mut p = snapshot_resize(&mut p, 8, 80);
        assert_eq!(row_text(&p, 7), "prompt>");

        // 2nd resize: expand back to 16.
        let mut p = snapshot_resize(&mut p, 16, 80);
        assert!(
            (0..16).map(|r| row_text(&p, r)).any(|r| r == "file00"),
            "file00 should survive expand"
        );
        assert_eq!(row_text(&p, 15), "prompt>");

        // 3rd resize: shrink to 5.
        let p = snapshot_resize(&mut p, 5, 80);
        assert_eq!(row_text(&p, 4), "prompt>");
    }

    #[test]
    fn snapshot_multiple_horizontal_resizes() {
        // Multiple horizontal resizes back and forth.
        // Use contents() to check preservation because words naturally
        // split across rows at narrower widths.
        let mut p = Parser::new(10, 80, 1000);
        p.process(b"user@host:~$ ls -lah\r\n");
        p.process(b"-rw-r--r--  1 user user   66 Jan 26  2022 .bash_history\r\n");
        p.process(b"drwxr-xr-x  3 user user 4.0K Feb 23  2023 projects\r\n");
        p.process(b"-rw-------  1 user user    5 Apr 12 00:56 notes.txt\r\n");
        p.process(b"user@host:~$ ");

        // Shrink to 50 cols.
        let mut p = snapshot_resize(&mut p, 10, 50);
        let c = p.screen().contents();
        assert!(c.contains("projects"), "projects 80→50: {c}");
        assert!(c.contains(".bash_history"), ".bash_history 80→50: {c}");

        // Shrink to 40 cols.
        let mut p = snapshot_resize(&mut p, 10, 40);
        let c = p.screen().contents();
        assert!(c.contains("projects"), "projects 50→40: {c}");
        assert!(c.contains(".bash_history"), ".bash_history 50→40: {c}");

        // Expand back to 80 cols.
        let mut p = snapshot_resize(&mut p, 10, 80);
        let c = p.screen().contents();
        assert!(c.contains("projects"), "projects 40→80: {c}");
        assert!(c.contains(".bash_history"), ".bash_history 40→80: {c}");
        assert!(c.contains("notes.txt"), "notes.txt 40→80: {c}");

        // Shrink again to 50.
        let mut p = snapshot_resize(&mut p, 10, 50);
        let c = p.screen().contents();
        assert!(c.contains("projects"), "projects 80→50 (2nd): {c}");
        assert!(c.contains("notes.txt"), "notes.txt 80→50 (2nd): {c}");

        // And back to 80.
        let p = snapshot_resize(&mut p, 10, 80);
        let c = p.screen().contents();
        assert!(c.contains("projects"), "projects final 80: {c}");
        assert!(c.contains(".bash_history"), ".bash_history final 80: {c}");
        assert!(c.contains("notes.txt"), "notes.txt final 80: {c}");
        assert!(c.contains("user@host:~$"), "prompt final 80: {c}");
    }

    #[test]
    fn snapshot_multiple_resizes_with_scrollback() {
        // When content is pushed into scrollback by narrow resize,
        // subsequent resizes must preserve it through scrollback paging.
        // Use snapshot_screen (not contents()) to verify full content
        // since contents() only returns visible rows.
        let mut p = Parser::new(5, 80, 1000);
        p.process(b"line1: first\r\n");
        p.process(b"line2: second\r\n");
        p.process(b"line3: third\r\n");
        p.process(b"line4: fourth\r\n");
        p.process(b"line5: fifth\r\n");
        p.process(b"line6: sixth\r\n");
        p.process(b"line7: seventh\r\n");
        p.process(b"prompt> ");

        let check = |p: &mut Parser, label: &str| {
            let snap = snapshot_screen(p);
            let s = String::from_utf8_lossy(&snap);
            assert!(s.contains("first"), "{label}: missing 'first' in:\n{s}");
            assert!(s.contains("seventh"), "{label}: missing 'seventh' in:\n{s}");
            assert!(s.contains("prompt>"), "{label}: missing 'prompt>' in:\n{s}");
        };

        check(&mut p, "initial");

        let mut p = snapshot_resize(&mut p, 5, 20);
        check(&mut p, "80→20");

        let mut p = snapshot_resize(&mut p, 5, 15);
        check(&mut p, "20→15");

        let mut p = snapshot_resize(&mut p, 5, 80);
        check(&mut p, "15→80");

        let mut p = snapshot_resize(&mut p, 5, 20);
        check(&mut p, "80→20 (2nd)");

        let mut p = snapshot_resize(&mut p, 5, 80);
        check(&mut p, "20→80 (final)");
    }

    #[test]
    fn snapshot_wrap_across_page_boundary() {
        // A wrapped row at the boundary between two scrollback pages must
        // NOT get a spurious \r\n.  Use a 3-row terminal so pages are small.
        let mut p = Parser::new(3, 10, 1000);
        // A 20-char line wraps into two 10-char rows at width 10.
        p.process(b"ABCDEFGHIJKLMNOPQRST\r\n");
        // Push more content to move the wrapped pair into scrollback.
        p.process(b"line1\r\n");
        p.process(b"line2\r\n");
        p.process(b"line3\r\n");
        p.process(b"end");

        let fresh = snapshot_resize(&mut p, 6, 20);
        let rows: Vec<String> = (0..6).map(|r| row_text(&fresh, r)).collect();
        eprintln!("RESULT: {:?}", rows);
        // The 20-char line should appear intact at width 20.
        assert!(
            rows.iter().any(|r| r == "ABCDEFGHIJKLMNOPQRST"),
            "20-char line should be intact: {rows:?}"
        );
    }

    #[test]
    fn snapshot_wrapped_scrollback_merges_continuations() {
        // Simulate the real bug: narrow terminal (50 cols) where MOTD lines
        // wrap.  Wrapped continuations go into scrollback.  The snapshot
        // must rejoin wrapped rows so they re-wrap naturally at the new width.
        let mut p = Parser::new(10, 50, 1000);
        // This 75-char line wraps into two rows at 50 cols:
        //   Row 0: "Linux host-abcd1234 6.1.0-40-cloud-amd64 #1 SMP Li" (50)
        //   Row 1: "nux (2025-09-20) x86_64" (wrapped continuation)
        p.process(b"Linux host-abcd1234 6.1.0-40-cloud-amd64 #1 SMP Linux (2025-09-20) x86_64\r\n");
        // Another long line:
        p.process(b"This system comes with ABSOLUTELY NO WARRANTY.\r\n");
        // Fill enough to push wrapped rows into scrollback.
        for i in 0..10 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        p.process(b"prompt> ");

        let snapshot = snapshot_screen(&mut p);
        let snap_text = String::from_utf8_lossy(&snapshot);
        eprintln!("SNAPSHOT:\n{}", snap_text);

        // Resize to 80 cols.  The original 75-char line should now fit on
        // one row — not be split as two lines with \r\n between them.
        let fresh = snapshot_resize(&mut p, 10, 80);
        let rows: Vec<String> = (0..10).map(|r| row_text(&fresh, r)).collect();
        eprintln!("RESULT:");
        for (i, r) in rows.iter().enumerate() {
            eprintln!("  [{:2}] {}", i, r);
        }

        // The 75-char MOTD line, if visible, should appear as one row.
        // At minimum, no single-char rows should exist.
        for (i, row) in rows.iter().enumerate() {
            assert!(
                row.is_empty() || row.len() > 1,
                "row {i} is a single char: {row:?}\nall rows: {rows:?}"
            );
        }
    }

    // Alternate screen uses set_size, not snapshot — verify that path still
    // works for the EmbeddedTerminal::resize() branch.
    #[test]
    fn alternate_screen_uses_set_size_not_snapshot() {
        let mut p = Parser::new(10, 10, 1000);
        p.process(b"\x1b[?1049h"); // enter alternate screen
        p.process(&row_bytes(10));

        // Simulate what EmbeddedTerminal::resize does for alternate screen.
        p.screen_mut().set_size(5, 10);

        assert_eq!(p.screen().size(), (5, 10));
        // set_size truncates from bottom — top rows visible (app will redraw).
        assert_eq!(row_text(&p, 0), "R00");
        assert_eq!(row_text(&p, 4), "R04");
    }

}
