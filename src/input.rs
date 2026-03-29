use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use log::{debug, error, trace};
use ratatui::{layout::Rect, style::Color, widgets::ListState};

use crate::app::App;
use crate::browser::{BrowserKeyAction, DragAction, SshBrowserState, handle_browser_key};
use crate::keybindings::KeyBinding;
use crate::pane::{
    ConnectOverlay, KeyEditorState, Pane, Split, editor_binding_index, editor_nav_down,
    editor_nav_up, pane_inner,
};

// ---------------------------------------------------------------------------
// Action — returned by input handlers to signal the main loop
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
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
    // ---- Dismiss context menu on any keypress ----
    if app.context_menu.is_some() {
        app.context_menu = None;
        return Action::Continue;
    }

    let focused_pane_has_app_cursor = app.focused_pane_app_cursor();

    // When the key editor is in capture mode, skip global shortcuts and
    // Alt suppression so the captured key reaches the editor handler.
    let editor_capturing = matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(KeyEditorState { editing: true, .. }),
            ..
        })
    );

    if !editor_capturing {
        // ---- Global shortcuts ----
        let g = &app.keybindings.global;
        if g.quit.matches(code, ctrl, alt, shift) {
            return Action::Quit;
        }
        if g.prev_tab.matches(code, ctrl, alt, shift) {
            if app.selected_tab > 0 {
                app.selected_tab -= 1;
            } else {
                app.selected_tab = app.tabs.len() - 1;
            }
            return Action::Continue;
        }
        if g.next_tab.matches(code, ctrl, alt, shift) {
            app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
            return Action::Continue;
        }
        if g.prev_pane.matches(code, ctrl, alt, shift) {
            app.tab_mut().focus_prev();
            return Action::Continue;
        }
        if g.next_pane.matches(code, ctrl, alt, shift) {
            app.tab_mut().focus_next();
            return Action::Continue;
        }
        if g.close.matches(code, ctrl, alt, shift) {
            let was_last_pane = app.tab().leaf_count() == 1;
            if was_last_pane {
                app.close_tab();
            } else {
                app.tab_mut().close_focused();
                app.resize_all(last_area);
            }
            return Action::Continue;
        }
        if g.new_tab.matches(code, ctrl, alt, shift) {
            app.new_tab();
            return Action::Continue;
        }
        if g.split_horizontal.matches(code, ctrl, alt, shift) {
            app.tab_mut().split(Split::TopBottom, pane_inner(last_area));
            return Action::Continue;
        }
        if g.split_vertical.matches(code, ctrl, alt, shift) {
            app.tab_mut().split(Split::LeftRight, pane_inner(last_area));
            return Action::Continue;
        }
    }

    // ---- Connect pane ----
    if let Some(action) = handle_connect_key(app, code, ctrl, alt, shift, last_area) {
        return action;
    }

    // ---- Browser pane (SFTP or SCP) ----
    if let Some(action) = handle_browser_key_dispatch(app, code, ctrl, alt, shift) {
        return action;
    }

    // Suppress unbound Alt+Char from reaching session passthrough.
    // Connect and browser panes are already handled above, so this only
    // affects sessions where we must prevent Alt+key from being forwarded
    // to the remote terminal.
    if !editor_capturing && alt && !ctrl {
        return Action::Continue;
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
fn handle_connect_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
    last_area: Rect,
) -> Option<Action> {
    let focus_idx = app.tabs[app.selected_tab].focus_idx;
    if !matches!(
        app.tabs[app.selected_tab].root.leaf(focus_idx),
        Some(Pane::Connect { .. })
    ) {
        return None;
    }

    // Browser menu overlay
    if matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect {
            overlay: ConnectOverlay::BrowserMenu(_),
            ..
        })
    ) {
        match code {
            KeyCode::Up => {
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::BrowserMenu(ms),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    ms.select_previous();
                }
            }
            KeyCode::Down => {
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::BrowserMenu(ms),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    ms.select_next();
                }
            }
            KeyCode::Enter => {
                let (host_idx, menu_idx) = if let Some(Pane::Connect {
                    list_state,
                    overlay: ConnectOverlay::BrowserMenu(ms),
                }) = app.tab().focused_pane()
                {
                    (list_state.selected(), ms.selected())
                } else {
                    (None, None)
                };
                if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
                    *overlay = ConnectOverlay::None;
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
                if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
                    *overlay = ConnectOverlay::None;
                }
            }
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Connect input overlay
    if matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect {
            overlay: ConnectOverlay::ConnectInput(_),
            ..
        })
    ) {
        match code {
            KeyCode::Char(c) if !ctrl => {
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::ConnectInput(input),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    input.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::ConnectInput(input),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    input.pop();
                }
            }
            KeyCode::Enter => {
                let args = if let Some(Pane::Connect {
                    overlay: ConnectOverlay::ConnectInput(input),
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
                } else if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut()
                {
                    *overlay = ConnectOverlay::None;
                }
            }
            KeyCode::Esc => {
                if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
                    *overlay = ConnectOverlay::None;
                }
            }
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Key editor overlay
    if let Some(Pane::Connect {
        overlay: ConnectOverlay::KeyEditor(editor),
        ..
    }) = app.tab().focused_pane()
    {
        let is_editing = editor.editing;
        let display_idx = editor.list_state.selected().unwrap_or(0);

        if is_editing {
            // Capture mode: Esc cancels, any other key sets the binding
            if code == KeyCode::Esc {
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::KeyEditor(editor),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    editor.editing = false;
                    editor.status = Some("Cancelled".into());
                }
            } else if let Some(entry_idx) = editor_binding_index(display_idx) {
                let entries = app.keybindings.entries();
                if let Some(entry) = entries.get(entry_idx) {
                    let new_kb = KeyBinding::new(code, ctrl, alt, shift);
                    app.keybindings
                        .set_binding(entry.group, entry.field, new_kb);
                    match app.keybindings.save() {
                        Ok(()) => {}
                        Err(e) => error!("save keybindings: {e}"),
                    }
                }
                if let Some(Pane::Connect {
                    overlay: ConnectOverlay::KeyEditor(editor),
                    ..
                }) = app.tab_mut().focused_pane_mut()
                {
                    editor.editing = false;
                    editor.status = Some("Saved!".into());
                }
            }
        } else {
            // Navigation mode
            match code {
                KeyCode::Up => {
                    if let Some(Pane::Connect {
                        overlay: ConnectOverlay::KeyEditor(editor),
                        ..
                    }) = app.tab_mut().focused_pane_mut()
                    {
                        editor_nav_up(&mut editor.list_state);
                    }
                }
                KeyCode::Down => {
                    if let Some(Pane::Connect {
                        overlay: ConnectOverlay::KeyEditor(editor),
                        ..
                    }) = app.tab_mut().focused_pane_mut()
                    {
                        editor_nav_down(&mut editor.list_state);
                    }
                }
                KeyCode::Enter => {
                    if editor_binding_index(display_idx).is_some()
                        && let Some(Pane::Connect {
                            overlay: ConnectOverlay::KeyEditor(editor),
                            ..
                        }) = app.tab_mut().focused_pane_mut()
                    {
                        editor.editing = true;
                        editor.status = None;
                    }
                }
                KeyCode::Esc => {
                    if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
                        *overlay = ConnectOverlay::None;
                    }
                }
                _ => {}
            }
        }
        return Some(Action::Continue);
    }

    // Normal connect pane
    let cb = &app.keybindings.connect;
    if cb.select_prev.matches(code, ctrl, alt, shift) {
        if let Some(Pane::Connect { list_state, .. }) = app.tab_mut().focused_pane_mut() {
            list_state.select_previous();
        }
    } else if cb.select_next.matches(code, ctrl, alt, shift) {
        if let Some(Pane::Connect { list_state, .. }) = app.tab_mut().focused_pane_mut() {
            list_state.select_next();
        }
    } else if cb.connect.matches(code, ctrl, alt, shift) {
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
    } else if cb.browser_menu.matches(code, ctrl, alt, shift) {
        if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
            let mut ms = ListState::default();
            ms.select(Some(0));
            *overlay = ConnectOverlay::BrowserMenu(ms);
        }
    } else if cb.manual_connect.matches(code, ctrl, alt, shift) {
        if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
            *overlay = ConnectOverlay::ConnectInput(String::new());
        }
    } else if cb.help.matches(code, ctrl, alt, shift) {
        if let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut() {
            *overlay = if matches!(overlay, ConnectOverlay::KeyEditor(_)) {
                ConnectOverlay::None
            } else {
                ConnectOverlay::KeyEditor(KeyEditorState::new())
            };
        }
    } else if code == KeyCode::Esc
        && let Some(Pane::Connect { overlay, .. }) = app.tab_mut().focused_pane_mut()
    {
        *overlay = ConnectOverlay::None;
    }
    Some(Action::Continue)
}

// ---------------------------------------------------------------------------
// Browser keys (shared for SFTP and SCP)
// ---------------------------------------------------------------------------

/// Returns `Some(Action)` if the focused pane is a browser (FileBrowser or SshBrowser).
///
/// SSH password prompts are handled as a special case before the shared path.
fn handle_browser_key_dispatch(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Action> {
    // SSH-specific: password prompt must be handled before the generic browser path.
    if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut()
        && browser.waiting_password
    {
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

    let browser_bindings = app.keybindings.browser.clone();

    // Get a trait-object reference to whichever browser is focused.
    let browser = app
        .tab_mut()
        .focused_pane_mut()
        .and_then(|p| p.as_browser_mut())?;

    // While connecting/authenticating, forward raw keystrokes.
    if browser.is_connecting() {
        browser.send_connect_key(code);
        return Some(Action::Continue);
    }

    // Log all key events while paste is in progress to diagnose
    // characters that go missing (e.g. backslash on Windows drag-and-drop).
    if !browser.core_mut().paste_buf.is_empty() {
        debug!(
            "paste key event: code={:?} ctrl={} alt={} shift={}",
            code, ctrl, alt, shift
        );
    }

    // Ignore ctrl/alt chars — they are not browser actions and must not
    // trigger paste accumulation.  However, during active paste accumulation
    // let them through: Windows sends backslash as Ctrl+Alt+\ (AltGr) in
    // drag-and-drop paths.
    if (ctrl || alt) && matches!(code, KeyCode::Char(_)) && browser.core_mut().paste_buf.is_empty()
    {
        return Some(Action::Continue);
    }

    match handle_browser_key(
        browser.core_mut(),
        code,
        ctrl,
        alt,
        shift,
        &browser_bindings,
    ) {
        BrowserKeyAction::Enter => browser.enter(),
        BrowserKeyAction::GoUp => browser.go_up(),
        BrowserKeyAction::Download => browser.download(),
        BrowserKeyAction::Upload => browser.upload(),
        BrowserKeyAction::Delete => browser.delete_focused(),
        BrowserKeyAction::ConfirmDeleteYes => browser.confirm_delete_yes(),
        BrowserKeyAction::Handled => {}
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
// Context menu mouse
// ---------------------------------------------------------------------------

fn context_menu_hit(
    col: u16,
    row: u16,
    last_area: Rect,
    menu: &crate::app::ContextMenu,
) -> Option<usize> {
    let rect = crate::app::context_menu_rect(menu.col, menu.row, last_area);
    // Inner area (inside border)
    let inner_x = rect.x + 1;
    let inner_y = rect.y + 1;
    let inner_w = rect.width.saturating_sub(2);
    let inner_h = crate::app::CONTEXT_MENU_ITEMS.len() as u16;
    if col >= inner_x && col < inner_x + inner_w && row >= inner_y && row < inner_y + inner_h {
        Some((row - inner_y) as usize)
    } else {
        None
    }
}

fn handle_context_menu_mouse(
    app: &mut App,
    kind: MouseEventKind,
    col: u16,
    row: u16,
    last_area: Rect,
) -> Action {
    let menu = app.context_menu.as_ref().unwrap();
    match kind {
        MouseEventKind::Drag(MouseButton::Right) | MouseEventKind::Moved => {
            let hit = context_menu_hit(col, row, last_area, menu);
            app.context_menu.as_mut().unwrap().selected = hit;
            Action::Continue
        }
        MouseEventKind::Up(MouseButton::Right) => {
            let selected = context_menu_hit(col, row, last_area, menu);
            app.context_menu = None;
            if let Some(idx) = selected {
                execute_context_menu_action(app, idx, last_area)
            } else {
                Action::Continue
            }
        }
        // Any other event dismisses the menu
        _ => {
            app.context_menu = None;
            Action::Continue
        }
    }
}

fn execute_context_menu_action(app: &mut App, idx: usize, last_area: Rect) -> Action {
    match idx {
        0 => app.new_tab(),
        1 => {
            let was_last = app.tab().leaf_count() == 1;
            if was_last {
                app.close_tab();
            } else {
                app.tab_mut().close_focused();
                app.resize_all(last_area);
            }
        }
        2 => app.tab_mut().split(Split::LeftRight, pane_inner(last_area)),
        3 => app.tab_mut().split(Split::TopBottom, pane_inner(last_area)),
        4 => return Action::Quit,
        _ => {}
    }
    Action::Continue
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

pub fn handle_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    last_area: Rect,
) -> Action {
    // ---- Context menu intercept ----
    if app.context_menu.is_some() {
        return handle_context_menu_mouse(app, kind, column, row, last_area);
    }
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
        return Action::Continue;
    };

    let prev_focus = app.tabs[app.selected_tab].focus_idx;

    if matches!(kind, MouseEventKind::Down(_)) {
        app.tabs[app.selected_tab].focus_idx = pane_idx;
    }

    // Right-click: open context menu (after focus is set)
    if matches!(kind, MouseEventKind::Down(MouseButton::Right)) {
        app.context_menu = Some(crate::app::ContextMenu {
            col: column,
            row,
            selected: None,
        });
        return Action::Continue;
    }

    // ---- Browser mouse (shared for both SFTP and SCP) ----
    let is_browser = app.tabs[app.selected_tab]
        .root
        .leaf(pane_idx)
        .is_some_and(|p| p.is_browser());

    if is_browser {
        handle_browser_mouse(app, kind, column, row, pane_area);
        return Action::Continue;
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
            return Action::Continue;
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
    Action::Continue
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
) {
    let leaf_count = app.tabs[app.selected_tab].root.leaf_count();

    let Some(browser) = app
        .tab_mut()
        .focused_pane_mut()
        .and_then(|p| p.as_browser_mut())
    else {
        return;
    };
    let action = browser
        .core_mut()
        .handle_mouse(kind, column, row, pane_area, leaf_count);

    if let Some(drag_action) = action {
        let Some(browser) = app
            .tab_mut()
            .focused_pane_mut()
            .and_then(|p| p.as_browser_mut())
        else {
            return;
        };
        match drag_action {
            DragAction::LocalToRemote => browser.upload(),
            DragAction::RemoteToLocal => browser.download(),
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
