use std::{
    io::{Read, Write},
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
    widgets::{Block, Borders, List, ListState, StatefulWidget, Widget},
    Terminal,
};

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Write a line to the debug log file, ignoring lock failures.
/// Uses a macro so the call site can use `format!`-style arguments without
/// paying the formatting cost when the lock is unavailable.
macro_rules! log {
    ($log:expr, $($arg:tt)*) => {{
        if let Ok(mut f) = $log.lock() {
            writeln!(f, $($arg)*).ok();
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

/// A single pseudo-terminal session connected to a remote host via SSH.
///
/// Each `EmbeddedTerminal` owns:
/// - a `vt100::Parser` that maintains a virtual screen updated by the PTY
///   reader thread,
/// - the PTY master (for resize signals) and a writer half (for key input),
/// - three shared flags: `dirty` (set by the reader on new output, cleared on
///   render), `mouse_active` (tracks SGR mouse reporting state), and
///   `cursor_visible` (tracks DEC mode 25 show/hide cursor state).
///
/// The reader thread runs independently and communicates solely through the
/// `Arc`-shared fields above; no explicit join is performed — the thread
/// exits naturally when the PTY reaches EOF.
struct EmbeddedTerminal {
    /// VT100 screen state, updated by the reader thread.
    parser:       Arc<Mutex<vt100::Parser>>,
    /// PTY master handle, kept alive so the slave side stays open.
    master:       Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Write half of the PTY; used to forward key events to the remote shell.
    writer:       Arc<Mutex<Box<dyn Write + Send>>>,
    /// Set to `true` by the reader thread whenever new output is processed.
    /// Cleared (swapped to `false`) during the dirty-check phase of the event
    /// loop so that spurious redraws are avoided.
    dirty:        Arc<AtomicBool>,
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
}

impl EmbeddedTerminal {
    /// Spawn a PTY of `rows × cols` cells, connect it to `ssh <ssh_host> -t`,
    /// and start the background reader thread.
    fn new(rows: u16, cols: u16, ssh_host: &str, log: Arc<Mutex<std::fs::File>>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg(ssh_host);
        cmd.arg("-t");
        // Advertise full 256-colour and true-colour support so that remote
        // applications (vim, tmux, …) use the richest colour codes available.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        pair.slave.spawn_command(cmd)?;
        log!(log, "SSH spawned {}x{} host={}", cols, rows, ssh_host);

        let parser          = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let dirty           = Arc::new(AtomicBool::new(false));
        let mouse_active    = Arc::new(AtomicBool::new(false));
        // Cursor starts visible — DEC mode 25 is on by default.
        let cursor_visible  = Arc::new(AtomicBool::new(true));

        // Clone Arc handles for the reader thread before moving into the closure.
        let parser_c          = Arc::clone(&parser);
        let writer_c          = Arc::clone(&writer);
        let dirty_c           = Arc::clone(&dirty);
        let mouse_active_c    = Arc::clone(&mouse_active);
        let cursor_visible_c  = Arc::clone(&cursor_visible);
        let log_c             = Arc::clone(&log);

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
        Ok(Self { parser, master, writer, dirty, mouse_active, cursor_visible, rows, cols })
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
// Pane tree
// ---------------------------------------------------------------------------

/// Direction along which a split divides its available area.
enum Split { Horizontal, Vertical }

/// A node in the pane tree.
///
/// The tree is a binary-ish layout of leaf panes separated by split nodes.
/// Leaf variants:
/// - `Connect` — the host-picker screen shown before a session is opened.
/// - `Session` — a live `EmbeddedTerminal`.
///
/// Internal variant:
/// - `Split`   — divides its area among `children` horizontally or vertically,
///   each child being another `Pane` (possibly itself a split).
enum Pane {
    Connect { list_state: ListState },
    Session { terminal: EmbeddedTerminal },
    Split   { kind: Split, children: Vec<Pane> },
}

impl Pane {
    /// Create a `Connect` leaf with the first host pre-selected.
    fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect { list_state: ls }
    }

    /// Collect the screen rectangles of every leaf in DFS order.
    ///
    /// The returned `Vec` is index-aligned with the DFS leaf numbering used
    /// by `leaf_count`, `leaf_mut`, and `focused_cursor`.
    fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => vec![area],
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                children.iter().zip(areas).flat_map(|(c, a)| c.leaf_areas(a)).collect()
            }
        }
    }

    /// Total number of leaf panes (DFS count).
    fn leaf_count(&self) -> usize {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    /// Return a mutable reference to the `n`-th leaf in DFS order.
    fn leaf_mut(&mut self, n: usize) -> Option<&mut Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf_mut(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    /// Return a shared reference to the `n`-th leaf in DFS order.
    fn leaf(&self, n: usize) -> Option<&Pane> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => {
                if n == 0 { Some(self) } else { None }
            }
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    /// Replace the `n`-th leaf with a new split that contains the original
    /// leaf and a fresh `Connect` pane side-by-side.
    fn split_leaf(&mut self, n: usize, kind: Split) {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => {}
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for (i, child) in children.iter_mut().enumerate() {
                    let count = child.leaf_count();
                    if n < offset + count {
                        if count == 1 {
                            // Wrap this single leaf in a new split node.
                            let old = std::mem::replace(child, Pane::new_connect());
                            *child = Pane::Split {
                                kind,
                                children: vec![old, Pane::new_connect()],
                            };
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

    /// Return `true` if any session leaf has produced new output since the
    /// last call.  Clears the dirty flag as a side-effect (swap semantics).
    fn any_dirty(&self) -> bool {
        match self {
            Pane::Session { terminal } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::Split { children, .. } => children.iter().any(|c| c.any_dirty()),
            _ => false,
        }
    }

    /// Propagate a resize event down to every `Session` leaf, using the
    /// sub-area that the layout algorithm assigns to each node.
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
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    // Once we are inside a Split there is always more than one
                    // leaf, so every leaf will receive a border.
                    child.resize_all(a, true);
                }
            }
            _ => {}
        }
    }

    /// Recursively render the pane tree into `buf`.
    ///
    /// `focus_idx` is the DFS index of the currently focused leaf.
    /// `leaf_count` is the total leaf count for the whole tab — used to
    /// decide whether to draw per-pane borders (only when > 1).
    /// `my_idx` is a running DFS counter threaded through the recursion.
    fn render(&mut self, area: Rect, buf: &mut Buffer, hosts: &[SshHost], focus_idx: usize, leaf_count: usize, my_idx: &mut usize) {
        match self {
            Pane::Connect { list_state } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;

                // Draw a border only when multiple panes are visible; colour it
                // blue for the focused pane and dark-gray for the others.
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

                // Split the inner area: host list on top, keybinding hints at bottom.
                const HELP_LINES: u16 = 7;
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

                // Render the keybinding reference table.
                let shortcuts = [
                    ("Alt+T",    "new tab"),
                    ("Alt+W",    "close pane / tab"),
                    ("Alt+-",    "split vertical"),
                    ("Alt++",    "split horizontal"),
                    ("Alt+↑↓",   "cycle pane focus"),
                    ("Alt+←→",   "switch tab"),
                    ("Ctrl+C",   "quit"),
                ];
                for (i, (key, desc)) in shortcuts.iter().enumerate() {
                    let y = help_area.y + i as u16;
                    if y >= help_area.y + help_area.height { break; }
                    let key_span = Span::raw(format!("  {:10}", key))
                        .style(Style::default().fg(Color::Yellow));
                    let desc_span = Span::raw(*desc)
                        .style(Style::default().fg(Color::DarkGray));
                    let line = Line::from(vec![key_span, desc_span]);
                    buf.set_line(help_area.x, y, &line, help_area.width);
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
    fn focus_next(&mut self) {
        self.focus_idx = (self.focus_idx + 1) % self.leaf_count();
    }

    /// Move focus to the previous leaf (wraps around).
    fn focus_prev(&mut self) {
        if self.focus_idx == 0 { self.focus_idx = self.leaf_count() - 1; }
        else { self.focus_idx -= 1; }
    }

    /// Label shown in the tab bar.
    ///
    /// For a single-pane tab the label reflects the current pane type
    /// (`<connect>` or the session host name); for multi-pane tabs the
    /// original tab name is used.
    fn display_name(&self) -> &str {
        if self.leaf_count() == 1 {
            match &self.root {
                Pane::Connect { .. } => "<connect>",
                Pane::Session { .. } => &self.name,
                _                    => &self.name,
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
        let n = self.focus_idx;
        let count = self.leaf_count();
        if count == 1 {
            // Root is already a leaf; promote it to a split directly.
            let old = std::mem::replace(&mut self.root, Pane::new_connect());
            self.root = Pane::Split { kind, children: vec![old, Pane::new_connect()] };
        } else {
            self.root.split_leaf(n, kind);
        }
        // After a split the tree always has >1 leaf, so borders will be drawn.
        self.root.resize_all(area, self.leaf_count() > 1);
    }

    /// Remove the focused leaf from the pane tree and clamp `focus_idx`
    /// so it remains a valid DFS index after the removal.
    fn close_focused(&mut self) {
        let target = self.focus_idx;
        remove_leaf(&mut self.root, target);
        if self.focus_idx >= self.leaf_count().max(1) {
            self.focus_idx = self.leaf_count().saturating_sub(1);
        }
    }

    /// Return the terminal-absolute `(col, row)` position of the hardware
    /// cursor inside the focused session pane, or `None` if the focused pane
    /// is not a session or its cursor is hidden.
    ///
    /// Used by the main draw loop to call `frame.set_cursor_position` so that
    /// the hardware cursor blinks at the correct location on screen.
    fn focused_cursor(&self, content: Rect) -> Option<(u16, u16)> {
        let areas = self.root.leaf_areas(content);
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
///
/// After removal, any `Split` node left with a single child is automatically
/// collapsed so the child replaces the split in the tree.
fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect { .. } | Pane::Session { .. } => {}
        Pane::Split { children, .. } => {
            let mut offset = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 {
                        to_remove = Some(i);
                    } else {
                        remove_leaf(child, n - offset);
                    }
                    break;
                }
                offset += count;
            }
            if let Some(i) = to_remove {
                children.remove(i);
            }
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
    log:          Arc<Mutex<std::fs::File>>,
}

impl App {
    fn new(log: Arc<Mutex<std::fs::File>>) -> Self {
        App {
            tabs: vec![Tab::new("1")],
            selected_tab: 0,
            hosts: parse_ssh_config(),
            log,
        }
    }

    fn tab(&self) -> &Tab { &self.tabs[self.selected_tab] }
    fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.selected_tab] }

    /// Return `true` if any session in any tab has produced new output.
    fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.root.any_dirty())
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

    /// Open an SSH session to `hosts[host_idx]` in the currently focused
    /// connect pane, sizing the terminal to the pane's inner area.
    ///
    /// When the tab holds only one pane the tab name is updated to the
    /// host label so the tab bar reflects the active connection.
    fn open_session(&mut self, host_idx: usize, area: Rect) -> Result<()> {
        let host = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let pane_area = self.focused_pane_area(area);
        // Subtract the pane border when multiple panes are visible.
        let term_area = if self.tab().leaf_count() > 1 { pane_inner(pane_area) } else { pane_area };
        let term = EmbeddedTerminal::new(term_area.height, term_area.width, &host.label, Arc::clone(&self.log))?;
        if self.tab().leaf_count() == 1 {
            self.tab_mut().name = host.label.clone();
        }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::Session { terminal: term };
        }
        Ok(())
    }

    /// Return the screen rectangle occupied by the focused pane.
    fn focused_pane_area(&self, full: Rect) -> Rect {
        let content = content_area(full);
        let areas = self.tab().root.leaf_areas(content);
        areas.get(self.tab().focus_idx).copied().unwrap_or(content)
    }

    /// Propagate the current terminal size to every session in every tab.
    ///
    /// Called after window resize events and after layout changes (split /
    /// open session) to ensure each PTY has the correct dimensions.
    fn resize_all(&mut self, full: Rect) {
        let content = content_area(full);
        for tab in &mut self.tabs {
            let multi = tab.leaf_count() > 1;
            tab.root.resize_all(content, multi);
        }
    }

    /// Append a new tab and switch focus to it.
    fn new_tab(&mut self) {
        let name = (self.tabs.len() + 1).to_string();
        self.tabs.push(Tab::new(&name));
        self.selected_tab = self.tabs.len() - 1;
    }

    /// Close the active tab.
    ///
    /// If this was the last tab a fresh connect tab is opened so the
    /// application always has at least one tab visible.
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

    /// Draw the outer border + tab bar, then delegate to the pane tree.
    fn render(&mut self, full: Rect, buf: &mut Buffer) {
        // Build tab bar spans: active tab in bold yellow, others in white.
        let mut spans: Vec<Span> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" │ ").style(Style::default().fg(Color::DarkGray)));
            }
            let span = Span::raw(format!(" {} ", tab.display_name()));
            if i == self.selected_tab {
                spans.push(span.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(span.style(Style::default().fg(Color::White)));
            }
        }

        let outer_block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(spans));

        let content = outer_block.inner(full);
        outer_block.render(full, buf);

        let focus_idx = self.tabs[self.selected_tab].focus_idx;
        let hosts = &self.hosts;
        let mut idx = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab].root.render(content, buf, hosts, focus_idx, leaf_count, &mut idx);
    }
}

/// The drawable area inside the outer application border (1-cell inset on all sides).
fn content_area(full: Rect) -> Rect {
    Rect {
        x:      full.x + 1,
        y:      full.y + 1,
        width:  full.width.saturating_sub(2),
        height: full.height.saturating_sub(2),
    }
}

/// The drawable area inside a pane's own border (1-cell inset on all sides).
fn pane_inner(area: Rect) -> Rect {
    Rect {
        x:      area.x + 1,
        y:      area.y + 1,
        width:  area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let log_file = Arc::new(Mutex::new(std::fs::File::create("debug.log")?));

    // Enter the alternate screen so the TUI does not clobber the user's
    // scrollback, and enable raw mode so key events arrive unfiltered.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(Arc::clone(&log_file));
    let mut last_area = { let s = terminal.size()?; Rect { x:0, y:0, width: s.width, height: s.height } };
    // Tracks whether mouse capture has been re-enabled after ratatui's draw
    // cycle temporarily disables it; we keep it permanently active.
    let mut host_mouse_captured = false;

    loop {
        // Drain any pending OS-level events; the short timeout lets us also
        // service PTY-dirty redraws without busy-waiting at 100 % CPU.
        event::poll(Duration::from_millis(5))?;
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
                    // Intercepted before any pane-specific handling so they
                    // work regardless of which pane is focused.
                    if alt && !ctrl {
                        match key.code {
                            // Cycle through tabs.
                            KeyCode::Left  => {
                                if app.selected_tab > 0 { app.selected_tab -= 1; }
                                else { app.selected_tab = app.tabs.len() - 1; }
                            }
                            KeyCode::Right => {
                                app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
                            }
                            // Cycle focus across panes within the current tab.
                            KeyCode::Up   => app.tab_mut().focus_prev(),
                            KeyCode::Down => app.tab_mut().focus_next(),
                            // Close focused pane; close the whole tab when it was the
                            // last pane (close_tab handles the single-tab edge case).
                            KeyCode::Char('w') => {
                                let was_last_pane = app.tab().leaf_count() == 1;
                                if was_last_pane {
                                    app.close_tab();
                                } else {
                                    app.tab_mut().close_focused();
                                }
                            }
                            KeyCode::Char('t') => app.new_tab(),
                            // Alt+- : top/bottom split.
                            KeyCode::Char('-') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Vertical, content_area(area));
                            }
                            // Alt++ : left/right split.
                            KeyCode::Char('+') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Horizontal, content_area(area));
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
                                // Restore the terminal to its original state before exiting.
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
                                    // Resize all panes after opening a session so the new
                                    // terminal gets the correct dimensions immediately,
                                    // including when placed inside a split.
                                    app.resize_all(last_area);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ---- Active session key handling ----

                    // Ctrl+Arrow: emit xterm modifier escape sequences
                    // (`ESC [ 1 ; 5 D/C/A/B`) so that word-jump navigation
                    // works in bash, nano, zsh, and other readline-based apps.
                    // Must be checked before the generic Ctrl branch below,
                    // which would otherwise consume these codes incorrectly.
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
                    let areas = app.tabs[app.selected_tab].root.leaf_areas(content);

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
                        }

                        // Only forward the mouse sequence to the PTY when:
                        //   1. The event targets the already-focused pane (so a
                        //      cross-pane click is consumed as a focus change only), and
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
                    last_area = Rect { x:0, y:0, width: w, height: h };
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