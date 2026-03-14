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

struct EmbeddedTerminal {
    parser:       Arc<Mutex<vt100::Parser>>,
    master:       Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer:       Arc<Mutex<Box<dyn Write + Send>>>,
    dirty:        Arc<AtomicBool>,
    mouse_active: Arc<AtomicBool>,
    rows:         u16,
    cols:         u16,
}

impl EmbeddedTerminal {
    fn new(rows: u16, cols: u16, ssh_host: &str, log: Arc<Mutex<std::fs::File>>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let mut cmd = CommandBuilder::new("ssh");
        cmd.arg(ssh_host);
        cmd.arg("-t");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        pair.slave.spawn_command(cmd)?;
        log!(log, "SSH spawned {}x{} host={}", cols, rows, ssh_host);

        let parser       = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let dirty        = Arc::new(AtomicBool::new(false));
        let mouse_active = Arc::new(AtomicBool::new(false));

        let parser_c       = Arc::clone(&parser);
        let writer_c       = Arc::clone(&writer);
        let dirty_c        = Arc::clone(&dirty);
        let mouse_active_c = Arc::clone(&mouse_active);
        let log_c          = Arc::clone(&log);

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => { log!(log_c, "PTY EOF"); break; }
                    Ok(n) => {
                        let data = &buf[..n];
                        if let Ok(mut p) = parser_c.lock() { p.process(data); }
                        dirty_c.store(true, Ordering::Release);

                        // Mouse enable/disable detection
                        let mut i = 0;
                        while i + 2 < data.len() {
                            if data[i] == 0x1b && data[i+1] == b'[' && data[i+2] == b'?' {
                                let start = i + 3;
                                let mut end = start;
                                while end < data.len() && data[end] != b'h' && data[end] != b'l' { end += 1; }
                                if end < data.len() {
                                    if let Ok(params) = std::str::from_utf8(&data[start..end]) {
                                        let mouse = params.split(';').any(|p| matches!(p.trim(), "1000"|"1002"|"1003"|"1006"));
                                        if mouse {
                                            mouse_active_c.store(data[end] == b'h', Ordering::Release);
                                        }
                                    }
                                    i = end + 1;
                                    continue;
                                }
                            }
                            i += 1;
                        }

                        // DSR reply
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
        Ok(Self { parser, master, writer, dirty, mouse_active, rows, cols })
    }

    fn send_str(&mut self, s: &str) {
        if let Ok(mut w) = self.writer.lock() { let _ = w.write_all(s.as_bytes()); }
    }

    fn send_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.send_str(c.encode_utf8(&mut buf));
    }

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

    fn render_into(&self, area: Rect, buf: &mut Buffer) {
        let Ok(parser) = self.parser.try_lock() else { return };
        let screen = parser.screen();

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
    }
}

// ---------------------------------------------------------------------------
// SSH host config
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SshHost { label: String }

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

// Each tab contains a tree of panes. A pane is either a leaf (connect menu
// or live session) or an internal split node (horizontal or vertical).

enum Split { Horizontal, Vertical }

enum Pane {
    Connect { list_state: ListState },
    Session { terminal: EmbeddedTerminal },
    Split   { kind: Split, children: Vec<Pane> },
}

impl Pane {
    fn new_connect() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        Pane::Connect { list_state: ls }
    }

    // Collect all leaf pane areas for focus tracking (DFS order)
    fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => vec![area],
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                children.iter().zip(areas).flat_map(|(c, a)| c.leaf_areas(a)).collect()
            }
        }
    }

    // Number of leaves
    fn leaf_count(&self) -> usize {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => 1,
            Pane::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    // Get mutable reference to the Nth leaf (DFS order)
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

    // Replace the Nth leaf with a horizontal or vertical split containing
    // the original pane and a new connect pane
    fn split_leaf(&mut self, n: usize, kind: Split) {
        match self {
            Pane::Connect { .. } | Pane::Session { .. } => {}
            Pane::Split { children, .. } => {
                let mut offset = 0;
                for (i, child) in children.iter_mut().enumerate() {
                    let count = child.leaf_count();
                    if n < offset + count {
                        if count == 1 {
                            // Replace the leaf child with a split
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

    fn any_dirty(&self) -> bool {
        match self {
            Pane::Session { terminal } => terminal.dirty.swap(false, Ordering::AcqRel),
            Pane::Split { children, .. } => children.iter().any(|c| c.any_dirty()),
            _ => false,
        }
    }

    fn resize_all(&mut self, area: Rect) {
        match self {
            Pane::Session { terminal } => terminal.resize(area.height, area.width),
            Pane::Split { kind, children } => {
                let areas = split_areas(area, kind, children.len());
                for (child, a) in children.iter_mut().zip(areas) {
                    child.resize_all(a);
                }
            }
            _ => {}
        }
    }



    fn render(&mut self, area: Rect, buf: &mut Buffer, hosts: &[SshHost], focus_idx: usize, leaf_count: usize, my_idx: &mut usize) {
        match self {
            Pane::Connect { list_state } => {
                let is_focus = *my_idx == focus_idx;
                *my_idx += 1;
                let inner = if leaf_count > 1 {
                    // Multiple panes: draw a border, blue if focused
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
                    // Single pane: no border, use full area
                    area
                };
                // Split: host list on top, shortcut help at bottom
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

                // Shortcut help
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
                    // key in yellow, desc in dark gray
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
                    // Multiple panes: draw a border, blue if focused
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
                    // Single pane: no border, full area
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

struct Tab {
    name:      String,
    root:      Pane,
    focus_idx: usize, // which leaf is focused (DFS order)
}

impl Tab {
    fn new(name: &str) -> Self {
        Tab { name: name.to_string(), root: Pane::new_connect(), focus_idx: 0 }
    }

    fn leaf_count(&self) -> usize { self.root.leaf_count() }

    fn focus_next(&mut self) {
        self.focus_idx = (self.focus_idx + 1) % self.leaf_count();
    }

    fn focus_prev(&mut self) {
        if self.focus_idx == 0 { self.focus_idx = self.leaf_count() - 1; }
        else { self.focus_idx -= 1; }
    }

    // When there's only one pane, show its content label in the tab bar
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

    fn split(&mut self, kind: Split, area: Rect) {
        let n = self.focus_idx;
        let count = self.leaf_count();
        if count == 1 {
            // Root is a leaf — replace root directly
            let old = std::mem::replace(&mut self.root, Pane::new_connect());
            self.root = Pane::Split { kind, children: vec![old, Pane::new_connect()] };
        } else {
            self.root.split_leaf(n, kind);
        }
        // Focus stays on the same index (the original pane)
        // Resize all panes to their new areas
        self.root.resize_all(area);
    }

    fn close_focused(&mut self) {
        let target = self.focus_idx;
        remove_leaf(&mut self.root, target);
        if self.focus_idx >= self.leaf_count().max(1) {
            self.focus_idx = self.leaf_count().saturating_sub(1);
        }
    }
}

// Remove the Nth leaf from the pane tree, collapsing single-child splits
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

struct App {
    tabs:         Vec<Tab>,
    selected_tab: usize,
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

    fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.root.any_dirty())
    }



    fn send_str(&mut self, s: &str) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_str(s);
        }
    }

    fn send_char(&mut self, c: char) {
        if let Some(Pane::Session { terminal }) = self.tab_mut().focused_pane_mut() {
            terminal.send_char(c);
        }
    }

    // Open a session in the focused connect pane
    fn open_session(&mut self, host_idx: usize, area: Rect) -> Result<()> {
        let host = self.hosts.get(host_idx).cloned().ok_or_else(|| anyhow::anyhow!("invalid host"))?;
        let pane_area = self.focused_pane_area(area);
        let term_area = if self.tab().leaf_count() > 1 { pane_inner(pane_area) } else { pane_area };
        let term = EmbeddedTerminal::new(term_area.height, term_area.width, &host.label, Arc::clone(&self.log))?;
        // Update tab name to host label when it's a single-pane tab
        if self.tab().leaf_count() == 1 {
            self.tab_mut().name = host.label.clone();
        }
        if let Some(pane) = self.tab_mut().focused_pane_mut() {
            *pane = Pane::Session { terminal: term };
        }
        Ok(())
    }

    fn focused_pane_area(&self, full: Rect) -> Rect {
        let content = content_area(full);
        let areas = self.tab().root.leaf_areas(content);
        areas.get(self.tab().focus_idx).copied().unwrap_or(content)
    }

    fn resize_all(&mut self, full: Rect) {
        let content = content_area(full);
        for tab in &mut self.tabs {
            tab.root.resize_all(content);
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
            // Last tab closed — open a fresh one instead of leaving nothing
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
        // Tab bar spans
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

        // Render pane tree
        let focus_idx = self.tabs[self.selected_tab].focus_idx;
        let hosts = &self.hosts;
        let mut idx = 0;
        let leaf_count = self.tabs[self.selected_tab].root.leaf_count();
        self.tabs[self.selected_tab].root.render(content, buf, hosts, focus_idx, leaf_count, &mut idx);
    }
}

// The content area inside the outer block border
fn content_area(full: Rect) -> Rect {
    Rect {
        x:      full.x + 1,
        y:      full.y + 1,
        width:  full.width.saturating_sub(2),
        height: full.height.saturating_sub(2),
    }
}

// The inner area of a pane (inside its own border)
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

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(Arc::clone(&log_file));
    let mut last_area = { let s = terminal.size()?; Rect { x:0, y:0, width: s.width, height: s.height } };
    let mut host_mouse_captured = false;

    loop {
        event::poll(Duration::from_millis(5))?;
        let needs_draw = app.any_dirty();

        // Always keep mouse capture enabled so clicks work for pane focus
        // switching even when no remote app has requested mouse reporting.
        // Shift+click still works in Windows Terminal for text selection.
        if !host_mouse_captured {
            execute!(terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
            host_mouse_captured = true;
        }

        let mut had_event = false;
        while event::poll(Duration::ZERO)? {
            had_event = true;
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press { continue; }

                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt  = key.modifiers.contains(KeyModifiers::ALT);

                    // ---- Global shortcuts (Alt+...) ----
                    if alt && !ctrl {
                        match key.code {
                            // Alt+Left/Right: switch tabs
                            KeyCode::Left  => {
                                if app.selected_tab > 0 { app.selected_tab -= 1; }
                                else { app.selected_tab = app.tabs.len() - 1; }
                            }
                            KeyCode::Right => {
                                app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
                            }
                            // Alt+Up/Down : cycle focused pane
                            KeyCode::Up   => app.tab_mut().focus_prev(),
                            KeyCode::Down => app.tab_mut().focus_next(),
                            // Alt+W: close focused pane.
                            // If last pane: close tab (or reset to connect if last tab).
                            KeyCode::Char('w') => {
                                let was_last_pane = app.tab().leaf_count() == 1;
                                if was_last_pane {
                                    // Just close the whole tab — close_tab handles
                                    // the single-tab case by resetting to a fresh connect
                                    app.close_tab();
                                } else {
                                    app.tab_mut().close_focused();
                                }
                            }
                            // Alt+T: new tab
                            KeyCode::Char('t') => app.new_tab(),
                            // Alt+- : vertical split (top/bottom)
                            KeyCode::Char('-') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Vertical, content_area(area));
                            }
                            // Alt++ : horizontal split (left/right)
                            KeyCode::Char('+') => {
                                let area = last_area;
                                app.tab_mut().split(Split::Horizontal, content_area(area));
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ---- Connect pane ----
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
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ---- Active session ----
                    match key.code {
                        KeyCode::Char(c) if ctrl && !alt => {
                            let code = (c as u8).to_ascii_uppercase().wrapping_sub(b'@');
                            app.send_str(&String::from_utf8_lossy(&[code]));
                        }
                        KeyCode::Char(c) => {
                            // Shift is already encoded in the char for printable chars
                            app.send_char(c);
                        }
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
                                1=>"OP", 2=>"OQ", 3=>"OR", 4=>"OS",
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

                    // Find which pane was clicked
                    let clicked_pane = areas.iter().enumerate().find(|(_, area)| {
                        mouse.column >= area.x && mouse.column < area.x + area.width
                            && mouse.row >= area.y && mouse.row < area.y + area.height
                    }).map(|(i, area)| (i, *area));

                    if let Some((pane_idx, pane_area)) = clicked_pane {
                        let prev_focus = app.tabs[app.selected_tab].focus_idx;

                        // Always update focus on click — even when mouse capture is off
                        if matches!(mouse.kind, MouseEventKind::Down(_)) {
                            app.tabs[app.selected_tab].focus_idx = pane_idx;
                        }

                        // Only forward mouse sequences to the SAME pane that was
                        // already focused AND has mouse reporting active.
                        // If the click changed focus, swallow it (just focus change).
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
                            let col = (mouse.column as i32 - inner.x as i32).max(0) as u16;
                            let row = (mouse.row    as i32 - inner.y as i32).max(0) as u16;
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
                if needs_draw { app.resize_all(last_area); }
                app.render(last_area, f.buffer_mut());
            })?;
        }
    }
}