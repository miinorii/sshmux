use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};

use std::io::Write;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Module declarations
// ---------------------------------------------------------------------------

mod app;
mod pane;
mod sftp;
mod sftp_parse;
mod ssh_config;
mod tab;
mod terminal;

use app::{App, content_area};
use pane::{Pane, Split, pane_inner};
use sftp::BrowserFocus;

// ---------------------------------------------------------------------------
// Logging macro (available to all modules via #[macro_use] or re-export)
// ---------------------------------------------------------------------------

/// Write a line to the debug log file.
/// When `$log` is `None` (debug mode not enabled) this is a no-op.
#[macro_export]
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
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let debug = std::env::args().any(|a| a == "--debug");
    let log_file: Option<Arc<Mutex<std::fs::File>>> = if debug {
        Some(Arc::new(Mutex::new(std::fs::File::create("debug.log")?)))
    } else {
        None
    };

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(log_file.clone());
    let mut last_area = {
        let s = terminal.size()?;
        Rect {
            x: 0,
            y: 0,
            width: s.width,
            height: s.height,
        }
    };
    let mut host_mouse_captured = false;

    loop {
        event::poll(Duration::from_millis(5))?;

        app.tick_browsers();

        let needs_draw = app.any_dirty();

        if !host_mouse_captured {
            execute!(terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
            host_mouse_captured = true;
        }

        let mut had_event = false;
        while event::poll(Duration::ZERO)? {
            had_event = true;
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);

                    // ---- Global shortcuts (Alt+...) ----
                    if alt && !ctrl {
                        match key.code {
                            KeyCode::Left => {
                                if app.selected_tab > 0 {
                                    app.selected_tab -= 1;
                                } else {
                                    app.selected_tab = app.tabs.len() - 1;
                                }
                            }
                            KeyCode::Right => {
                                app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
                            }
                            KeyCode::Up => app.tab_mut().focus_prev(),
                            KeyCode::Down => app.tab_mut().focus_next(),
                            KeyCode::Char('w') => {
                                let was_last_pane = app.tab().leaf_count() == 1;
                                if was_last_pane {
                                    app.close_tab();
                                } else {
                                    app.tab_mut().close_focused();
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
                            KeyCode::Char('b') => {
                                let focus_idx = app.tabs[app.selected_tab].focus_idx;
                                let focused_is_connect = matches!(
                                    app.tabs[app.selected_tab].root.leaf(focus_idx),
                                    Some(Pane::Connect { .. })
                                );
                                if focused_is_connect {
                                    let selected = if let Some(Pane::Connect { list_state }) =
                                        app.tab_mut().focused_pane_mut()
                                    {
                                        list_state.selected()
                                    } else {
                                        None
                                    };
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
                                execute!(
                                    terminal.backend_mut(),
                                    LeaveAlternateScreen,
                                    crossterm::event::DisableMouseCapture
                                )?;
                                terminal.show_cursor()?;
                                return Ok(());
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if let Some(Pane::Connect { list_state }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.select_previous();
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(Pane::Connect { list_state }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.select_next();
                                }
                            }
                            KeyCode::Enter => {
                                let selected = if let Some(Pane::Connect { list_state }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.selected()
                                } else {
                                    None
                                };
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

                    // ---- FileBrowser pane ----
                    let focus_idx = app.tabs[app.selected_tab].focus_idx;
                    let focused_is_browser = matches!(
                        app.tabs[app.selected_tab].root.leaf(focus_idx),
                        Some(Pane::FileBrowser { .. })
                    );

                    if focused_is_browser {
                        if let Some(Pane::FileBrowser { browser }) =
                            app.tab_mut().focused_pane_mut()
                        {
                            if browser.confirm_delete.is_some() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                        browser.confirm_delete_yes()
                                    }
                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                        browser.confirm_delete_no()
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Tab => {
                                        browser.focus = if browser.focus == BrowserFocus::Local {
                                            BrowserFocus::Remote
                                        } else {
                                            BrowserFocus::Local
                                        };
                                    }
                                    KeyCode::Up => browser.nav_up(),
                                    KeyCode::Down => browser.nav_down(),
                                    KeyCode::Char(' ') | KeyCode::Enter => browser.enter(),
                                    KeyCode::Backspace => browser.go_up(),
                                    KeyCode::F(5) => browser.download(),
                                    KeyCode::F(6) => browser.upload(),
                                    KeyCode::Delete => browser.delete_focused(),
                                    KeyCode::Char('c') if ctrl => {
                                        disable_raw_mode()?;
                                        execute!(
                                            terminal.backend_mut(),
                                            LeaveAlternateScreen,
                                            crossterm::event::DisableMouseCapture
                                        )?;
                                        terminal.show_cursor()?;
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        continue;
                    }

                    // ---- Session: Ctrl+Arrow word-jump ----
                    if ctrl && !alt {
                        match key.code {
                            KeyCode::Left => {
                                app.send_str("\x1b[1;5D");
                                continue;
                            }
                            KeyCode::Right => {
                                app.send_str("\x1b[1;5C");
                                continue;
                            }
                            KeyCode::Up => {
                                app.send_str("\x1b[1;5A");
                                continue;
                            }
                            KeyCode::Down => {
                                app.send_str("\x1b[1;5B");
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // ---- Session: regular keys ----
                    match key.code {
                        KeyCode::Char(c) if ctrl && !alt => {
                            let code = (c as u8).to_ascii_uppercase().wrapping_sub(b'@');
                            app.send_str(&String::from_utf8_lossy(&[code]));
                        }
                        KeyCode::Char(c) => app.send_char(c),
                        KeyCode::Enter => app.send_str("\r"),
                        KeyCode::Backspace => app.send_str("\x7f"),
                        KeyCode::Delete => app.send_str("\x1b[3~"),
                        KeyCode::Tab => app.send_str("\t"),
                        KeyCode::BackTab => app.send_str("\x1b[Z"),
                        KeyCode::Left => app.send_str("\x1b[D"),
                        KeyCode::Right => app.send_str("\x1b[C"),
                        KeyCode::Up => app.send_str("\x1b[A"),
                        KeyCode::Down => app.send_str("\x1b[B"),
                        KeyCode::Home => app.send_str("\x1b[H"),
                        KeyCode::End => app.send_str("\x1b[F"),
                        KeyCode::PageUp => app.send_str("\x1b[5~"),
                        KeyCode::PageDown => app.send_str("\x1b[6~"),
                        KeyCode::F(n) => {
                            let seq = match n {
                                1 => "\x1bOP",
                                2 => "\x1bOQ",
                                3 => "\x1bOR",
                                4 => "\x1bOS",
                                5 => "\x1b[15~",
                                6 => "\x1b[17~",
                                7 => "\x1b[18~",
                                8 => "\x1b[19~",
                                9 => "\x1b[20~",
                                10 => "\x1b[21~",
                                11 => "\x1b[23~",
                                12 => "\x1b[24~",
                                _ => "",
                            };
                            if !seq.is_empty() {
                                app.send_str(seq);
                            }
                        }
                        _ => {}
                    }
                }

                Event::Mouse(mouse) => {
                    let content = content_area(last_area);
                    let areas = app.tabs[app.selected_tab].root.leaf_areas(content);

                    let clicked_pane = areas
                        .iter()
                        .enumerate()
                        .find(|(_, area)| {
                            mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                        })
                        .map(|(i, area)| (i, *area));

                    if let Some((pane_idx, pane_area)) = clicked_pane {
                        let prev_focus = app.tabs[app.selected_tab].focus_idx;

                        if matches!(mouse.kind, MouseEventKind::Down(_)) {
                            app.tabs[app.selected_tab].focus_idx = pane_idx;
                            app.drag_origin = Some(pane_idx);
                        }

                        // ---- FileBrowser mouse ----
                        let is_browser = matches!(
                            app.tabs[app.selected_tab].root.leaf(pane_idx),
                            Some(Pane::FileBrowser { .. })
                        );
                        if is_browser {
                            if let MouseEventKind::Up(_) = mouse.kind {
                                let origin = app.drag_origin.take();
                                let _origin_is_other_browser = origin
                                    .map(|o| {
                                        o != pane_idx
                                            && matches!(
                                                app.tabs[app.selected_tab].root.leaf(o),
                                                Some(Pane::FileBrowser { .. })
                                            )
                                    })
                                    .unwrap_or(false);
                                if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner = pane_inner(pane_area);
                                    let half = inner.width / 2;
                                    let in_remote = mouse.column >= inner.x + half;
                                    browser.focus = if in_remote {
                                        BrowserFocus::Remote
                                    } else {
                                        BrowserFocus::Local
                                    };
                                }
                            }
                            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                                if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner = pane_inner(pane_area);
                                    let half = inner.width / 2;
                                    browser.focus = if mouse.column >= inner.x + half {
                                        BrowserFocus::Remote
                                    } else {
                                        BrowserFocus::Local
                                    };
                                }
                            }
                            if let MouseEventKind::Up(MouseButton::Left) = mouse.kind {
                                if let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner = pane_inner(pane_area);
                                    let half = inner.width / 2;
                                    let in_remote = mouse.column >= inner.x + half;
                                    let prev_focus_panel = browser.focus;
                                    if in_remote && prev_focus_panel == BrowserFocus::Local {
                                        browser.drag_local_to_remote();
                                    } else if !in_remote && prev_focus_panel == BrowserFocus::Remote
                                    {
                                        browser.drag_remote_to_local();
                                    }
                                }
                            }
                            continue;
                        }

                        app.drag_origin = None;

                        // ---- Session mouse forwarding ----
                        let same_pane = pane_idx == prev_focus;
                        let pane_wants_mouse = app.tabs[app.selected_tab]
                            .root
                            .leaf_mut(pane_idx)
                            .map(|p| {
                                if let Pane::Session { terminal } = p {
                                    terminal.mouse_active.load(Ordering::Acquire)
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(false);

                        if same_pane && pane_wants_mouse {
                            let leaf_count = app.tabs[app.selected_tab].root.leaf_count();
                            let inner = if leaf_count > 1 {
                                pane_inner(pane_area)
                            } else {
                                pane_area
                            };
                            let col = (mouse.column as i32 - inner.x as i32).max(0) as u16;
                            let row = (mouse.row as i32 - inner.y as i32).max(0) as u16;
                            let seq = match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    format!("\x1b[<0;{};{}M", col + 1, row + 1)
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    format!("\x1b[<0;{};{}m", col + 1, row + 1)
                                }
                                MouseEventKind::Down(MouseButton::Right) => {
                                    format!("\x1b[<2;{};{}M", col + 1, row + 1)
                                }
                                MouseEventKind::Up(MouseButton::Right) => {
                                    format!("\x1b[<2;{};{}m", col + 1, row + 1)
                                }
                                MouseEventKind::Down(MouseButton::Middle) => {
                                    format!("\x1b[<1;{};{}M", col + 1, row + 1)
                                }
                                MouseEventKind::Up(MouseButton::Middle) => {
                                    format!("\x1b[<1;{};{}m", col + 1, row + 1)
                                }
                                MouseEventKind::ScrollUp => {
                                    format!("\x1b[<64;{};{}M", col + 1, row + 1)
                                }
                                MouseEventKind::ScrollDown => {
                                    format!("\x1b[<65;{};{}M", col + 1, row + 1)
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    format!("\x1b[<32;{};{}M", col + 1, row + 1)
                                }
                                _ => String::new(),
                            };
                            if !seq.is_empty() {
                                app.send_str(&seq);
                            }
                        }
                    }
                }

                Event::Resize(w, h) => {
                    last_area = Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    };
                    app.resize_all(last_area);
                    log!(log_file, "resize {}x{}", w, h);
                }
                _ => {}
            }
        }

        if needs_draw || had_event {
            terminal.draw(|f| {
                last_area = f.area();
                if needs_draw {
                    app.resize_all(last_area);
                }
                app.render(last_area, f.buffer_mut());
                let content = content_area(last_area);
                if let Some((cx, cy)) = app.tabs[app.selected_tab].focused_cursor(content) {
                    f.set_cursor_position((cx, cy));
                }
            })?;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pane::{Pane, Split, remove_leaf, split_areas};
    use sftp_parse::{
        epoch_to_ymd, human_size, parse_ls, parse_pwd, shell_quote, skip_n_tokens, strip_ansi,
    };
    use tab::Tab;

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }
    fn connect() -> Pane {
        Pane::new_connect()
    }
    fn hsplit() -> Pane {
        Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), connect()],
        }
    }
    fn vsplit() -> Pane {
        Pane::Split {
            kind: Split::Vertical,
            children: vec![connect(), connect()],
        }
    }
    fn ls(raw: &str) -> Vec<String> {
        raw.lines().map(|l| l.to_string()).collect()
    }

    // split_areas
    #[test]
    fn split_areas_horizontal_even() {
        let a = split_areas(r(100, 20), &Split::Horizontal, 2);
        assert_eq!(
            a[0],
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 20
            }
        );
        assert_eq!(
            a[1],
            Rect {
                x: 50,
                y: 0,
                width: 50,
                height: 20
            }
        );
    }
    #[test]
    fn split_areas_horizontal_remainder_to_last() {
        let a = split_areas(r(101, 20), &Split::Horizontal, 2);
        assert_eq!(a[0].width + a[1].width, 101);
        assert_eq!(a[1].width, 51);
    }
    #[test]
    fn split_areas_vertical_even() {
        let a = split_areas(r(80, 40), &Split::Vertical, 2);
        assert_eq!(
            a[0],
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 20
            }
        );
        assert_eq!(
            a[1],
            Rect {
                x: 0,
                y: 20,
                width: 80,
                height: 20
            }
        );
    }
    #[test]
    fn split_areas_vertical_three() {
        let a = split_areas(r(80, 30), &Split::Vertical, 3);
        assert_eq!(a.len(), 3);
        assert_eq!(a.iter().map(|x| x.height).sum::<u16>(), 30);
    }
    #[test]
    fn split_areas_empty() {
        assert!(split_areas(r(80, 40), &Split::Horizontal, 0).is_empty());
    }

    // leaf_count
    #[test]
    fn leaf_count_single() {
        assert_eq!(connect().leaf_count(), 1);
    }
    #[test]
    fn leaf_count_split() {
        assert_eq!(hsplit().leaf_count(), 2);
    }
    #[test]
    fn leaf_count_nested() {
        let p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
        };
        assert_eq!(p.leaf_count(), 3);
    }

    // leaf / leaf_areas
    #[test]
    fn leaf_single_bounds() {
        let p = connect();
        assert!(p.leaf(0).is_some());
        assert!(p.leaf(1).is_none());
    }
    #[test]
    fn leaf_split_dfs_order() {
        let p = hsplit();
        assert!(matches!(p.leaf(0), Some(Pane::Connect { .. })));
        assert!(p.leaf(2).is_none());
    }
    #[test]
    fn leaf_areas_covers_full() {
        assert_eq!(connect().leaf_areas(r(100, 50)), vec![r(100, 50)]);
    }
    #[test]
    fn leaf_areas_sum_equals_parent() {
        let a = hsplit().leaf_areas(r(100, 50));
        assert_eq!(a[0].width + a[1].width, 100);
    }
    #[test]
    fn leaf_areas_count_matches_leaf_count() {
        let p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
        };
        assert_eq!(p.leaf_areas(r(120, 60)).len(), p.leaf_count());
    }

    // remove_leaf
    #[test]
    fn remove_leaf_first() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        assert_eq!(p.leaf_count(), 1);
    }
    #[test]
    fn remove_leaf_second() {
        let mut p = hsplit();
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 1);
    }
    #[test]
    fn remove_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::Horizontal,
            children: vec![connect(), vsplit()],
        };
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 2);
    }
    #[test]
    fn remove_leaf_noop_on_single() {
        let mut p = connect();
        remove_leaf(&mut p, 0);
        assert_eq!(p.leaf_count(), 1);
    }

    // Tab
    #[test]
    fn tab_initial_state() {
        let t = Tab::new("1");
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.focus_idx, 0);
        assert!(matches!(t.root, Pane::Connect { .. }));
        assert_eq!(t.display_name(), "<connect>");
    }
    #[test]
    fn tab_split_h() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        assert_eq!(t.leaf_count(), 2);
    }
    #[test]
    fn tab_split_v() {
        let mut t = Tab::new("1");
        t.split(Split::Vertical, r(200, 50));
        assert_eq!(t.leaf_count(), 2);
    }
    #[test]
    fn tab_double_split_three_panes() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        t.focus_idx = 1;
        t.split(Split::Vertical, r(200, 50));
        assert_eq!(t.leaf_count(), 3);
    }
    #[test]
    fn tab_focus_next_wraps() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        t.focus_next();
        assert_eq!(t.focus_idx, 1);
        t.focus_next();
        assert_eq!(t.focus_idx, 0);
    }
    #[test]
    fn tab_focus_prev_wraps() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        t.focus_prev();
        assert_eq!(t.focus_idx, 1);
        t.focus_prev();
        assert_eq!(t.focus_idx, 0);
    }
    #[test]
    fn tab_close_reduces_count() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        t.close_focused();
        assert_eq!(t.leaf_count(), 1);
    }
    #[test]
    fn tab_close_clamps_focus() {
        let mut t = Tab::new("1");
        t.split(Split::Horizontal, r(200, 50));
        t.focus_idx = 1;
        t.close_focused();
        assert_eq!(t.focus_idx, 0);
    }
    #[test]
    fn tab_display_name_multi_pane() {
        let mut t = Tab::new("myhost");
        t.split(Split::Horizontal, r(200, 50));
        assert_eq!(t.display_name(), "myhost");
    }

    // parse_pwd
    #[test]
    fn parse_pwd_label() {
        assert_eq!(
            parse_pwd(&["Remote working directory: /home/debian".to_string()]),
            Some("/home/debian".to_string())
        );
    }
    #[test]
    fn parse_pwd_root() {
        assert_eq!(
            parse_pwd(&["Remote working directory: /".to_string()]),
            Some("/".to_string())
        );
    }
    #[test]
    fn parse_pwd_bare() {
        assert_eq!(
            parse_pwd(&["/home/user".to_string()]),
            Some("/home/user".to_string())
        );
    }
    #[test]
    fn parse_pwd_spaces() {
        assert_eq!(parse_pwd(&["/home/my dir".to_string()]), None);
    }
    #[test]
    fn parse_pwd_empty() {
        assert_eq!(parse_pwd(&[]), None);
    }

    // parse_ls
    #[test]
    fn parse_ls_file_and_dir() {
        let e = parse_ls(&ls(
            "drwx------    ? debian  debian  4096 Mar 14 09:44 docs\n-rw-r--r--    ? debian  debian   220 Aug  4  2021 .bashrc\nsftp>",
        ));
        assert_eq!(e.len(), 3);
        assert!(e.iter().any(|x| x.name == "docs"));
        assert!(e.iter().any(|x| x.name == ".bashrc"));
    }
    #[test]
    fn parse_ls_dirs_first() {
        let e = parse_ls(&ls(
            "-rw-r--r--    ? u g   100 Jan  1  2020 aaa.txt\ndrwxr-xr-x    ? u g  4096 Jan  1  2020 zzz_dir",
        ));
        assert_eq!(e[0].name, "..");
        assert_eq!(e[1].name, "zzz_dir");
        assert_eq!(e[2].name, "aaa.txt");
    }
    #[test]
    fn parse_ls_skips_dot_dotdot() {
        let e = parse_ls(&ls(
            "drwx------    ? u g 4096 Mar 14 09:44 .\ndrwx------    ? u g 4096 Jan  1  2020 ..",
        ));
        assert_eq!(e.len(), 1);
    }
    #[test]
    fn parse_ls_symlink() {
        let e = parse_ls(&ls(
            "lrwxrwxrwx    ? u g 11 Jan  1  2020 mylink -> /etc/target",
        ));
        assert!(e.iter().find(|x| x.name == "mylink").is_some());
    }
    #[test]
    fn parse_ls_skips_noise() {
        let e = parse_ls(&ls(
            "sftp>\ntotal 42\n-rw-r--r--    ? u g 100 Jan  1  2020 file.txt",
        ));
        assert_eq!(e.len(), 2);
    }
    #[test]
    fn parse_ls_masked_perms() {
        let e = parse_ls(&ls("drwx******    ? u g 4096 Mar 14 09:44 somedir"));
        assert!(e.iter().find(|x| x.name == "somedir").unwrap().is_dir);
    }
    #[test]
    fn parse_ls_perms_modified() {
        let e = parse_ls(&ls("-rw-r--r--    ? u g 100 Jan  1 12:00 notes.txt"));
        let f = e.iter().find(|x| x.name == "notes.txt").unwrap();
        assert_eq!(f.perms, "-rw-r--r--");
    }

    // strip_ansi
    #[test]
    fn strip_ansi_plain() {
        assert_eq!(strip_ansi(b"hello"), "hello");
    }
    #[test]
    fn strip_ansi_csi() {
        assert_eq!(strip_ansi(b"\x1b[32mhi\x1b[0m"), "hi");
    }
    #[test]
    fn strip_ansi_osc() {
        assert_eq!(strip_ansi(b"\x1b]0;title\x07x"), "x");
    }
    #[test]
    fn strip_ansi_bare_esc() {
        assert_eq!(strip_ansi(b"\x1bMtext"), "text");
    }
    #[test]
    fn strip_ansi_empty() {
        assert_eq!(strip_ansi(b""), "");
    }

    // human_size
    #[test]
    fn hs_bytes() {
        assert_eq!(human_size(500), "500 B");
    }
    #[test]
    fn hs_kb() {
        assert_eq!(human_size(1024), "1.0 KB");
    }
    #[test]
    fn hs_mb() {
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
    #[test]
    fn hs_gb() {
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
    }
    #[test]
    fn hs_zero() {
        assert_eq!(human_size(0), "0 B");
    }

    // shell_quote
    #[test]
    fn sq_plain() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }
    #[test]
    fn sq_spaces() {
        assert_eq!(shell_quote("my file"), "'my file'");
    }
    #[test]
    fn sq_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
    #[test]
    fn sq_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    // skip_n_tokens
    #[test]
    fn snt_zero() {
        assert_eq!(skip_n_tokens("a b c", 0), "a b c");
    }
    #[test]
    fn snt_one() {
        assert_eq!(skip_n_tokens("a b c", 1), "b c");
    }
    #[test]
    fn snt_all() {
        assert_eq!(skip_n_tokens("a b c", 3), "");
    }
    #[test]
    fn snt_spaces_in_name() {
        assert_eq!(
            skip_n_tokens("-rw-r--r-- 1 u g 100 Jan 1 12:00 my great file.txt", 8),
            "my great file.txt"
        );
    }

    // epoch_to_ymd
    #[test]
    fn epoch_unix_origin() {
        assert_eq!(epoch_to_ymd(0), (1970, 1, 1, 0, 0));
    }
    #[test]
    fn epoch_known_date() {
        let (y, mo, d, _, _) = epoch_to_ymd(1710374400);
        assert_eq!((y, mo, d), (2024, 3, 14));
    }
}
