use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use log::{debug, error};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect, widgets::ListState};

// ---------------------------------------------------------------------------
// Module declarations
// ---------------------------------------------------------------------------

mod app;
mod browser;
mod pane;
mod ssh_config;
mod tab;
mod terminal;

use app::{App, content_area};
use browser::{BrowserFocus, SftpState, SshBrowserState};
use pane::{Pane, Split, pane_inner};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let is_debug = std::env::args().any(|a| a == "--debug");
    if is_debug {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        // Convert epoch to YYYYMMDD_HHMMSS (UTC)
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        // Simple date from days since epoch
        let (y, mo, d) = epoch_days_to_ymd(days);
        let filename = format!("sshmux-debug-{y:04}{mo:02}{d:02}_{h:02}{m:02}{s:02}.log");
        let file = std::fs::File::create(&filename)?;
        simplelog::WriteLogger::init(
            simplelog::LevelFilter::Debug,
            simplelog::ConfigBuilder::new()
                .set_time_format_custom(time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]:[second]"
                ))
                .build(),
            file,
        )
        .ok();
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
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
    let mut first_frame = true;

    loop {
        std::thread::sleep(Duration::from_millis(5));

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
                    let focused_pane_has_app_cursor = app.focused_pane_app_cursor();

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
                        // Check if browser menu is open
                        let menu_open = matches!(
                            app.tab().focused_pane(),
                            Some(Pane::Connect {
                                browser_menu: Some(_),
                                ..
                            })
                        );

                        if menu_open {
                            match key.code {
                                KeyCode::Up => {
                                    if let Some(Pane::Connect {
                                        browser_menu: Some(ms),
                                        ..
                                    }) = app.tab_mut().focused_pane_mut()
                                    {
                                        ms.select_previous();
                                    }
                                }
                                KeyCode::Down => {
                                    if let Some(Pane::Connect {
                                        browser_menu: Some(ms),
                                        ..
                                    }) = app.tab_mut().focused_pane_mut()
                                    {
                                        ms.select_next();
                                    }
                                }
                                KeyCode::Enter => {
                                    let (host_idx, menu_idx) = if let Some(Pane::Connect {
                                        list_state,
                                        browser_menu: Some(ms),
                                        ..
                                    }) = app.tab().focused_pane()
                                    {
                                        (list_state.selected(), ms.selected())
                                    } else {
                                        (None, None)
                                    };
                                    // Close menu first
                                    if let Some(Pane::Connect { browser_menu, .. }) =
                                        app.tab_mut().focused_pane_mut()
                                    {
                                        *browser_menu = None;
                                    }
                                    if let (Some(idx), Some(mi)) = (host_idx, menu_idx) {
                                        match mi {
                                            0 => {
                                                if let Err(e) = app.open_browser(idx) {
                                                    error!("open_browser: {}", e);
                                                }
                                            }
                                            1 => {
                                                if let Err(e) = app.open_ssh_browser(idx) {
                                                    error!("open_ssh_browser: {}", e);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    if let Some(Pane::Connect { browser_menu, .. }) =
                                        app.tab_mut().focused_pane_mut()
                                    {
                                        *browser_menu = None;
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // Check if connect input is open
                        let input_open = matches!(
                            app.tab().focused_pane(),
                            Some(Pane::Connect {
                                connect_input: Some(_),
                                ..
                            })
                        );

                        if input_open {
                            match key.code {
                                KeyCode::Char(c) if !ctrl => {
                                    if let Some(Pane::Connect {
                                        connect_input: Some(input),
                                        ..
                                    }) = app.tab_mut().focused_pane_mut()
                                    {
                                        input.push(c);
                                    }
                                }
                                KeyCode::Backspace => {
                                    if let Some(Pane::Connect {
                                        connect_input: Some(input),
                                        ..
                                    }) = app.tab_mut().focused_pane_mut()
                                    {
                                        input.pop();
                                    }
                                }
                                KeyCode::Enter => {
                                    let args = if let Some(Pane::Connect {
                                        connect_input: Some(input),
                                        ..
                                    }) = app.tab().focused_pane()
                                    {
                                        let trimmed = input.trim().to_string();
                                        if trimmed.is_empty() {
                                            None
                                        } else {
                                            Some(trimmed)
                                        }
                                    } else {
                                        None
                                    };
                                    if let Some(args) = args {
                                        if let Err(e) = app.open_session_raw(&args, last_area) {
                                            error!("open_session_raw: {}", e);
                                        }
                                        app.resize_all(last_area);
                                    } else {
                                        if let Some(Pane::Connect { connect_input, .. }) =
                                            app.tab_mut().focused_pane_mut()
                                        {
                                            *connect_input = None;
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    if let Some(Pane::Connect { connect_input, .. }) =
                                        app.tab_mut().focused_pane_mut()
                                    {
                                        *connect_input = None;
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

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
                                if let Some(Pane::Connect { list_state, .. }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.select_previous();
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(Pane::Connect { list_state, .. }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.select_next();
                                }
                            }
                            KeyCode::Enter => {
                                let selected = if let Some(Pane::Connect { list_state, .. }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    list_state.selected()
                                } else {
                                    None
                                };
                                if let Some(idx) = selected {
                                    if let Err(e) = app.open_session(idx, last_area) {
                                        error!("open_session: {}", e);
                                    }
                                    app.resize_all(last_area);
                                }
                            }
                            KeyCode::Char('b') | KeyCode::Char('B') => {
                                if let Some(Pane::Connect {
                                    browser_menu,
                                    connect_input,
                                    show_help,
                                    ..
                                }) = app.tab_mut().focused_pane_mut()
                                {
                                    *show_help = false;
                                    *connect_input = None;
                                    let mut ms = ListState::default();
                                    ms.select(Some(0));
                                    *browser_menu = Some(ms);
                                }
                            }
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                if let Some(Pane::Connect {
                                    browser_menu,
                                    connect_input,
                                    show_help,
                                    ..
                                }) = app.tab_mut().focused_pane_mut()
                                {
                                    *show_help = false;
                                    *browser_menu = None;
                                    *connect_input = Some(String::new());
                                }
                            }
                            KeyCode::Char('h') | KeyCode::Char('H') => {
                                if let Some(Pane::Connect {
                                    browser_menu,
                                    connect_input,
                                    show_help,
                                    ..
                                }) = app.tab_mut().focused_pane_mut()
                                {
                                    *show_help = !*show_help;
                                    *browser_menu = None;
                                    *connect_input = None;
                                }
                            }
                            KeyCode::Esc => {
                                if let Some(Pane::Connect { show_help, .. }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    *show_help = false;
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
                            // While connecting (e.g. waiting for a password prompt from sftp),
                            // forward keystrokes directly to the underlying terminal so the
                            // user can interact with the ssh/sftp authentication dialogue.
                            if browser.sftp_state == SftpState::Connecting {
                                match key.code {
                                    KeyCode::Char(c) => browser.sftp.send_char(c),
                                    KeyCode::Enter => browser.sftp.send_str("\r\n"),
                                    KeyCode::Backspace => browser.sftp.send_str("\x7f"),
                                    _ => {}
                                }
                                continue;
                            }

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
                                        browser.dismiss_drive_picker();
                                        browser.focus = if browser.focus == BrowserFocus::Local {
                                            BrowserFocus::Remote
                                        } else {
                                            BrowserFocus::Local
                                        };
                                    }
                                    KeyCode::Esc => browser.dismiss_drive_picker(),
                                    KeyCode::Up => browser.nav_up(),
                                    KeyCode::Down => browser.nav_down(),
                                    KeyCode::Left => browser.scroll_left(),
                                    KeyCode::Right => browser.scroll_right(),
                                    KeyCode::Char(' ') | KeyCode::Enter => browser.enter(),
                                    KeyCode::Backspace => browser.go_up(),
                                    KeyCode::Char('t') => match browser.focus {
                                        BrowserFocus::Remote => browser.download(),
                                        BrowserFocus::Local => browser.upload(),
                                    },
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

                    // ---- SshBrowser pane ----
                    let focus_idx = app.tabs[app.selected_tab].focus_idx;
                    let focused_is_ssh_browser = matches!(
                        app.tabs[app.selected_tab].root.leaf(focus_idx),
                        Some(Pane::SshBrowser { .. })
                    );

                    if focused_is_ssh_browser {
                        if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut()
                        {
                            // Password prompt (both during connection and transfer)
                            if browser.waiting_password {
                                match key.code {
                                    KeyCode::Char(c) => browser.password_char(c),
                                    KeyCode::Backspace => browser.password_backspace(),
                                    KeyCode::Enter => browser.submit_password(),
                                    KeyCode::Esc => {
                                        browser.waiting_password = false;
                                        browser.password_buf.clear();
                                        browser.needs_redraw = true;
                                        if browser.ssh_state == SshBrowserState::Transferring {
                                            browser.scp_pty = None;
                                            browser.ssh_state = SshBrowserState::Idle;
                                            browser.status_msg = "Transfer cancelled".to_string();
                                        } else {
                                            browser.status_msg = "Password cancelled".to_string();
                                        }
                                        browser.status_color = ratatui::style::Color::Yellow;
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            // During connecting/setting prompt, forward keystrokes to SSH PTY
                            if matches!(
                                browser.ssh_state,
                                SshBrowserState::Connecting | SshBrowserState::SettingPrompt
                            ) {
                                match key.code {
                                    KeyCode::Char(c) => browser.ssh.send_char(c),
                                    KeyCode::Enter => browser.ssh.send_str("\r\n"),
                                    KeyCode::Backspace => browser.ssh.send_str("\x7f"),
                                    _ => {}
                                }
                                continue;
                            }

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
                                        browser.dismiss_drive_picker();
                                        browser.focus = if browser.focus == BrowserFocus::Local {
                                            BrowserFocus::Remote
                                        } else {
                                            BrowserFocus::Local
                                        };
                                    }
                                    KeyCode::Esc => browser.dismiss_drive_picker(),
                                    KeyCode::Up => browser.nav_up(),
                                    KeyCode::Down => browser.nav_down(),
                                    KeyCode::Left => browser.scroll_left(),
                                    KeyCode::Right => browser.scroll_right(),
                                    KeyCode::Char(' ') | KeyCode::Enter => browser.enter(),
                                    KeyCode::Backspace => browser.go_up(),
                                    KeyCode::Char('t') => match browser.focus {
                                        BrowserFocus::Remote => browser.download(),
                                        BrowserFocus::Local => browser.upload(),
                                    },
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

                    // Reset scrollback on any keypress to a session
                    if let Some(Pane::Session { terminal }) = app.tab_mut().focused_pane_mut() {
                        terminal.reset_scroll();
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
                        KeyCode::Left => {
                            if focused_pane_has_app_cursor {
                                app.send_str("\x1bOD");
                            } else {
                                app.send_str("\x1b[D");
                            }
                        }
                        KeyCode::Right => {
                            if focused_pane_has_app_cursor {
                                app.send_str("\x1bOC");
                            } else {
                                app.send_str("\x1b[C");
                            }
                        }
                        KeyCode::Up => {
                            if focused_pane_has_app_cursor {
                                app.send_str("\x1bOA");
                            } else {
                                app.send_str("\x1b[A");
                            }
                        }
                        KeyCode::Down => {
                            if focused_pane_has_app_cursor {
                                app.send_str("\x1bOB");
                            } else {
                                app.send_str("\x1b[B");
                            }
                        }
                        KeyCode::Home => app.send_str("\x1b[H"),
                        KeyCode::End => app.send_str("\x1b[F"),
                        KeyCode::Esc => app.send_str("\x1b"),
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
                        }

                        // ---- FileBrowser mouse ----
                        let is_browser = matches!(
                            app.tabs[app.selected_tab].root.leaf(pane_idx),
                            Some(Pane::FileBrowser { .. })
                        );
                        if is_browser {
                            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                                let leaf_count = app.tabs[app.selected_tab].root.leaf_count();
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
                                    browser.click_select(
                                        mouse.column,
                                        mouse.row,
                                        pane_area,
                                        leaf_count,
                                    );
                                }
                            }
                            if let MouseEventKind::Up(MouseButton::Left) = mouse.kind
                                && let Some(Pane::FileBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                            {
                                let inner = pane_inner(pane_area);
                                let half = inner.width / 2;
                                let in_remote = mouse.column >= inner.x + half;
                                let drag_from = browser.focus;
                                if in_remote && drag_from == BrowserFocus::Local {
                                    browser.drag_local_to_remote();
                                } else if !in_remote && drag_from == BrowserFocus::Remote {
                                    browser.drag_remote_to_local();
                                }
                                browser.focus = if in_remote {
                                    BrowserFocus::Remote
                                } else {
                                    BrowserFocus::Local
                                };
                            }
                            continue;
                        }

                        // ---- SshBrowser mouse ----
                        let is_ssh_browser = matches!(
                            app.tabs[app.selected_tab].root.leaf(pane_idx),
                            Some(Pane::SshBrowser { .. })
                        );
                        if is_ssh_browser {
                            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                                let leaf_count = app.tabs[app.selected_tab].root.leaf_count();
                                if let Some(Pane::SshBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                                {
                                    let inner = pane_inner(pane_area);
                                    let half = inner.width / 2;
                                    browser.focus = if mouse.column >= inner.x + half {
                                        BrowserFocus::Remote
                                    } else {
                                        BrowserFocus::Local
                                    };
                                    browser.click_select(
                                        mouse.column,
                                        mouse.row,
                                        pane_area,
                                        leaf_count,
                                    );
                                }
                            }
                            if let MouseEventKind::Up(MouseButton::Left) = mouse.kind
                                && let Some(Pane::SshBrowser { browser }) =
                                    app.tab_mut().focused_pane_mut()
                            {
                                let inner = pane_inner(pane_area);
                                let half = inner.width / 2;
                                let in_remote = mouse.column >= inner.x + half;
                                let drag_from = browser.focus;
                                if in_remote && drag_from == BrowserFocus::Local {
                                    browser.drag_local_to_remote();
                                } else if !in_remote && drag_from == BrowserFocus::Remote {
                                    browser.drag_remote_to_local();
                                }
                                browser.focus = if in_remote {
                                    BrowserFocus::Remote
                                } else {
                                    BrowserFocus::Local
                                };
                            }
                            continue;
                        }

                        // ---- Session mouse forwarding ----
                        let same_pane = pane_idx == prev_focus;
                        let pane_wants_mouse = app.tabs[app.selected_tab]
                            .root
                            .leaf_mut(pane_idx)
                            .map(|p| {
                                if let Pane::Session { terminal } = p {
                                    terminal.mouse_active() && !terminal.process_exited()
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(false);

                        // Scrollback when remote app doesn't capture mouse
                        // and is not in alternate screen (vim, htop, etc.)
                        if !pane_wants_mouse {
                            let in_alt_screen = app.tabs[app.selected_tab]
                                .root
                                .leaf(pane_idx)
                                .map(|p| {
                                    if let Pane::Session { terminal } = p {
                                        terminal.alternate_screen()
                                    } else {
                                        false
                                    }
                                })
                                .unwrap_or(false);
                            let is_scroll = matches!(
                                mouse.kind,
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                            );
                            if is_scroll {
                                if in_alt_screen {
                                    // In alternate screen (vim, less, htop…),
                                    // translate scroll into arrow keys
                                    let use_app = app.focused_pane_app_cursor();
                                    let seq = match (mouse.kind, use_app) {
                                        (MouseEventKind::ScrollUp, true) => "\x1bOA",
                                        (MouseEventKind::ScrollUp, false) => "\x1b[A",
                                        (MouseEventKind::ScrollDown, true) => "\x1bOB",
                                        (MouseEventKind::ScrollDown, false) => "\x1b[B",
                                        _ => "",
                                    };
                                    if !seq.is_empty() {
                                        app.send_str(seq);
                                    }
                                } else if let Some(Pane::Session { terminal }) =
                                    app.tabs[app.selected_tab].root.leaf_mut(pane_idx)
                                {
                                    match mouse.kind {
                                        MouseEventKind::ScrollUp => terminal.scroll_up(3),
                                        MouseEventKind::ScrollDown => terminal.scroll_down(3),
                                        _ => {}
                                    }
                                }
                                continue;
                            }
                        }

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
                    debug!("resize {}x{}", w, h);
                }
                _ => {}
            }
        }

        if needs_draw || had_event || first_frame {
            first_frame = false;
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

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970;
    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        mo += 1;
    }
    (y, mo, days + 1)
}

fn is_leap(y: u64) -> bool {
    y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400))
}
