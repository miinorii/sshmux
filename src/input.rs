use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use log::{debug, error, trace};
use ratatui::{layout::Rect, style::Color, widgets::ListState};

use crate::app::App;
use crate::browser::{
    BrowserKeyAction, DragAction, SftpState, SshBrowserState, handle_browser_key,
};
use crate::pane::{Pane, Split, pane_inner};

// ---------------------------------------------------------------------------
// Action — returned by input handlers to signal the main loop
// ---------------------------------------------------------------------------

pub enum Action {
    Continue,
    Quit,
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub fn handle_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
    last_area: Rect,
) -> Action {
    let focused_pane_has_app_cursor = app.focused_pane_app_cursor();

    // ---- Global shortcuts (Alt+…) ----
    if alt && !ctrl {
        match code {
            KeyCode::Char('q') => return Action::Quit,
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
                app.tab_mut().split(Split::Vertical, pane_inner(last_area));
            }
            KeyCode::Char('+') => {
                app.tab_mut()
                    .split(Split::Horizontal, pane_inner(last_area));
            }
            _ => {}
        }
        return Action::Continue;
    }

    // ---- Connect pane ----
    if let Some(action) = handle_connect_key(app, code, ctrl, last_area) {
        return action;
    }

    // ---- FileBrowser pane ----
    if let Some(action) = handle_sftp_browser_key(app, code, ctrl, alt, shift) {
        return action;
    }

    // ---- SshBrowser pane ----
    if let Some(action) = handle_ssh_browser_key(app, code, ctrl, alt, shift) {
        return action;
    }

    // ---- Session exit menu ----
    if handle_session_exit_key(app, code, last_area) {
        return Action::Continue;
    }

    // ---- Session: reset scrollback on any keypress ----
    if let Some(Pane::Session { terminal, .. }) = app.tab_mut().focused_pane_mut() {
        terminal.reset_scroll();
    }

    // ---- Session: Ctrl+Arrow word-jump ----
    if ctrl && !alt {
        match code {
            KeyCode::Left => {
                app.send_str("\x1b[1;5D");
                return Action::Continue;
            }
            KeyCode::Right => {
                app.send_str("\x1b[1;5C");
                return Action::Continue;
            }
            KeyCode::Up => {
                app.send_str("\x1b[1;5A");
                return Action::Continue;
            }
            KeyCode::Down => {
                app.send_str("\x1b[1;5B");
                return Action::Continue;
            }
            _ => {}
        }
    }

    // ---- Session: regular keys ----
    match code {
        KeyCode::Char(c) if ctrl && !alt => {
            // Convert Ctrl+<letter> to the corresponding control byte (0x01..0x1A).
            // Some terminals report Ctrl+C as Char('\x03') with CONTROL modifier
            // instead of Char('c') with CONTROL — handle both forms.
            let byte = if c.is_ascii_control() {
                c as u8
            } else {
                (c as u8).to_ascii_uppercase().wrapping_sub(b'@')
            };
            trace!(
                "ctrl+char: c={:?} (0x{:02X}) -> byte=0x{:02X}",
                c, c as u32, byte
            );
            app.send_str(&String::from_utf8_lossy(&[byte]));
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

    Action::Continue
}

// ---------------------------------------------------------------------------
// Connect pane keys
// ---------------------------------------------------------------------------

/// Returns `Some(Action)` if the focused pane is a Connect pane.
fn handle_connect_key(app: &mut App, code: KeyCode, ctrl: bool, last_area: Rect) -> Option<Action> {
    let focus_idx = app.tabs[app.selected_tab].focus_idx;
    if !matches!(
        app.tabs[app.selected_tab].root.leaf(focus_idx),
        Some(Pane::Connect { .. })
    ) {
        return None;
    }

    // Browser menu overlay
    let menu_open = matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect {
            browser_menu: Some(_),
            ..
        })
    );
    if menu_open {
        match code {
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
                if let Some(Pane::Connect { browser_menu, .. }) = app.tab_mut().focused_pane_mut() {
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
                if let Some(Pane::Connect { browser_menu, .. }) = app.tab_mut().focused_pane_mut() {
                    *browser_menu = None;
                }
            }
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Connect input overlay
    let input_open = matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect {
            connect_input: Some(_),
            ..
        })
    );
    if input_open {
        match code {
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
                } else if let Some(Pane::Connect { connect_input, .. }) =
                    app.tab_mut().focused_pane_mut()
                {
                    *connect_input = None;
                }
            }
            KeyCode::Esc => {
                if let Some(Pane::Connect { connect_input, .. }) = app.tab_mut().focused_pane_mut()
                {
                    *connect_input = None;
                }
            }
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Normal connect pane
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(Pane::Connect { list_state, .. }) = app.tab_mut().focused_pane_mut() {
                list_state.select_previous();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(Pane::Connect { list_state, .. }) = app.tab_mut().focused_pane_mut() {
                list_state.select_next();
            }
        }
        KeyCode::Enter => {
            let selected =
                if let Some(Pane::Connect { list_state, .. }) = app.tab_mut().focused_pane_mut() {
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
            if let Some(Pane::Connect { show_help, .. }) = app.tab_mut().focused_pane_mut() {
                *show_help = false;
            }
        }
        _ => {}
    }
    Some(Action::Continue)
}

// ---------------------------------------------------------------------------
// SFTP browser keys
// ---------------------------------------------------------------------------

/// Returns `Some(Action)` if the focused pane is a FileBrowser.
fn handle_sftp_browser_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Action> {
    let focus_idx = app.tabs[app.selected_tab].focus_idx;
    if !matches!(
        app.tabs[app.selected_tab].root.leaf(focus_idx),
        Some(Pane::FileBrowser { .. })
    ) {
        return None;
    }

    if let Some(Pane::FileBrowser { browser }) = app.tab_mut().focused_pane_mut() {
        // While connecting, forward keystrokes to the SFTP PTY
        if browser.sftp_state == SftpState::Connecting {
            match code {
                KeyCode::Char(c) => browser.sftp.send_char(c),
                KeyCode::Enter => browser.sftp.send_str("\r\n"),
                KeyCode::Backspace => browser.sftp.send_str("\x7f"),
                _ => {}
            }
            return Some(Action::Continue);
        }

        // Ignore ctrl/alt chars in idle browser mode — they are not browser
        // actions and must not trigger paste accumulation.
        if (ctrl || alt) && matches!(code, KeyCode::Char(_)) {
            return Some(Action::Continue);
        }

        match handle_browser_key(&mut browser.core, code, shift) {
            BrowserKeyAction::Enter => browser.enter(),
            BrowserKeyAction::GoUp => browser.go_up(),
            BrowserKeyAction::Download => browser.download(),
            BrowserKeyAction::Upload => browser.upload(),
            BrowserKeyAction::UploadPaths => browser.upload_pending_paths(),
            BrowserKeyAction::Delete => browser.delete_focused(),
            BrowserKeyAction::ConfirmDeleteYes => browser.confirm_delete_yes(),

            BrowserKeyAction::Handled => {}
        }
    }

    Some(Action::Continue)
}

// ---------------------------------------------------------------------------
// SSH/SCP browser keys
// ---------------------------------------------------------------------------

/// Returns `Some(Action)` if the focused pane is an SshBrowser.
fn handle_ssh_browser_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Action> {
    let focus_idx = app.tabs[app.selected_tab].focus_idx;
    if !matches!(
        app.tabs[app.selected_tab].root.leaf(focus_idx),
        Some(Pane::SshBrowser { .. })
    ) {
        return None;
    }

    if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut() {
        // Password prompt
        if browser.waiting_password {
            match code {
                KeyCode::Char(c) => browser.password_char(c),
                KeyCode::Backspace => browser.password_backspace(),
                KeyCode::Enter => browser.submit_password(),
                KeyCode::Esc => {
                    browser.waiting_password = false;
                    browser.password_buf.clear();
                    browser.core.needs_redraw = true;
                    if browser.ssh_state == SshBrowserState::Transferring {
                        browser.scp_pty = None;
                        browser.ssh_state = SshBrowserState::Idle;
                        browser.core.status_msg = "Transfer cancelled".to_string();
                    } else {
                        browser.core.status_msg = "Password cancelled".to_string();
                    }
                    browser.core.status_color = Color::Yellow;
                }
                _ => {}
            }
            return Some(Action::Continue);
        }

        // During connecting/setting prompt, forward keystrokes to SSH PTY
        if matches!(
            browser.ssh_state,
            SshBrowserState::Connecting | SshBrowserState::SettingPrompt
        ) {
            match code {
                KeyCode::Char(c) => browser.ssh.send_char(c),
                KeyCode::Enter => browser.ssh.send_str("\r\n"),
                KeyCode::Backspace => browser.ssh.send_str("\x7f"),
                _ => {}
            }
            return Some(Action::Continue);
        }

        if (ctrl || alt) && matches!(code, KeyCode::Char(_)) {
            return Some(Action::Continue);
        }

        match handle_browser_key(&mut browser.core, code, shift) {
            BrowserKeyAction::Enter => browser.enter(),
            BrowserKeyAction::GoUp => browser.go_up(),
            BrowserKeyAction::Download => browser.download(),
            BrowserKeyAction::Upload => browser.upload(),
            BrowserKeyAction::UploadPaths => browser.upload_pending_paths(),
            BrowserKeyAction::Delete => browser.delete_focused(),
            BrowserKeyAction::ConfirmDeleteYes => browser.confirm_delete_yes(),

            BrowserKeyAction::Handled => {}
        }
    }

    Some(Action::Continue)
}

// ---------------------------------------------------------------------------
// Session exit menu keys
// ---------------------------------------------------------------------------

/// Returns true if the focused pane is an exited session and the event was handled.
fn handle_session_exit_key(app: &mut App, code: KeyCode, last_area: Rect) -> bool {
    let session_exited = matches!(
        app.tab().focused_pane(),
        Some(Pane::Session { terminal, .. }) if terminal.process_exited()
    );
    if !session_exited {
        return false;
    }

    match code {
        KeyCode::Left | KeyCode::Right => {
            if let Some(Pane::Session { exit_selection, .. }) = app.tab_mut().focused_pane_mut() {
                *exit_selection ^= 1;
            }
        }
        KeyCode::Enter => {
            let action =
                if let Some(Pane::Session { exit_selection, .. }) = app.tab().focused_pane() {
                    Some(*exit_selection)
                } else {
                    None
                };
            match action {
                Some(0) => {
                    // Reconnect
                    if let Some(Pane::Session { ssh_args, .. }) = app.tab().focused_pane() {
                        let args = ssh_args.clone();
                        if let Err(e) = app.open_session_raw(&args, last_area) {
                            error!("reconnect: {}", e);
                        }
                        app.resize_all(last_area);
                    }
                }
                Some(1) => {
                    // Close pane
                    let was_last_pane = app.tab().leaf_count() == 1;
                    if was_last_pane {
                        app.close_tab();
                    } else {
                        app.tab_mut().close_focused();
                        app.resize_all(last_area);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    true
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

pub fn handle_mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16, last_area: Rect) {
    let content = pane_inner(last_area);
    let areas = app.tabs[app.selected_tab].root.leaf_areas(content);

    let clicked_pane = areas
        .iter()
        .enumerate()
        .find(|(_, area)| {
            column >= area.x
                && column < area.x + area.width
                && row >= area.y
                && row < area.y + area.height
        })
        .map(|(i, area)| (i, *area));

    let Some((pane_idx, pane_area)) = clicked_pane else {
        return;
    };

    let prev_focus = app.tabs[app.selected_tab].focus_idx;

    if matches!(kind, MouseEventKind::Down(_)) {
        app.tabs[app.selected_tab].focus_idx = pane_idx;
    }

    // ---- Browser mouse (shared for both SFTP and SCP) ----
    let is_browser = matches!(
        app.tabs[app.selected_tab].root.leaf(pane_idx),
        Some(Pane::FileBrowser { .. })
    );
    let is_ssh_browser = matches!(
        app.tabs[app.selected_tab].root.leaf(pane_idx),
        Some(Pane::SshBrowser { .. })
    );

    if is_browser || is_ssh_browser {
        handle_browser_mouse(app, kind, column, row, pane_area, is_browser);
        return;
    }

    // ---- Session mouse forwarding ----
    let same_pane = pane_idx == prev_focus;
    let pane_wants_mouse = app.tabs[app.selected_tab]
        .root
        .leaf_mut(pane_idx)
        .map(|p| {
            if let Pane::Session { terminal, .. } = p {
                terminal.mouse_active() && !terminal.process_exited()
            } else {
                false
            }
        })
        .unwrap_or(false);

    // Scrollback / alternate-screen arrow translation
    if !pane_wants_mouse {
        let in_alt_screen = app.tabs[app.selected_tab]
            .root
            .leaf(pane_idx)
            .map(|p| {
                if let Pane::Session { terminal, .. } = p {
                    terminal.alternate_screen()
                } else {
                    false
                }
            })
            .unwrap_or(false);
        let is_scroll = matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown);
        if is_scroll {
            if in_alt_screen {
                let use_app = app.focused_pane_app_cursor();
                let seq = match (kind, use_app) {
                    (MouseEventKind::ScrollUp, true) => "\x1bOA",
                    (MouseEventKind::ScrollUp, false) => "\x1b[A",
                    (MouseEventKind::ScrollDown, true) => "\x1bOB",
                    (MouseEventKind::ScrollDown, false) => "\x1b[B",
                    _ => "",
                };
                if !seq.is_empty() {
                    app.send_str(seq);
                }
            } else if let Some(Pane::Session { terminal, .. }) =
                app.tabs[app.selected_tab].root.leaf_mut(pane_idx)
            {
                match kind {
                    MouseEventKind::ScrollUp => terminal.scroll_up(3),
                    MouseEventKind::ScrollDown => terminal.scroll_down(3),
                    _ => {}
                }
            }
            return;
        }
    }

    if same_pane && pane_wants_mouse {
        let leaf_count = app.tabs[app.selected_tab].root.leaf_count();
        let inner = if leaf_count > 1 {
            pane_inner(pane_area)
        } else {
            pane_area
        };
        let col = (column as i32 - inner.x as i32).max(0) as u16;
        let r = (row as i32 - inner.y as i32).max(0) as u16;

        // Check if remote app wants motion events (AnyMotion / mode 1003)
        let wants_motion = app.tabs[app.selected_tab]
            .root
            .leaf(pane_idx)
            .map(|p| {
                if let Pane::Session { terminal, .. } = p {
                    terminal.mouse_wants_motion()
                } else {
                    false
                }
            })
            .unwrap_or(false);

        // SGR extended mouse encoding: \x1b[<Cb;Cx;CyM (press) / m (release)
        // Button codes: 0=left, 1=middle, 2=right, 32+=motion flag, 64/65=scroll
        let seq = match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                format!("\x1b[<0;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                format!("\x1b[<0;{};{}m", col + 1, r + 1)
            }
            MouseEventKind::Down(MouseButton::Right) => {
                format!("\x1b[<2;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Up(MouseButton::Right) => {
                format!("\x1b[<2;{};{}m", col + 1, r + 1)
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                format!("\x1b[<1;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Up(MouseButton::Middle) => {
                format!("\x1b[<1;{};{}m", col + 1, r + 1)
            }
            MouseEventKind::ScrollUp => {
                format!("\x1b[<64;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::ScrollDown => {
                format!("\x1b[<65;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                format!("\x1b[<32;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Drag(MouseButton::Right) => {
                format!("\x1b[<34;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Drag(MouseButton::Middle) => {
                format!("\x1b[<33;{};{}M", col + 1, r + 1)
            }
            MouseEventKind::Moved if wants_motion => {
                format!("\x1b[<35;{};{}M", col + 1, r + 1)
            }
            _ => String::new(),
        };
        if !seq.is_empty() {
            app.send_str(&seq);
        }
    }
}

// ---------------------------------------------------------------------------
// Browser mouse (shared between SFTP and SCP browsers)
// ---------------------------------------------------------------------------

fn handle_browser_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    pane_area: Rect,
    is_sftp: bool,
) {
    let leaf_count = app.tabs[app.selected_tab].root.leaf_count();

    // Block all mouse events when the upload confirmation overlay is active
    let overlay_active = if is_sftp {
        matches!(app.tab().focused_pane(), Some(Pane::FileBrowser { browser }) if !browser.core.pending_uploads.is_empty())
    } else {
        matches!(app.tab().focused_pane(), Some(Pane::SshBrowser { browser }) if !browser.core.pending_uploads.is_empty())
    };
    if overlay_active {
        return;
    }

    if let MouseEventKind::Down(MouseButton::Left) = kind {
        if is_sftp {
            if let Some(Pane::FileBrowser { browser }) = app.tab_mut().focused_pane_mut() {
                browser
                    .core
                    .handle_click(column, row, pane_area, leaf_count);
            }
        } else if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut() {
            browser
                .core
                .handle_click(column, row, pane_area, leaf_count);
        }
    }

    if let MouseEventKind::Up(MouseButton::Left) = kind {
        if is_sftp {
            if let Some(Pane::FileBrowser { browser }) = app.tab_mut().focused_pane_mut() {
                let indices = browser.core.selected_indices();
                if indices.len() > 1 {
                    browser.core.queue_transfers_from_indices(&indices);
                    browser.core.clear_selection();
                }
                match browser
                    .core
                    .handle_drag_release(column, pane_area, leaf_count)
                {
                    Some(DragAction::LocalToRemote) => browser.upload(),
                    Some(DragAction::RemoteToLocal) => browser.download(),
                    None => {}
                }
            }
        } else if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut() {
            let indices = browser.core.selected_indices();
            if indices.len() > 1 {
                browser.core.queue_transfers_from_indices(&indices);
                browser.core.clear_selection();
            }
            match browser
                .core
                .handle_drag_release(column, pane_area, leaf_count)
            {
                Some(DragAction::LocalToRemote) => browser.upload(),
                Some(DragAction::RemoteToLocal) => browser.download(),
                None => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Paste handling (bracketed paste for SSH sessions)
// ---------------------------------------------------------------------------

pub fn handle_paste(app: &mut App, text: &str) {
    debug!("handle_paste: text={:?}", text);
    if matches!(app.tab().focused_pane(), Some(Pane::Session { .. })) {
        debug!("handle_paste: forwarding to session as bracketed paste");
        let bracketed = format!("\x1b[200~{}\x1b[201~", text);
        app.send_str(&bracketed);
    } else {
        debug!("handle_paste: ignored (not a session pane)");
    }
}
