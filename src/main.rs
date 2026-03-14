use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, StatefulWidget, Widget},
    Terminal,
};

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Write a line to the debug log file, ignoring lock failures.
/// Uses a macro so the call site can use `format!`-style arguments without
/// paying the formatting cost when the lock is unavailable.
/// When `$log` is `None` (debug mode not enabled) the macro is a no-op and
/// the format arguments are never evaluated.
macro_rules! log {
    ($log:expr, $($arg:tt)*) => {{
        if let Some(ref log_inner) = $log {
            if let Ok(mut f) = log_inner.lock() {
                writeln!(f, $($arg)*).ok();
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// DSR / mouse helpers
// ---------------------------------------------------------------------------

/// Count the number of Device Status Report sequences (`ESC [ 6 n`) in `data`.
///
/// Some applications (e.g. neovim) send DSR to probe the cursor position.
/// We intercept each occurrence in the reader thread and reply immediately
/// with the current parser position so the remote app gets a timely answer
/// instead of blocking on a response that would never come.
fn count_dsr(data: &[u8]) -> usize {
    const DSR: &[u8] = b"\x1b[6n";
    let mut count = 0;
    let mut i = 0;
    while i + DSR.len() <= data.len() {
        if data[i..].starts_with(DSR) { count += 1; i += DSR.len(); } else { i += 1; }
    }
    count
}

// ---------------------------------------------------------------------------
// EmbeddedTerminal
// ---------------------------------------------------------------------------

/// A single pseudo-terminal session driven by an arbitrary command.
///
/// Used for both SSH interactive shells (`ssh host -t`) and SFTP subsessions
/// (`sftp host`).  The caller provides a fully configured `CommandBuilder`
/// so this struct remains command-agnostic.
///
/// Shared state with the background reader thread:
/// - `parser`        — vt100 virtual screen, updated on every PTY read.
/// - `dirty`         — set on new output; cleared (swap) by the draw loop.
/// - `mouse_active`  — tracks SGR mouse reporting modes 1000/1002/1003/1006.
/// - `cursor_visible`— tracks DEC mode 25 (show/hide cursor).
struct EmbeddedTerminal {
    /// VT100 screen state, updated by the reader thread.
    parser:         Arc<Mutex<vt100::Parser>>,
    /// PTY master handle, kept alive so the slave side stays open.
    master:         Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Write half of the PTY; used to forward key events and commands.
    writer:         Arc<Mutex<Box<dyn Write + Send>>>,
    /// Set to `true` by the reader thread whenever new output is processed.
    /// Cleared (swapped to `false`) during the dirty-check phase of the event
    /// loop so that spurious redraws are avoided.
    dirty:          Arc<AtomicBool>,
    /// `true` while the remote application has requested SGR mouse reporting
    /// (modes 1000, 1002, 1003, or 1006).  Mouse events are only forwarded
    /// to the PTY when this flag is set.
    mouse_active:   Arc<AtomicBool>,
    /// Tracks DEC private mode 25 (`ESC [ ? 25 h/l`): `true` while the cursor
    /// is visible, `false` when the remote app has hidden it.  Starts `true`
    /// because the cursor is visible by default in every VT terminal.
    cursor_visible: Arc<AtomicBool>,
    /// Current PTY dimensions, kept in sync with every `resize` call.
    rows:           u16,
    cols:           u16,
    /// Raw byte accumulator for subprocess output (used by SFTP scraping).
    /// Appended by the reader thread; read and drained by the main thread.
    raw_output:     Arc<Mutex<Vec<u8>>>,
}

impl EmbeddedTerminal {
    /// Spawn `cmd` inside a PTY of `rows × cols` cells and start the
    /// background reader thread.
    fn new(rows: u16, cols: u16, cmd: CommandBuilder, log: Option<Arc<Mutex<std::fs::File>>>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        pair.slave.spawn_command(cmd)?;

        let parser         = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let dirty          = Arc::new(AtomicBool::new(false));
        let mouse_active   = Arc::new(AtomicBool::new(false));
        // Cursor starts visible — DEC mode 25 is on by default.
        let cursor_visible = Arc::new(AtomicBool::new(true));

        let raw_output       = Arc::new(Mutex::new(Vec::<u8>::new()));

        // Clone Arc handles for the reader thread before moving into the closure.
        let parser_c         = Arc::clone(&parser);
        let writer_c         = Arc::clone(&writer);
        let dirty_c          = Arc::clone(&dirty);
        let mouse_active_c   = Arc::clone(&mouse_active);
        let cursor_visible_c = Arc::clone(&cursor_visible);
        let raw_output_c     = Arc::clone(&raw_output);
        let log_c            = log.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => { log!(log_c, "PTY EOF"); break; }
                    Ok(n) => {
                        let data = &buf[..n];

                        // Feed raw bytes into the VT100 parser to update the
                        // virtual screen, then signal the main thread to redraw.
                        if let Ok(mut p) = parser_c.lock() { p.process(data); }
                        // Also accumulate into the raw buffer for SFTP scraping.
                        if let Ok(mut rb) = raw_output_c.lock() { rb.extend_from_slice(data); }
                        dirty_c.store(true, Ordering::Release);

                        // Scan for DEC private mode set/reset sequences
                        // (`ESC [ ? <params> h/l`) to track whether the remote
                        // app has enabled or disabled mouse reporting or hidden
                        // the cursor (mode 25).
                        let mut i = 0;
                        while i + 2 < data.len() {
                            if data[i] == 0x1b && data[i+1] == b'[' && data[i+2] == b'?' {
                                let start = i + 3;
                                let mut end = start;
                                while end < data.len() && data[end] != b'h' && data[end] != b'l' { end += 1; }
                                if end < data.len() {
                                    if let Ok(params) = std::str::from_utf8(&data[start..end]) {
                                        let set = data[end] == b'h';
                                        for param in params.split(';') {
                                            match param.trim() {
                                                // SGR mouse reporting modes.
                                                "1000" | "1002" | "1003" | "1006" => {
                                                    mouse_active_c.store(set, Ordering::Release);
                                                }
                                                // DEC mode 25: show (`h`) / hide (`l`) cursor.
                                                "25" => {
                                                    cursor_visible_c.store(set, Ordering::Release);
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    i = end + 1;
                                    continue;
                                }
                            }
                            i += 1;
                        }

                        // Reply to DSR (`ESC [ 6 n`) probes with the current
                        // cursor row/column from the parser state.  Some TUIs
                        // (e.g. neovim) block on this response during startup.
                        let dsr_count = count_dsr(data);
                        if dsr_count > 0 {
                            let (row, col) = if let Ok(p) = parser_c.lock() {
                                let pos = p.screen().cursor_position();
                                (pos.0 + 1, pos.1 + 1)
                            } else { (1, 1) };
                            let reply = format!("\x1b[{};{}R", row, col);
                            if let Ok(mut w) = writer_c.lock() {
                                for _ in 0..dsr_count { let _ = w.write_all(reply.as_bytes()); }
                            }
                        }
                    }
                    Err(e) => { log!(log_c, "PTY error: {}", e); break; }
                }
            }
        });

        let master = Arc::new(Mutex::new(pair.master));
        Ok(Self { parser, master, writer, dirty, mouse_active, cursor_visible, rows, cols, raw_output })
    }

    /// Build and spawn an SSH interactive session to `host`.
    fn ssh(rows: u16, cols: u16, host: &str, log: Option<Arc<Mutex<std::fs::File>>>) -> Result<Self> {
        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg(host);
        cmd.arg("-t");
        // Advertise full 256-colour and true-colour support so that remote
        // applications (vim, tmux, …) use the richest colour codes available.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        log!(log, "SSH spawned {}x{} host={}", cols, rows, host);
        Self::new(rows, cols, cmd, log)
    }

    /// Build and spawn an SFTP subsession to `host`.
    ///
    /// The PTY is kept small (1 row) because we never render the sftp process
    /// output directly — we only screen-scrape it for prompt detection and
    /// directory listings.
    fn sftp(host: &str, log: Option<Arc<Mutex<std::fs::File>>>) -> Result<Self> {
        let mut cmd = CommandBuilder::new("sftp");
        cmd.arg(host);
        // Disable colour and progress bars for deterministic output parsing.
        cmd.env("TERM", "dumb");
        log!(log, "SFTP spawned host={}", host);
        // Tall enough for ls output but small; we never render this terminal.
        Self::new(200, 220, cmd, log)
    }

    /// Write a raw byte string directly into the PTY input stream.
    fn send_str(&mut self, s: &str) {
        if let Ok(mut w) = self.writer.lock() { let _ = w.write_all(s.as_bytes()); }
    }

    /// Encode `c` as UTF-8 and forward it to the PTY.
    fn send_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.send_str(c.encode_utf8(&mut buf));
    }

    /// Notify the PTY and the VT100 parser of a geometry change.
    ///
    /// A snapshot of the current screen contents is re-fed into a freshly
    /// sized parser so that existing output is not lost on resize.  This is a
    /// best-effort heuristic: the remote application will receive `SIGWINCH`
    /// and typically redraws itself completely anyway.
    fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols { return; }
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
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

    /// Blit the VT100 virtual screen into a ratatui `Buffer` region.
    ///
    /// Each cell's symbol, foreground/background colour, and text attributes
    /// are copied verbatim from the parser state.  The cursor cell is
    /// additionally rendered with `REVERSED` so it remains visible even when
    /// the host terminal's hardware cursor is obscured by ratatui's own draw
    /// cycle.
    fn render_into(&self, area: Rect, buf: &mut Buffer) {
        let Ok(parser) = self.parser.try_lock() else { return };
        let screen = parser.screen();

        /// Translate a `vt100::Color` to the ratatui equivalent.
        fn vc(c: vt100::Color) -> Color {
            match c {
                vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
                vt100::Color::Idx(i)       => Color::Indexed(i),
                _                          => Color::Reset,
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
                        if cell.bold()      { style = style.add_modifier(Modifier::BOLD); }
                        if cell.italic()    { style = style.add_modifier(Modifier::ITALIC); }
                        if cell.underline() { style = style.add_modifier(Modifier::UNDERLINED); }
                        if cell.inverse()   { style = style.add_modifier(Modifier::REVERSED); }
                        bc.set_style(style);
                    }
                }
            }
        }

        // Overlay the cursor cell with REVERSED when the remote app has not
        // hidden it (DEC mode 25).  This keeps the cursor visible even when
        // ratatui has suppressed the hardware caret during its draw cycle.
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

    /// Return the cursor position as `(col, row)` in terminal-local coordinates,
    /// or `None` if the remote app has hidden the cursor (DEC mode 25 off).
    fn cursor_pos(&self) -> Option<(u16, u16)> {
        if !self.cursor_visible.load(Ordering::Acquire) { return None; }
        let Ok(parser) = self.parser.try_lock() else { return None };
        let screen = parser.screen();
        let (cy, cx) = screen.cursor_position();
        Some((cx, cy))
    }

    /// Return the accumulated raw output of the subprocess as a list of lines.
    ///
    /// Strips ANSI/VT escape sequences so the result is plain text suitable
    /// for the SFTP scraping functions.  The buffer is NOT drained here so
    /// multiple callers can read the same output; call `drain_raw()` after
    /// processing is complete to avoid unbounded growth.
    fn raw_lines(&self) -> Vec<String> {
        let Ok(rb) = self.raw_output.lock() else { return vec![] };
        let text = strip_ansi(&rb);
        text.lines().map(|l| l.trim_end().to_string()).collect()
    }

    /// Drain (clear) the raw output accumulator.
    fn drain_raw(&self) {
        if let Ok(mut rb) = self.raw_output.lock() { rb.clear(); }
    }

    /// Return the current byte length of the raw output accumulator.
    fn raw_len(&self) -> usize {
        self.raw_output.lock().map(|rb| rb.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// SSH host config
// ---------------------------------------------------------------------------

/// A single entry from `~/.ssh/config` that can be dialled directly.
#[derive(Clone)]
struct SshHost { label: String }

/// Parse `~/.ssh/config` and return all non-wildcard `Host` entries.
///
/// Glob patterns (`*`, `?`) are skipped because they cannot be passed
/// verbatim to `ssh(1)` as a target host name.
fn parse_ssh_config() -> Vec<SshHost> {
    let config_path = match dirs::home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None    => return vec![],
    };
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c)  => c,
        Err(_) => return vec![],
    };
    let mut hosts = Vec::new();
    for line in content.lines() {
        // Strip a leading BOM that some editors insert on Windows.
        let line = line.trim_start_matches('\u{feff}');
        let trimmed = line.trim();
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());
        if let (Some(kw), Some(rest)) = (parts.next(), parts.next()) {
            if kw.eq_ignore_ascii_case("host") {
                let name = rest.trim();
                if !name.is_empty() && !name.contains('*') && !name.contains('?') {
                    hosts.push(SshHost { label: name.to_string() });
                }
            }
        }
    }
    hosts
}

// ---------------------------------------------------------------------------
// SFTP file browser
// ---------------------------------------------------------------------------

/// A single entry in a directory listing (local or remote).
#[derive(Clone)]
struct FsEntry {
    name:     String,
    is_dir:   bool,
    /// Human-readable size string (e.g. "4.2 MB").  Empty for directories.
    size:     String,
    /// Permission string as returned by ls (e.g. `drwxr-xr-x`).
    perms:    String,
    /// Last-modified date string (e.g. `Mar 14 09:44` or `Oct  1  2021`).
    modified: String,
}

/// Which panel of the file browser is active.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserFocus { Local, Remote }

/// State machine for the SFTP subprocess interaction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SftpState {
    /// Waiting for the initial `sftp>` prompt after connection.
    Connecting,
    /// Sitting at the `sftp>` prompt, ready to accept commands.
    Idle,
    /// A `pwd` command was sent; waiting for its prompt to reappear before
    /// issuing `ls -la`.  Keeping the two commands separate ensures we never
    /// mistake the post-pwd prompt for the post-ls prompt.
    WaitingPwd,
    /// An `ls -la` command was sent; waiting for the prompt to reappear.
    WaitingLs,
    /// A `cd` command was sent; waiting for the prompt before issuing `pwd`.
    WaitingCd,
    /// A `remove` command was sent; waiting for the prompt before issuing `ls -la`.
    WaitingDelete,
    /// A `get` or `put` command was sent; watching for completion.
    Transferring,
}

/// In-progress or completed transfer record shown in the status bar.
#[derive(Clone)]
struct TransferStatus {
    filename:  String,
    done:      bool,
    /// Latest progress line scraped from the sftp output (e.g. "50%").
    progress:  String,
}

/// State for a `Pane::FileBrowser`.
struct FileBrowser {
    host:             String,
    /// The hidden SFTP subprocess used for all remote operations.
    sftp:             EmbeddedTerminal,
    sftp_state:       SftpState,

    /// Local filesystem path currently displayed in the left panel.
    local_path:       PathBuf,
    local_entries:    Vec<FsEntry>,
    local_sel:        ListState,

    /// Current remote working directory (updated after every successful cd).
    remote_path:      String,
    remote_entries:   Vec<FsEntry>,
    remote_sel:       ListState,

    focus:            BrowserFocus,

    /// Most recent transfer, shown in the status bar.
    last_transfer:    Option<TransferStatus>,
    /// Queue of commands waiting to be sent once the sftp subprocess is idle.
    /// Using a queue rather than a single Option ensures that rapid navigation
    /// (e.g. pressing Enter twice quickly) does not silently drop commands.
    pending_cmds:     VecDeque<String>,
    /// Status / error message to display at the bottom of the pane.
    status_msg:       String,
    /// Snapshot of raw sftp output shown in the remote panel while connecting
    /// or waiting for an ls to complete.  Cleared once entries are populated.
    raw_snapshot:     Vec<String>,
    /// Number of consecutive ticks the prompt has been stable (buffer unchanged).
    /// We only act on a prompt once it has been stable for several ticks,
    /// ensuring all ls output has arrived before we parse and drain.
    prompt_stable:    u8,
    /// Byte length of raw_output on the previous tick, used to detect growth.
    prev_raw_len:     usize,
    /// Set to `true` by tick() when a state transition requires a redraw even
    /// if the sftp PTY dirty flag is not set.
    needs_redraw:     bool,
    /// When `Some(name)`, a confirmation prompt is shown before deleting.
    confirm_delete:   Option<String>,
    /// Name of the remote file currently being deleted, used to set the
    /// success message once `WaitingDelete` completes.
    pending_delete_name: Option<String>,
    /// Shared debug log handle — `None` unless `--debug` was passed at launch.
    log:              Option<Arc<Mutex<std::fs::File>>>,
}

impl FileBrowser {
    /// Create a new `FileBrowser` connected to `host`.
    ///
    /// The SFTP subprocess is spawned immediately; the first `ls` is sent as
    /// soon as the initial `sftp>` prompt is detected in `tick()`.
    fn new(host: &str, log: Option<Arc<Mutex<std::fs::File>>>) -> Result<Self> {
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
            pending_cmds: VecDeque::new(),
            status_msg: String::from("Connecting…"),
            raw_snapshot: vec![],
            prompt_stable: 0,
            prev_raw_len:  0,
            needs_redraw:  false,
            confirm_delete: None,
            pending_delete_name: None,
            log:            log.clone(),
        })
    }

    /// Called every draw tick.  Drives the SFTP state machine by inspecting
    /// the raw output accumulator of the hidden sftp subprocess.
    fn tick(&mut self) {
        // Only update the snapshot while actively waiting — not during Idle,
        // which would overwrite a valid snapshot with an empty-drained buffer.
        if !matches!(self.sftp_state, SftpState::Idle) {
            self.raw_snapshot = self.sftp.raw_lines();
        }

        // Track whether the raw buffer has grown since the last tick.
        let cur_len = self.sftp.raw_len();
        if cur_len != self.prev_raw_len {
            // New bytes arrived — reset the stability counter.
            self.prompt_stable = 0;
            self.prev_raw_len  = cur_len;
        } else if self.prompt_raw_ends_with_prompt() {
            // Buffer unchanged and prompt visible — increment stability.
            self.prompt_stable = self.prompt_stable.saturating_add(1);
        } else {
            self.prompt_stable = 0;
        }

        // We act on a prompt only once it has been stable for at least 3 ticks
        // (~15 ms) with no new bytes, ensuring all output has been flushed.
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
                    self.remote_path = parse_pwd(&lines)
                        .unwrap_or_else(|| self.remote_path.clone());
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
                    if let Some(p) = parse_pwd(&lines) { self.remote_path = p; }
                    let parsed = parse_ls(&lines);
                    log!(self.log, "SFTP ls done: {} entries parsed (raw {} lines)", parsed.len(), lines.len());
                    if parsed.len() > 1 {
                        self.remote_entries = parsed;
                        self.raw_snapshot.clear();
                        // Clamp selection so it stays within the new entry list.
                        let max = self.remote_entries.len().saturating_sub(1);
                        let cur = self.remote_sel.selected().unwrap_or(0);
                        self.remote_sel.select(Some(cur.min(max)));
                    }
                    if self.remote_sel.selected().is_none() { self.remote_sel.select_first(); }
                    self.sftp.drain_raw();
                    self.prev_raw_len = 0;
                    self.sftp_state = SftpState::Idle;
                    self.needs_redraw = true;
                    if let Some(cmd) = self.pending_cmds.pop_front() {
                        self.sftp.send_str(&cmd);
                        self.sftp_state = SftpState::WaitingLs;
                    }
                }
            }

            SftpState::Transferring => {
                if prompt_ready {
                    self.prompt_stable = 0;
                    if let Some(ref mut t) = self.last_transfer {
                        t.done = true;
                        t.progress = "100%".to_string();
                    }
                    // Reload local panel so downloaded files appear immediately.
                    self.local_entries = read_local_dir(&self.local_path);
                    log!(self.log, "SFTP transfer complete, refreshed local dir");
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
                    let lines = self.sftp.raw_lines();
                    log!(self.log, "SFTP WaitingDelete complete, raw output: {:?}", lines);
                    // Set a success message using the name we stored before sending rm.
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

            SftpState::Idle => {
                if let Some(cmd) = self.pending_cmds.pop_front() {
                    self.prompt_stable = 0;
                    self.sftp.send_str(&cmd);
                    self.sftp_state = SftpState::WaitingLs;
                }
            }
        }
    }

    /// Return `true` when the last non-empty line of raw output contains `sftp>`.
    fn prompt_raw_ends_with_prompt(&self) -> bool {
        let Ok(rb) = self.sftp.raw_output.lock() else { return false };
        let text = strip_ansi(&rb);
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.contains("sftp>"))
            .unwrap_or(false)
    }

    // ---- navigation --------------------------------------------------------

    fn nav_up(&mut self) {
        match self.focus {
            BrowserFocus::Local  => self.local_sel.select_previous(),
            BrowserFocus::Remote => self.remote_sel.select_previous(),
        }
    }

    fn nav_down(&mut self) {
        match self.focus {
            BrowserFocus::Local  => self.local_sel.select_next(),
            BrowserFocus::Remote => self.remote_sel.select_next(),
        }
    }

    /// Enter the selected directory, or download the selected remote file.
    /// Ignores keypresses while the sftp process is busy so rapid presses
    /// cannot corrupt the command stream.
    fn enter(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
                if let Some(i) = self.local_sel.selected() {
                    let entry = if let Some(e) = self.local_entries.get(i).cloned() { e } else { return };
                    if entry.name == ".." {
                        if let Some(p) = self.local_path.parent() {
                            self.local_path = p.to_path_buf();
                        } else {
                            self.local_path = PathBuf::from(local_root());
                        }
                    } else if entry.is_dir {
                        self.local_path.push(&entry.name);
                    } else {
                        return; // plain file — nothing to do on local side
                    }
                    self.local_entries = read_local_dir(&self.local_path);
                    self.local_sel.select_first();
                    self.needs_redraw = true;
                }
            }
            BrowserFocus::Remote => {
                if self.sftp_state != SftpState::Idle { return; }
                if let Some(i) = self.remote_sel.selected() {
                    if let Some(entry) = self.remote_entries.get(i).cloned() {
                        if entry.is_dir {
                            // Send cd as a single command; WaitingCd → WaitingPwd → WaitingLs.
                            self.sftp.send_str(&format!("cd {}\r\n", shell_quote(&entry.name)));
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

    /// Navigate up one directory in the focused panel.
    fn go_up(&mut self) {
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
                if self.sftp_state != SftpState::Idle { return; }
                self.sftp.send_str("cd ..\r\n");
                self.sftp_state = SftpState::WaitingCd;
                log!(self.log, "SFTP cd ..");
            }
        }
    }

    // ---- transfers ---------------------------------------------------------

    /// Download the selected remote file to the current local directory (F5).
    fn download(&mut self) {
        if self.sftp_state != SftpState::Idle { return; }
        if let Some(i) = self.remote_sel.selected() {
            let entry = if let Some(e) = self.remote_entries.get(i).cloned() { e } else { return };
            if entry.is_dir { return; }
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

    /// Upload the selected local file to the current remote directory (F6).
    fn upload(&mut self) {
        if self.sftp_state != SftpState::Idle { return; }
        if let Some(i) = self.local_sel.selected() {
            let entry = if let Some(e) = self.local_entries.get(i).cloned() { e } else { return };
            if entry.is_dir { return; }
            let local_path = self.local_path.join(&entry.name);
            let local_str  = local_path.to_string_lossy().replace('\\', "/");
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

    /// Stage a file for deletion — shows a confirmation prompt.
    /// Deletes from the local filesystem when the local panel is focused,
    /// or from the remote server when the remote panel is focused.
    fn delete_focused(&mut self) {
        match self.focus {
            BrowserFocus::Local => {
                if let Some(i) = self.local_sel.selected() {
                    let entry = if let Some(e) = self.local_entries.get(i).cloned() { e } else { return };
                    if entry.name == ".." { return; }
                    // Prefix with "local:" so confirm_delete_yes knows which side to delete.
                    self.confirm_delete = Some(format!("local:{}", entry.name));
                    self.needs_redraw = true;
                }
            }
            BrowserFocus::Remote => {
                if let Some(i) = self.remote_sel.selected() {
                    let entry = if let Some(e) = self.remote_entries.get(i).cloned() { e } else {
                        log!(self.log, "SFTP delete: no entry at index {:?}", self.remote_sel.selected());
                        return;
                    };
                    log!(self.log, "SFTP delete candidate: '{}' is_dir={} state={:?}",
                        entry.name, entry.is_dir, self.sftp_state == SftpState::Idle);
                    if entry.name == ".." { return; }
                    if self.sftp_state != SftpState::Idle { return; }
                    self.confirm_delete = Some(format!("remote:{}", entry.name));
                    self.needs_redraw = true;
                }
            }
        }
    }

    /// Execute the pending deletion after the user confirmed with `y`.
    fn confirm_delete_yes(&mut self) {
        if let Some(tagged) = self.confirm_delete.take() {
            if let Some(name) = tagged.strip_prefix("local:") {
                let path = self.local_path.join(name);
                log!(self.log, "Local delete: {:?}", path);
                if let Err(e) = std::fs::remove_file(&path) {
                    self.status_msg = format!("Delete failed: {}", e);
                    log!(self.log, "Local delete error: {}", e);
                } else {
                    self.status_msg = format!("Deleted local: {}", name);
                    self.local_entries = read_local_dir(&self.local_path);
                }
                self.needs_redraw = true;
            } else if let Some(name) = tagged.strip_prefix("remote:") {
                let cmd = format!("rm {}\r\n", shell_quote(name));
                log!(self.log, "SFTP sending: {}", cmd.trim());
                self.sftp.send_str(&cmd);
                self.sftp_state = SftpState::WaitingDelete;
                self.status_msg = format!("Deleting {}...", name);
                self.pending_delete_name = Some(name.to_string());
                self.needs_redraw = true;
            }
        }
    }

    /// Cancel the pending deletion.
    fn confirm_delete_no(&mut self) {
        self.confirm_delete = None;
        self.status_msg = String::from("Deletion cancelled.");
        self.needs_redraw = true;
    }

    // ---- drag-and-drop -----------------------------------------------------

    /// Called when a file is "dragged" from the local panel and "dropped"
    /// on the remote panel (mouse down in local, mouse up in remote).
    /// Triggers an upload of the currently selected local file.
    fn drag_local_to_remote(&mut self) {
        // Reuse the upload path — the selected local entry is the dragged file.
        self.upload();
    }

    /// Called when a file is dragged from the remote panel to the local panel.
    /// Triggers a download of the currently selected remote file.
    fn drag_remote_to_local(&mut self) {
        self.download();
    }

    // ---- render ------------------------------------------------------------

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool, leaf_count: usize) {
        // Only draw an outer border when multiple panes are visible.
        // When this is the sole pane the outer app border already provides
        // the frame and title, so a second border would double-up.
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

        // Reserve one line at the bottom for the status bar.
        let status_h = 1u16;
        let panels_area = Rect { height: inner.height.saturating_sub(status_h + 1), ..inner };
        let status_area = Rect {
            y:      inner.y + inner.height.saturating_sub(status_h),
            height: status_h,
            ..inner
        };

        // Divide the panels area into left (local) and right (remote).
        let half = panels_area.width / 2;
        let local_area = Rect { width: half, ..panels_area };
        let remote_area = Rect { x: panels_area.x + half, width: panels_area.width - half, ..panels_area };

        self.render_panel(local_area,  buf, BrowserFocus::Local,  is_focus);
        self.render_panel(remote_area, buf, BrowserFocus::Remote, is_focus);
        self.render_status(status_area, buf);
    }

    fn render_panel(&mut self, area: Rect, buf: &mut Buffer, side: BrowserFocus, pane_focused: bool) {
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

        let border_col = if is_active { Color::Cyan } else { Color::DarkGray };
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

        // When the remote panel has no real entries yet (only the synthetic ".."
        // or completely empty), show the raw sftp output so the user can see
        // banners, errors, host-key prompts, and ls output in real time.
        let only_dotdot = entries.len() <= 1 && entries.first().map(|e| e.name == "..").unwrap_or(true);
        if side == BrowserFocus::Remote && only_dotdot && !self.raw_snapshot.is_empty() {
            // Show last N lines of raw output that fit in the panel.
            let visible: Vec<&String> = self.raw_snapshot.iter()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let start = visible.len().saturating_sub(inner.height as usize);
            for (i, line) in visible[start..].iter().enumerate() {
                let y = inner.y + i as u16;
                if y >= inner.y + inner.height { break; }
                let span = Span::styled(
                    line.chars().take(inner.width as usize).collect::<String>(),
                    Style::default().fg(Color::DarkGray),
                );
                buf.set_span(inner.x, y, &span, inner.width);
            }
            return;
        }

        // Fixed column widths: size(9) + modified(16) + perms(10) + separators.
        // Name gets the remaining space and is truncated to fit.
        const W_SIZE:  usize =  9;
        const W_MOD:   usize = 16;
        const W_PERMS: usize = 10;
        const W_GAPS:  usize =  3;
        let w_name = (inner.width as usize).saturating_sub(W_SIZE + W_MOD + W_PERMS + W_GAPS);

        let items: Vec<ListItem> = entries.iter().map(|e| {
            let name_col = if e.is_dir { Color::Cyan } else { Color::White };
            let display_name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
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
        }).collect();

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(if is_active { Color::Cyan } else { Color::DarkGray })
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        StatefulWidget::render(list, inner, buf, list_state);
    }

    fn render_status(&self, area: Rect, buf: &mut Buffer) {
        // Confirmation prompt overrides the normal status bar.
        if let Some(ref tagged) = self.confirm_delete {
            let name = tagged.strip_prefix("local:").or_else(|| tagged.strip_prefix("remote:")).unwrap_or(tagged);
            let side = if tagged.starts_with("local:") { "local" } else { "remote" };
            let msg = format!("  Delete {} '{}'?  [y] Yes   [n] No", side, name);
            let span = Span::styled(msg, Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD));
            buf.set_span(area.x, area.y, &span, area.width);
            return;
        }

        let (state_label, state_col) = match self.sftp_state {
            SftpState::Connecting    => ("[connecting]", Color::Yellow),
            SftpState::WaitingCd     => ("[cd…]",        Color::Yellow),
            SftpState::WaitingPwd    => ("[pwd…]",       Color::Yellow),
            SftpState::Idle          => ("[idle]",       Color::DarkGray),
            SftpState::WaitingLs     => ("[ls…]",        Color::Yellow),
            SftpState::WaitingDelete => ("[deleting…]",  Color::Red),
            SftpState::Transferring  => ("[xfer…]",      Color::Green),
        };

        let transfer_str = if let Some(ref t) = self.last_transfer {
            if t.done { format!("✓ {}  ", t.filename) }
            else      { format!("⟳ {} {}  ", t.filename, t.progress) }
        } else {
            String::new()
        };

        let parse_hint = if self.remote_entries.len() <= 1 && !self.raw_snapshot.is_empty() {
            self.raw_snapshot.iter().rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| format!(" | {}", l.trim().chars().take(40).collect::<String>()))
                .unwrap_or_default()
        } else { String::new() };

        let help = "Tab:switch  Spc/Enter:cd  Bksp:up  F5:Download  F6:Upload  Del:rm";
        let state_span = Span::styled(state_label, Style::default().fg(state_col));
        let rest_span  = Span::styled(
            format!(" {}{}  {}  {}", self.status_msg, parse_hint, transfer_str, help),
            Style::default().fg(Color::DarkGray),
        );
        buf.set_line(area.x, area.y, &Line::from(vec![state_span, rest_span]), area.width);
    }
}

// ---------------------------------------------------------------------------
// ANSI stripping
// ---------------------------------------------------------------------------

/// Remove all ANSI/VT escape sequences from raw PTY bytes, returning plain text.
///
/// Handles CSI sequences (`ESC [ ... <final>`), OSC sequences
/// (`ESC ] ... ST/BEL`), and bare `ESC <char>` two-byte sequences.
fn strip_ansi(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == 0x1b {
            i += 1;
            if i >= raw.len() { break; }
            match raw[i] {
                // CSI sequence: ESC [ ... <0x40–0x7e>
                b'[' => {
                    i += 1;
                    while i < raw.len() && !(0x40..=0x7e).contains(&raw[i]) { i += 1; }
                    i += 1; // consume final byte
                }
                // OSC sequence: ESC ] ... BEL or ST
                b']' => {
                    i += 1;
                    while i < raw.len() && raw[i] != 0x07 {
                        if raw[i] == 0x1b && i + 1 < raw.len() && raw[i+1] == b'\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                    if i < raw.len() { i += 1; } // consume BEL
                }
                // Two-byte ESC sequence
                _ => { i += 1; }
            }
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    // Interpret as UTF-8, replacing any invalid bytes with the replacement char.
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// SFTP screen-scraping helpers
// ---------------------------------------------------------------------------


/// Extract the remote working directory from `pwd` output.
///
/// OpenSSH sftp prints: `Remote working directory: /home/user`
/// Some servers print just the path on its own line after the `pwd` command echo.
fn parse_pwd(lines: &[String]) -> Option<String> {
    // Preferred: explicit "working directory:" label.
    if let Some(l) = lines.iter().find(|l| l.contains("working directory")) {
        if let Some(path) = l.splitn(2, ':').nth(1) {
            let p = path.trim();
            if !p.is_empty() { return Some(p.to_string()); }
        }
    }
    // Fallback: first line that looks like an absolute path after the `pwd` echo.
    lines.iter()
        .find(|l| {
            let t = l.trim();
            (t.starts_with('/') || t.starts_with('~')) && !t.contains(' ')
        })
        .map(|l| l.trim().to_string())
}

/// Parse the output of `ls -la` into a `Vec<FsEntry>`.
///
/// Handles the OpenSSH sftp `ls -la` format, including servers that mask
/// permission bits with `*` and use `?` for the link count:
///   `drwx******    ?  debian  debian  4096  Mar 14 09:44  dirname`
///   `-rw-******    ?  debian  debian   220  Aug  4  2021  filename`
///
/// The name is located by finding the 9th whitespace-separated token,
/// counting from 0 (perms=0, links=1, user=2, group=3, size=4,
/// month=5, day=6, time-or-year=7, name=8+).
fn parse_ls(lines: &[String]) -> Vec<FsEntry> {
    let mut entries = vec![FsEntry { name: "..".to_string(), is_dir: true, size: String::new(), perms: String::new(), modified: String::new() }];
    for line in lines {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("sftp>")
            || line.starts_with("Remote")
            || line.starts_with("Changing")
            || line.starts_with("total")
            || line.starts_with("ls")
        { continue; }

        // Split into tokens on runs of whitespace (not individual chars).
        let tokens: Vec<&str> = line.split_whitespace().collect();

        // Need at least 9 tokens: perms links user group size month day time name
        if tokens.len() < 9 { continue; }

        let perms   = tokens[0];
        let is_dir  = perms.starts_with('d');
        let is_link = perms.starts_with('l');
        if !perms.starts_with('-') && !is_dir && !is_link { continue; }

        // Size is token[4]; link count may be '?' so we don't parse tokens[1].
        let size_bytes: u64 = tokens[4].parse().unwrap_or(0);

        // Name starts at token[8] and may contain spaces — rejoin from that
        // position using the original line rather than re-joining tokens.
        let name = skip_n_tokens(line, 8).trim_end().to_string();

        if name.is_empty() || name == "." || name == ".." { continue; }

        // Symlinks: strip " -> target" suffix.
        let name = if is_link {
            name.splitn(2, " -> ").next().unwrap_or(&name).to_string()
        } else {
            name
        };

        let modified = format!("{} {} {}", tokens[5], tokens[6], tokens[7]);
        entries.push(FsEntry {
            name,
            is_dir:   is_dir || is_link,
            size:     if is_dir { String::new() } else { human_size(size_bytes) },
            perms:    perms.to_string(),
            modified,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    entries
}

/// Return the slice of `line` that begins after skipping `n` whitespace-
/// separated tokens.  Used to extract the filename field from `ls -la` output
/// while preserving spaces within the name.
fn skip_n_tokens(line: &str, n: usize) -> &str {
    let mut remaining = line.trim_start();
    for _ in 0..n {
        // Skip one token (non-whitespace run).
        let end = remaining.find(|c: char| c.is_ascii_whitespace()).unwrap_or(remaining.len());
        remaining = &remaining[end..];
        // Skip whitespace between tokens.
        remaining = remaining.trim_start();
    }
    remaining
}

/// Scrape a transfer progress percentage from sftp output lines.
///
/// OpenSSH sftp prints lines like:
/// `filename.tar.gz           42%  142 MB   4.2 MB/s   00:33`
fn scrape_transfer_progress(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|l| {
        let l = l.trim();
        // Find a token that looks like "N%" where N is a number.
        l.split_whitespace()
            .find(|tok| tok.ends_with('%') && tok.trim_end_matches('%').parse::<u32>().is_ok())
            .map(|s| s.to_string())
    })
}

// ---------------------------------------------------------------------------
// Local filesystem helpers
// ---------------------------------------------------------------------------

/// Decompose a Unix timestamp into (year, month, day, hour, minute).
fn epoch_to_ymd(secs: u64) -> (u32, u32, u32, u32, u32) {
    let mi = (secs / 60) % 60;
    let h  = (secs / 3600) % 24;
    let days = secs / 86400;
    let mut y = 1970u32;
    let mut d = days as u32;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let ydays = if leap { 366 } else { 365 };
        if d < ydays { break; }
        d -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days: [u32; 12] = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0u32;
    for mlen in month_days {
        if d < mlen { break; }
        d -= mlen;
        mo += 1;
    }
    (y, mo + 1, d + 1, h as u32, mi as u32)
}

/// Return the top-level root to navigate to when Backspace is pressed at a
/// filesystem root.  On Windows this is the root of all drives; on Unix `/`.
fn local_root() -> &'static str {
    if cfg!(windows) { "\\.\\" } else { "/" }
}

/// Read a local directory into a sorted `Vec<FsEntry>`.
fn read_local_dir(path: &Path) -> Vec<FsEntry> {
    let mut entries = vec![FsEntry { name: "..".to_string(), is_dir: true, size: String::new(), perms: String::new(), modified: String::new() }];
    if let Ok(rd) = fs::read_dir(path) {
        for entry in rd.flatten() {
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size   = meta.as_ref().and_then(|m| if is_dir { None } else { Some(human_size(m.len())) }).unwrap_or_default();
            let modified = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let (y, mo, d, h, mi) = epoch_to_ymd(secs);
                    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, mo, d, h, mi)
                })
                .unwrap_or_default();
            entries.push(FsEntry {
                name:     entry.file_name().to_string_lossy().to_string(),
                is_dir,
                size,
                perms:    String::new(),
                modified,
            });
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    entries
}

/// Format a byte count as a human-readable string (e.g. `4.2 MB`).
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit + 1 < UNITS.len() {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{} B", bytes) } else { format!("{:.1} {}", val, UNITS[unit]) }
}

/// Quote a filename for use in an sftp command.
///
/// Wraps the name in single quotes and escapes any embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Pane tree
// ---------------------------------------------------------------------------

/// Direction along which a split divides its available area.
enum Split { Horizontal, Vertical }

/// A node in the pane tree.
///
/// Leaf variants:
/// - `Connect`     — host-picker shown before a session is opened.
/// - `Session`     — a live interactive SSH terminal.
/// - `FileBrowser` — two-panel SFTP file manager.
///
/// Internal variant:
/// - `Split` — divides its area among `children` horizontally or vertically.
enum Pane {
    Connect     { list_state: ListState },
    Session     { terminal: EmbeddedTerminal },
    FileBrowser { browser: FileBrowser },
    Split       { kind: Split, children: Vec<Pane> },
}

impl Pane {
    /// Create a `Connect` leaf with the first host pre-selected.
    fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect { list_state: ls }
    }

    /// Collect the screen rectangles of every leaf in DFS order.
    fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => vec![area],
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                children.iter().zip(areas).flat_map(|(c, a)| c.leaf_areas(a)).collect()
            }
        }
    }

    /// Total number of leaf panes (DFS count).
    fn leaf_count(&self) -> usize {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    /// Return a mutable reference to the `n`-th leaf in DFS order.
    fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count { return child.leaf_mut(n - offset); }
                    offset += count;
                }
                None
            }
        }
    }

    /// Return a shared reference to the `n`-th leaf in DFS order.
    fn leaf(&self, n: usize) -> Option<&Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count { return child.leaf(n - offset); }
                    offset += count;
                }
                None
            }
        }
    }

    /// Replace the `n`-th leaf with a new split containing the original leaf
    /// and a fresh `Connect` pane.
    fn split_leaf(&mut self, n: usize, kind: Split) {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {}
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for (i, child) in children.iter_mut().enumerate() {
                    let count = child.leaf_count();
                    if n < offset + count {
                        if count == 1 {
                            let old = std::mem::replace(child, Pane::new_connect());
                            *child = Pane::Split { kind, children: vec![old, Pane::new_connect()] };
                        } else {
                            child.split_leaf(n - offset, kind);
                        }
                        break;
                    }
                    offset += count;
                    let _ = i;
                }
            }
        }
    }

    /// Return `true` if any leaf has produced new output since the last call.
    fn any_dirty(&mut self) -> bool {
        match self {
            Pane::Session     { terminal } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::FileBrowser { browser  } => {
                let pty_dirty = browser.sftp.dirty.swap(false, Ordering::AcqRel);
                let state_dirty = browser.needs_redraw;
                browser.needs_redraw = false;
                pty_dirty || state_dirty
            }
            Pane::Split { children, .. }   => children.iter_mut().any(|c| c.any_dirty()),
            _ => false,
        }
    }

    /// Tick all `FileBrowser` panes so they can advance their SFTP state machine.
    fn tick_browsers(&mut self) {
        match self {
            Pane::FileBrowser { browser } => browser.tick(),
            Pane::Split { children, .. }  => children.iter_mut().for_each(|c| c.tick_browsers()),
            _ => {}
        }
    }

    /// Propagate a resize event down to every `Session` and `FileBrowser` leaf.
    ///
    /// `multi_pane` mirrors the `leaf_count > 1` condition in `render`: when
    /// `true`, each leaf will have a 1-cell border drawn around it, so the
    /// terminal is sized to the inner area (width - 2, height - 2).
    fn resize_all(&mut self, area: Rect, multi_pane: bool) {
        match self {
            Pane::Session { terminal } => {
                let (h, w) = if multi_pane {
                    (area.height.saturating_sub(2), area.width.saturating_sub(2))
                } else {
                    (area.height, area.width)
                };
                terminal.resize(h, w);
            }
            // The FileBrowser's sftp terminal is intentionally kept at a fixed
            // large size (200×220) for output parsing; we do not resize it with
            // the pane because we never render it directly.
            Pane::FileBrowser { .. } => {}
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    child.resize_all(a, true);
                }
            }
            _ => {}
        }
    }

    /// Recursively render the pane tree into `buf`.
    fn render(&mut self, area: Rect, buf: &mut Buffer, hosts: &[SshHost], focus_idx: usize, leaf_count: usize, my_idx: &mut usize) {
        match self {
            Pane::Connect { list_state } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus {
                        Style::default().fg(Color::Blue)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let block = Block::default().borders(Borders::ALL).border_style(border_style).title(" connect ");
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else {
                    area
                };

                const HELP_LINES: u16 = 8;
                let list_area = Rect {
                    x: inner.x, y: inner.y,
                    width: inner.width,
                    height: inner.height.saturating_sub(HELP_LINES + 1),
                };
                let help_area = Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(HELP_LINES),
                    width: inner.width,
                    height: HELP_LINES,
                };

                let items: Vec<&str> = hosts.iter().map(|h| h.label.as_str()).collect();
                let list = List::new(items)
                    .style(Style::default().fg(Color::White))
                    .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                    .highlight_symbol("> ");
                StatefulWidget::render(list, list_area, buf, list_state);

                let shortcuts = [
                    ("Alt+T",    "new tab"),
                    ("Alt+W",    "close pane / tab"),
                    ("Alt+-",    "split vertical"),
                    ("Alt++",    "split horizontal"),
                    ("Alt+B",    "open file browser"),
                    ("Alt+↑↓",   "cycle pane focus"),
                    ("Alt+←→",   "switch tab"),
                    ("Ctrl+C",   "quit"),
                ];
                for (i, (key, desc)) in shortcuts.iter().enumerate() {
                    let y = help_area.y + i as u16;
                    if y >= help_area.y + help_area.height { break; }
                    let key_span  = Span::raw(format!("  {:10}", key)).style(Style::default().fg(Color::Yellow));
                    let desc_span = Span::raw(*desc).style(Style::default().fg(Color::DarkGray));
                    buf.set_line(help_area.x, y, &Line::from(vec![key_span, desc_span]), help_area.width);
                }
            }

            Pane::Session { terminal } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                let inner = if leaf_count > 1 {
                    let border_style = if is_focus {
                        Style::default().fg(Color::Blue)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let block = Block::default().borders(Borders::ALL).border_style(border_style);
                    let inner = block.inner(area);
                    block.render(area, buf);
                    inner
                } else {
                    area
                };
                terminal.render_into(inner, buf);
            }

            Pane::FileBrowser { browser } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;
                browser.render(area, buf, is_focus, leaf_count);
            }

            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    child.render(a, buf, hosts, focus_idx, leaf_count, my_idx);
                }
            }
        }
    }
}

/// Divide `area` into `count` equal sub-rectangles along the split axis.
///
/// The last child absorbs any remainder so that the combined widths/heights
/// always equal the parent dimension exactly.
fn split_areas(area: Rect, kind: &Split, count: usize) -> Vec<Rect> {
    if count == 0 { return vec![]; }
    match kind {
        Split::Horizontal => {
            let w = area.width / count as u16;
            (0..count).map(|i| Rect {
                x:      area.x + i as u16 * w,
                y:      area.y,
                width:  if i == count - 1 { area.width - i as u16 * w } else { w },
                height: area.height,
            }).collect()
        }
        Split::Vertical => {
            let h = area.height / count as u16;
            (0..count).map(|i| Rect {
                x:      area.x,
                y:      area.y + i as u16 * h,
                width:  area.width,
                height: if i == count - 1 { area.height - i as u16 * h } else { h },
            }).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

/// A named workspace containing one pane tree and a focused-leaf index.
struct Tab {
    name:      String,
    /// Root of the pane tree; may be a single leaf or a nested split.
    root:      Pane,
    /// DFS index of the currently focused leaf pane.
    focus_idx: usize,
}

impl Tab {
    fn new(name: &str) -> Self {
        Tab { name: name.to_string(), root: Pane::new_connect(), focus_idx: 0 }
    }

    fn leaf_count(&self) -> usize { self.root.leaf_count() }

    /// Move focus to the next leaf (wraps around).
    fn focus_next(&mut self) { self.focus_idx = (self.focus_idx + 1) % self.leaf_count(); }

    /// Move focus to the previous leaf (wraps around).
    fn focus_prev(&mut self) {
        if self.focus_idx == 0 { self.focus_idx = self.leaf_count() - 1; }
        else { self.focus_idx -= 1; }
    }

    /// Label shown in the tab bar.
    fn display_name(&self) -> &str {
        if self.leaf_count() == 1 {
            match &self.root {
                Pane::Connect     { .. } => "<connect>",
                Pane::Session     { .. } => &self.name,
                Pane::FileBrowser { .. } => &self.name,
                _                        => &self.name,
            }
        } else {
            &self.name
        }
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.root.leaf_mut(self.focus_idx)
    }

    /// Wrap the currently focused leaf in a new split node and trigger a
    /// full resize pass so every terminal receives the correct new dimensions.
    fn split(&mut self, kind: Split, area: Rect) {
        let n     = self.focus_idx;
        let count = self.leaf_count();
        if count == 1 {
            let old = std::mem::replace(&mut self.root, Pane::new_connect());
            self.root = Pane::Split { kind, children: vec![old, Pane::new_connect()] };
        } else {
            self.root.split_leaf(n, kind);
        }
        self.root.resize_all(area, self.leaf_count() > 1);
    }

    /// Remove the focused leaf and clamp `focus_idx` to a valid index.
    fn close_focused(&mut self) {
        let target = self.focus_idx;
        remove_leaf(&mut self.root, target);
        if self.focus_idx >= self.leaf_count().max(1) {
            self.focus_idx = self.leaf_count().saturating_sub(1);
        }
    }

    /// Return the terminal-absolute cursor position for the focused session,
    /// or `None` if the focused pane is not a session or cursor is hidden.
    fn focused_cursor(&self, content: Rect) -> Option<(u16, u16)> {
        let areas     = self.root.leaf_areas(content);
        let pane_area = areas.get(self.focus_idx)?;
        let leaf_count = self.leaf_count();
        let inner = if leaf_count > 1 { pane_inner(*pane_area) } else { *pane_area };
        if let Some(Pane::Session { terminal }) = self.root.leaf(self.focus_idx) {
            if let Some((cx, cy)) = terminal.cursor_pos() {
                let sx = inner.x + cx;
                let sy = inner.y + cy;
                if sx < inner.x + inner.width && sy < inner.y + inner.height {
                    return Some((sx, sy));
                }
            }
        }
        None
    }
}

/// Remove the `n`-th leaf (DFS order) from the tree.
fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect { .. } | Pane::Session { .. } | Pane::FileBrowser { .. } => {}
        Pane::Split { children, .. } => {
            let mut offset    = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 { to_remove = Some(i); }
                    else { remove_leaf(child, n - offset); }
                    break;
                }
                offset += count;
            }
            if let Some(i) = to_remove { children.remove(i); }
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Top-level application state.
struct App {
    tabs:         Vec<Tab>,
    selected_tab: usize,
    /// Hosts parsed from `~/.ssh/config` at startup.
    hosts:        Vec<SshHost>,
    /// `Some` when the process was launched with `--debug`; `None` otherwise.
    log:          Option<Arc<Mutex<std::fs::File>>>,
    /// Tracks which pane a mouse-drag originated from (DFS leaf index).
    /// Used to implement intra-TUI drag-and-drop between browser panels.
    drag_origin:  Option<usize>,
}

impl App {
    fn new(log: Option<Arc<Mutex<std::fs::File>>>) -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: parse_ssh_config(),
            log,
            drag_origin: None,
        }
    }

    fn tab(&self)     -> &Tab     { &self.tabs[self.selected_tab] }
    fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.selected_tab] }

    /// Return `true` if any session in any tab has produced new output.
    fn any_dirty(&mut self) -> bool {
        self.tabs.iter_mut().any(|t| t.root.any_dirty())
    }

    /// Tick all file browsers in all tabs so their SFTP state machines advance.
    fn tick_browsers(&mut self) {
        for tab in &mut self.tabs {
            tab.root.tick_browsers();
        }
    }

    /// Forward a raw escape / control sequence to the focused session.
    fn send_str(&mut self, s: &str) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_str(s);
        }
    }

    /// Forward a single Unicode character to the focused session.
    fn send_char(&mut self, c: char) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_char(c);
        }
    }

    /// Open an SSH session to `hosts[host_idx]` in the currently focused connect pane.
    fn open_session(&mut self, host_idx: usize, area: Rect) -> Result<()> {
        let host      = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let pane_area = self.focused_pane_area(area);
        let term_area = if self.tab().leaf_count() > 1 { pane_inner(pane_area) } else { pane_area };
        let term      = EmbeddedTerminal::ssh(term_area.height, term_area.width, &host.label, self.log.clone())?;
        if self.tab().leaf_count() == 1 { self.tab_mut().name = host.label.clone(); }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::Session { terminal: term };
        }
        Ok(())
    }

    /// Open a `FileBrowser` to `hosts[host_idx]` in the currently focused pane.
    fn open_browser(&mut self, host_idx: usize) -> Result<()> {
        let host    = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let browser = FileBrowser::new(&host.label, self.log.clone())?;
        if self.tab().leaf_count() == 1 { self.tab_mut().name = format!("sftp:{}", host.label); }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::FileBrowser { browser };
        }
        Ok(())
    }

    fn focused_pane_area(&self, full: Rect) -> Rect {
        let content = content_area(full);
        let areas   = self.tab().root.leaf_areas(content);
        areas.get(self.tab().focus_idx).copied().unwrap_or(content)
    }

    /// Propagate the current terminal size to every session in every tab.
    fn resize_all(&mut self, full: Rect) {
        let content = content_area(full);
        for tab in &mut self.tabs {
            let multi = tab.leaf_count() > 1;
            tab.root.resize_all(content, multi);
        }
    }

    fn new_tab(&mut self) {
        let name = (self.tabs.len() + 1).to_string();
        self.tabs.push(Tab::new(&name));
        self.selected_tab = self.tabs.len() - 1;
    }

    fn close_tab(&mut self) {
        self.tabs.remove(self.selected_tab);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new("1"));
            self.selected_tab = 0;
        } else if self.selected_tab >= self.tabs.len() {
            self.selected_tab = self.tabs.len() - 1;
        }
    }

    // ------------------------------------------------------------------
    // Render
    // ------------------------------------------------------------------

    fn render(&mut self, full: Rect, buf: &mut Buffer) {
        let mut spans: Vec<Span> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 { spans.push(Span::raw(" │ ").style(Style::default().fg(Color::DarkGray))); }
            let span = Span::raw(format!(" {} ", tab.display_name()));
            if i == self.selected_tab {
                spans.push(span.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(span.style(Style::default().fg(Color::White)));
            }
        }

        let outer_block = Block::default().borders(Borders::ALL).title(Line::from(spans));
        let content = outer_block.inner(full);
        outer_block.render(full, buf);

        let focus_idx  = self.tabs[self.selected_tab].focus_idx;
        let hosts      = &self.hosts;
        let mut idx    = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab].root.render(content, buf, hosts, focus_idx, leaf_count, &mut idx);
    }
}

/// The drawable area inside the outer application border (1-cell inset on all sides).
fn content_area(full: Rect) -> Rect {
    Rect { x: full.x + 1, y: full.y + 1, width: full.width.saturating_sub(2), height: full.height.saturating_sub(2) }
}

/// The drawable area inside a pane's own border (1-cell inset on all sides).
fn pane_inner(area: Rect) -> Rect {
    Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(2) }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Enable debug logging by passing `--debug` on the command line.
    // When active, structured events are written to `debug.log` in the
    // working directory.  Omitting the flag silences all log output.
    let debug = std::env::args().any(|a| a == "--debug");
    let log_file: Option<Arc<Mutex<std::fs::File>>> = if debug {
        Some(Arc::new(Mutex::new(std::fs::File::create("debug.log")?)))
    } else {
        None
    };

    // Enter the alternate screen so the TUI does not clobber the user's
    // scrollback, and enable raw mode so key events arrive unfiltered.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;

    let backend  = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app       = App::new(log_file.clone());
    let mut last_area = { let s = terminal.size()?; Rect { x: 0, y: 0, width: s.width, height: s.height } };
    // Tracks whether mouse capture has been re-enabled after ratatui's draw
    // cycle temporarily disables it; we keep it permanently active.
    let mut host_mouse_captured = false;

    loop {
        // Drain any pending OS-level events; the short timeout lets us also
        // service PTY-dirty redraws without busy-waiting at 100 % CPU.
        event::poll(Duration::from_millis(5))?;

        // Advance SFTP state machines on every tick so they respond to
        // subprocess output even when no key event has occurred.
        app.tick_browsers();

        let needs_draw = app.any_dirty();

        // ratatui calls `DisableMouseCapture` at the end of every draw; we
        // immediately re-enable it here so click-to-focus always works.
        if !host_mouse_captured {
            execute!(terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
            host_mouse_captured = true;
        }

        let mut had_event = false;
        while event::poll(Duration::ZERO)? {
            had_event = true;
            match event::read()? {
                Event::Key(key) => {
                    // Ignore key-release and key-repeat events; act on presses only.
                    if key.kind != KeyEventKind::Press { continue; }

                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt  = key.modifiers.contains(KeyModifiers::ALT);

                    // ---- Global shortcuts (Alt+...) ----
                    if alt && !ctrl {
                        match key.code {
                            KeyCode::Left  => {
                                if app.selected_tab > 0 { app.selected_tab -= 1; }
                                else { app.selected_tab = app.tabs.len() - 1; }
                            }
                            KeyCode::Right => {
                                app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
                            }
                            KeyCode::Up    => app.tab_mut().focus_prev(),
                            KeyCode::Down  => app.tab_mut().focus_next(),
                            KeyCode::Char('w') => {
                                let was_last_pane = app.tab().leaf_count() == 1;
                                if was_last_pane {
                                    app.close_tab();
                                } else {
                                    app.tab_mut().close_focused();
                                    // Surviving panes now occupy more screen space;
                                    // resize all PTYs to reflect the new layout.
                                    app.resize_all(last_area);
                                }
                            }
                            KeyCode::Char('t') => app.new_tab(),
                            KeyCode::Char('-') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Vertical, content_area(area));
                            }
                            KeyCode::Char('+') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Horizontal, content_area(area));
                            }
                            // Alt+B: open a file browser in the focused connect pane.
                            KeyCode::Char('b') => {
                                let focus_idx = app.tabs[app.selected_tab].focus_idx;
                                let focused_is_connect = matches!(
                                    app.tabs[app.selected_tab].root.leaf(focus_idx),
                                    Some(Pane::Connect { .. })
                                );
                                if focused_is_connect {
                                    // Use the selected host from the connect list.
                                    let selected = if let Some(Pane::Connect { list_state }) =
                                        app.tab_mut().focused_pane_mut()
                                    { list_state.selected() } else { None };
                                    if let Some(idx) = selected {
                                        if let Err(e) = app.open_browser(idx) {
                                            log!(log_file, "open_browser error: {}", e);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ---- Connect pane key handling ----
                    let focus_idx = app.tabs[app.selected_tab].focus_idx;
                    let focused_is_connect = matches!(
                        app.tabs[app.selected_tab].root.leaf_mut(focus_idx),
                        Some(Pane::Connect { .. })
                    );

                    if focused_is_connect {
                        match key.code {
                            KeyCode::Char('c') if ctrl && !alt => {
                                disable_raw_mode()?;
                                execute!(terminal.backend_mut(), LeaveAlternateScreen, crossterm::event::DisableMouseCapture)?;
                                terminal.show_cursor()?;
                                return Ok(());
                            }
                            KeyCode::Up   | KeyCode::Char('k') => {
                                if let Some(Pane::Connect { list_state }) = app.tab_mut().focused_pane_mut() {
                                    list_state.select_previous();
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(Pane::Connect { list_state }) = app.tab_mut().focused_pane_mut() {
                                    list_state.select_next();
                                }
                            }
                            KeyCode::Enter => {
                                let selected = if let Some(Pane::Connect { list_state }) = app.tab_mut().focused_pane_mut() {
                                    list_state.selected()
                                } else { None };
                                if let Some(idx) = selected {
                                    if let Err(e) = app.open_session(idx, last_area) {
                                        log!(log_file, "open_session error: {}", e);
                                    }
                                    app.resize_all(last_area);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ---- FileBrowser pane key handling ----
                    let focus_idx = app.tabs[app.selected_tab].focus_idx;
                    let focused_is_browser = matches!(
                        app.tabs[app.selected_tab].root.leaf(focus_idx),
                        Some(Pane::FileBrowser { .. })
                    );

                    if focused_is_browser {
                        if let Some(Pane::FileBrowser { browser }) = app.tab_mut().focused_pane_mut() {
                            // When a deletion confirmation is pending, only y/n/Esc are active.
                            if browser.confirm_delete.is_some() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => browser.confirm_delete_yes(),
                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => browser.confirm_delete_no(),
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Tab                  => {
                                        browser.focus = if browser.focus == BrowserFocus::Local {
                                            BrowserFocus::Remote
                                        } else {
                                            BrowserFocus::Local
                                        };
                                    }
                                    KeyCode::Up                   => browser.nav_up(),
                                    KeyCode::Down                 => browser.nav_down(),
                                    KeyCode::Char(' ') | KeyCode::Enter => browser.enter(),
                                    KeyCode::Backspace            => browser.go_up(),
                                    KeyCode::F(5)                 => browser.download(),
                                    KeyCode::F(6)                 => browser.upload(),
                                    KeyCode::Delete               => browser.delete_focused(),
                                    KeyCode::Char('c') if ctrl    => {
                                        disable_raw_mode()?;
                                        execute!(terminal.backend_mut(), LeaveAlternateScreen, crossterm::event::DisableMouseCapture)?;
                                        terminal.show_cursor()?;
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        continue;
                    }

                    // ---- Active session key handling ----

                    // Ctrl+Arrow: emit xterm modifier escape sequences
                    // (`ESC [ 1 ; 5 D/C/A/B`) so that word-jump navigation
                    // works in bash, nano, zsh, and other readline-based apps.
                    if ctrl && !alt {
                        match key.code {
                            KeyCode::Left  => { app.send_str("\x1b[1;5D"); continue; }
                            KeyCode::Right => { app.send_str("\x1b[1;5C"); continue; }
                            KeyCode::Up    => { app.send_str("\x1b[1;5A"); continue; }
                            KeyCode::Down  => { app.send_str("\x1b[1;5B"); continue; }
                            _ => {}
                        }
                    }

                    match key.code {
                        // Convert Ctrl+<letter> to the corresponding C0 control code.
                        KeyCode::Char(c) if ctrl && !alt => {
                            let code = (c as u8).to_ascii_uppercase().wrapping_sub(b'@');
                            app.send_str(&String::from_utf8_lossy(&[code]));
                        }
                        KeyCode::Char(c) => {
                            // Shift is already encoded in the char value for printable keys.
                            app.send_char(c);
                        }
                        // Map special keys to their standard VT/xterm escape sequences.
                        KeyCode::Enter     => app.send_str("\r"),
                        KeyCode::Backspace => app.send_str("\x7f"),
                        KeyCode::Delete    => app.send_str("\x1b[3~"),
                        KeyCode::Tab       => app.send_str("\t"),
                        KeyCode::BackTab   => app.send_str("\x1b[Z"),
                        KeyCode::Left      => app.send_str("\x1b[D"),
                        KeyCode::Right     => app.send_str("\x1b[C"),
                        KeyCode::Up        => app.send_str("\x1b[A"),
                        KeyCode::Down      => app.send_str("\x1b[B"),
                        KeyCode::Home      => app.send_str("\x1b[H"),
                        KeyCode::End       => app.send_str("\x1b[F"),
                        KeyCode::PageUp    => app.send_str("\x1b[5~"),
                        KeyCode::PageDown  => app.send_str("\x1b[6~"),
                        KeyCode::F(n) => {
                            let seq = match n {
                                1=>"OP", 2=>"OQ", 3=>"OR", 4=>"OS",
                                5=>"\x1b[15~", 6=>"\x1b[17~", 7=>"\x1b[18~", 8=>"\x1b[19~",
                                9=>"\x1b[20~", 10=>"\x1b[21~", 11=>"\x1b[23~", 12=>"\x1b[24~",
                                _ => "",
                            };
                            if !seq.is_empty() { app.send_str(seq); }
                        }
                        _ => {}
                    }
                }

                Event::Mouse(mouse) => {
                    let content = content_area(last_area);
                    let areas   = app.tabs[app.selected_tab].root.leaf_areas(content);

                    // Determine which pane was hit by the mouse event.
                    let clicked_pane = areas.iter().enumerate().find(|(_, area)| {
                        mouse.column >= area.x && mouse.column < area.x + area.width
                            && mouse.row >= area.y && mouse.row < area.y + area.height
                    }).map(|(i, area)| (i, *area));

                    if let Some((pane_idx, pane_area)) = clicked_pane {
                        let prev_focus = app.tabs[app.selected_tab].focus_idx;

                        // A mouse-down on any pane switches focus to it.
                        if matches!(mouse.kind, MouseEventKind::Down(_)) {
                            app.tabs[app.selected_tab].focus_idx = pane_idx;
                            app.drag_origin = Some(pane_idx);
                        }

                        // ---- FileBrowser mouse: drag-and-drop + panel click ----
                        let is_browser = matches!(
                            app.tabs[app.selected_tab].root.leaf(pane_idx),
                            Some(Pane::FileBrowser { .. })
                        );
                        if is_browser {
                            if let MouseEventKind::Up(_) = mouse.kind {
                                // Check if this is the end of a cross-pane drag.
                                let origin = app.drag_origin.take();
                                let origin_is_browser = origin.map(|o| o != pane_idx && matches!(
                                    app.tabs[app.selected_tab].root.leaf(o),
                                    Some(Pane::FileBrowser { .. })
                                )).unwrap_or(false);

                                if origin_is_browser {
                                    // Drag ended on a different browser pane — not applicable
                                    // in this layout (we only have one browser per pane), so
                                    // treat as a normal click.
                                } else if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    // Determine which panel the mouse is in based on x position.
                                    let inner = pane_inner(pane_area);
                                    let half  = inner.width / 2;
                                    let in_remote = mouse.column >= inner.x + half;
                                    browser.focus = if in_remote { BrowserFocus::Remote } else { BrowserFocus::Local };
                                }
                            }

                            // Drag within a single browser pane: mouse-down in one
                            // panel, mouse-up in the other triggers a transfer.
                            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                                if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner = pane_inner(pane_area);
                                    let half  = inner.width / 2;
                                    let in_remote = mouse.column >= inner.x + half;
                                    // Store which side the drag started on.
                                    browser.focus = if in_remote { BrowserFocus::Remote } else { BrowserFocus::Local };
                                }
                            }

                            if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
                                // On drag-release (handled in Up above) we'll trigger the transfer.
                                // Nothing to do during the drag itself.
                            }

                            // Mouse-up: if drag started in opposite panel, transfer.
                            if let MouseEventKind::Up(MouseButton::Left) = mouse.kind {
                                if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner    = pane_inner(pane_area);
                                    let half     = inner.width / 2;
                                    let in_remote = mouse.column >= inner.x + half;
                                    let prev_focus_panel = browser.focus;
                                    // If mouse released in the opposite panel from where
                                    // the drag started, trigger a transfer.
                                    if in_remote && prev_focus_panel == BrowserFocus::Local {
                                        browser.drag_local_to_remote();
                                    } else if !in_remote && prev_focus_panel == BrowserFocus::Remote {
                                        browser.drag_remote_to_local();
                                    }
                                }
                            }

                            continue;
                        }

                        app.drag_origin = None;

                        // Only forward the mouse sequence to the PTY when:
                        //   1. The event targets the already-focused pane, and
                        //   2. The remote application has requested mouse reporting.
                        let same_pane = pane_idx == prev_focus;
                        let pane_wants_mouse = app.tabs[app.selected_tab]
                            .root.leaf_mut(pane_idx)
                            .map(|p| if let Pane::Session { terminal } = p {
                                terminal.mouse_active.load(Ordering::Acquire)
                            } else { false })
                            .unwrap_or(false);

                        if same_pane && pane_wants_mouse {
                            let leaf_count = app.tabs[app.selected_tab].root.leaf_count();
                            let inner = if leaf_count > 1 { pane_inner(pane_area) } else { pane_area };
                            // Translate absolute screen coordinates to pane-local coordinates.
                            let col = (mouse.column as i32 - inner.x as i32).max(0) as u16;
                            let row = (mouse.row    as i32 - inner.y as i32).max(0) as u16;
                            // Encode as SGR mouse sequences (1-based column/row).
                            let seq = match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left)   => format!("\x1b[<0;{};{}M", col+1, row+1),
                                MouseEventKind::Up(MouseButton::Left)     => format!("\x1b[<0;{};{}m", col+1, row+1),
                                MouseEventKind::Down(MouseButton::Right)  => format!("\x1b[<2;{};{}M", col+1, row+1),
                                MouseEventKind::Up(MouseButton::Right)    => format!("\x1b[<2;{};{}m", col+1, row+1),
                                MouseEventKind::Down(MouseButton::Middle) => format!("\x1b[<1;{};{}M", col+1, row+1),
                                MouseEventKind::Up(MouseButton::Middle)   => format!("\x1b[<1;{};{}m", col+1, row+1),
                                MouseEventKind::ScrollUp                  => format!("\x1b[<64;{};{}M", col+1, row+1),
                                MouseEventKind::ScrollDown                => format!("\x1b[<65;{};{}M", col+1, row+1),
                                MouseEventKind::Drag(MouseButton::Left)   => format!("\x1b[<32;{};{}M", col+1, row+1),
                                _ => String::new(),
                            };
                            if !seq.is_empty() { app.send_str(&seq); }
                        }
                    }
                }

                Event::Resize(w, h) => {
                    last_area = Rect { x: 0, y: 0, width: w, height: h };
                    app.resize_all(last_area);
                    log!(log_file, "resize {}x{}", w, h);
                }
                _ => {}
            }
        }

        if needs_draw || had_event {
            terminal.draw(|f| {
                last_area = f.area();
                // Re-run resize propagation when dirty output implies the
                // terminal may have been resized between draw calls.
                if needs_draw { app.resize_all(last_area); }
                app.render(last_area, f.buffer_mut());

                // Place the hardware cursor at the focused session's cursor
                // position so the blinking caret appears in the correct cell.
                let content = content_area(last_area);
                if let Some((cx, cy)) = app.tabs[app.selected_tab].focused_cursor(content) {
                    f.set_cursor_position((cx, cy));
                }
            })?;
        }
    }
}