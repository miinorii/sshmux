use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use anyhow::Result;
use log::debug;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

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

/// A single pseudo-terminal session driven by an arbitrary command.
pub struct EmbeddedTerminal {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub dirty: Arc<AtomicBool>,
    pub mouse_active: Arc<AtomicBool>,
    pub app_cursor: Arc<AtomicBool>,
    pub cursor_visible: Arc<AtomicBool>,
    pub rows: u16,
    pub cols: u16,
    pub raw_output: Arc<Mutex<Vec<u8>>>,
    pub exited: Arc<AtomicBool>,
    pub child: Option<Arc<Mutex<Box<dyn Child + Send + Sync>>>>,
}

impl EmbeddedTerminal {
    pub fn new(
        rows: u16,
        cols: u16,
        cmd: CommandBuilder,
    ) -> Result<Self> {
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let dirty = Arc::new(AtomicBool::new(false));
        let mouse_active = Arc::new(AtomicBool::new(false));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let cursor_visible = Arc::new(AtomicBool::new(true));
        let raw_output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let exited = Arc::new(AtomicBool::new(false));

        let parser_c = Arc::clone(&parser);
        let writer_c = Arc::clone(&writer);
        let dirty_c = Arc::clone(&dirty);
        let mouse_active_c = Arc::clone(&mouse_active);
        let app_cursor_c = Arc::clone(&app_cursor);
        let cursor_visible_c = Arc::clone(&cursor_visible);
        let raw_output_c = Arc::clone(&raw_output);
        let exited_c = Arc::clone(&exited);

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("PTY EOF");
                        exited_c.store(true, Ordering::Release);
                        break;
                    }
                    Ok(n) => {
                        let data = &buf[..n];

                        if let Ok(mut p) = parser_c.lock() {
                            p.process(data);
                        }
                        if let Ok(mut rb) = raw_output_c.lock() {
                            rb.extend_from_slice(data);
                        }
                        dirty_c.store(true, Ordering::Release);

                        // Build scan buffer: leftover incomplete sequence from the
                        // previous read (if any) prepended to the new data.
                        let mut scan = std::mem::take(&mut carry);
                        scan.extend_from_slice(data);

                        // Scan for DEC private mode set/reset sequences (ESC [ ? ... h/l).
                        // Incomplete sequences at the end of the buffer are saved in
                        // `carry` and prepended to the next read so nothing is missed.
                        let mut i = 0;
                        while i < scan.len() {
                            if scan[i] != 0x1b {
                                i += 1;
                                continue;
                            }
                            if i + 1 >= scan.len() {
                                carry = scan[i..].to_vec();
                                break;
                            }
                            if scan[i + 1] != b'[' {
                                i += 2;
                                continue;
                            }
                            if i + 2 >= scan.len() {
                                carry = scan[i..].to_vec();
                                break;
                            }
                            if scan[i + 2] != b'?' {
                                i += 3;
                                continue;
                            }
                            let start = i + 3;
                            let mut end = start;
                            while end < scan.len() && scan[end] != b'h' && scan[end] != b'l' {
                                end += 1;
                            }
                            if end >= scan.len() {
                                carry = scan[i..].to_vec();
                                break;
                            }
                            if let Ok(params) = std::str::from_utf8(&scan[start..end]) {
                                let set = scan[end] == b'h';
                                for param in params.split(';') {
                                    match param.trim() {
                                        "1" => {
                                            app_cursor_c.store(set, Ordering::Release);
                                        }
                                        "1000" | "1002" | "1003" | "1006" => {
                                            mouse_active_c.store(set, Ordering::Release);
                                        }
                                        "25" => {
                                            cursor_visible_c.store(set, Ordering::Release);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            i = end + 1;
                        }

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
                        debug!("PTY error: {}", e);
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
            mouse_active,
            app_cursor,
            cursor_visible,
            rows,
            cols,
            raw_output,
            exited,
            child: Some(Arc::new(Mutex::new(child_handle))),
        })
    }

    /// Spawn an SSH interactive session to `host`.
    pub fn ssh(rows: u16, cols: u16, host: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg(host);
        cmd.arg("-t");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        debug!("SSH spawned {}x{} host={}", cols, rows, host);
        Self::new(rows, cols, cmd)
    }

    /// Spawn an SFTP subsession to `host` (small fixed size, never rendered).
    pub fn sftp(host: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("sftp");
        cmd.arg(host);
        cmd.env("TERM", "dumb");
        debug!("SFTP spawned host={}", host);
        Self::new(200, 220, cmd)
    }

    /// Spawn an SSH shell to `host` for browsing (fixed size, parsed not rendered).
    pub fn ssh_shell(host: &str) -> Result<Self> {
        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg(host);
        cmd.arg("-t");
        cmd.env("TERM", "dumb");
        debug!("SSH shell spawned host={}", host);
        Self::new(200, 220, cmd)
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
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        if let Ok(mut p) = self.parser.lock() {
            let snapshot = p.screen().contents_formatted();
            let mut np = vt100::Parser::new(rows, cols, 0);
            np.process(&snapshot);
            *p = np;
        }
        self.rows = rows;
        self.cols = cols;
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        let Ok(parser) = self.parser.try_lock() else {
            return;
        };
        let screen = parser.screen();

        fn vc(c: vt100::Color) -> Color {
            match c {
                vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
                vt100::Color::Idx(i) => Color::Indexed(i),
                _ => Color::Reset,
            }
        }

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

        if self.cursor_visible.load(Ordering::Acquire) {
            let (cy, cx) = screen.cursor_position();
            let sx = area.x + cx;
            let sy = area.y + cy;
            if sx < area.x + area.width && sy < area.y + area.height {
                if let Some(bc) = buf.cell_mut((sx, sy)) {
                    let style = bc.style().add_modifier(Modifier::REVERSED);
                    bc.set_style(style);
                }
            }
        }
    }

    pub fn cursor_pos(&self) -> Option<(u16, u16)> {
        if !self.cursor_visible.load(Ordering::Acquire) {
            return None;
        }
        let Ok(parser) = self.parser.try_lock() else {
            return None;
        };
        let (cy, cx) = parser.screen().cursor_position();
        Some((cx, cy))
    }

    pub fn raw_lines(&self) -> Vec<String> {
        let Ok(rb) = self.raw_output.lock() else {
            return vec![];
        };
        crate::browser::parse::strip_ansi(&rb)
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
        if let Some(ref child) = self.child {
            if let Ok(mut c) = child.lock() {
                if let Ok(Some(_status)) = c.try_wait() {
                    self.exited.store(true, Ordering::Release);
                    return true;
                }
            }
        }
        false
    }
}
