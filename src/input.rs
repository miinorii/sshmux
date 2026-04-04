use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use log::{debug, error, trace};
use ratatui::{layout::Rect, style::Color, widgets::ListState};

use crate::app::{App, PaneResizeDrag};
use crate::browser::{BrowserKeyAction, DragAction, SshBrowserState, handle_browser_key};
use crate::keybindings::KeyBinding;
use crate::pane::{
    ConnectOverlay, KeyEditorState, Pane, Split, editor_binding_index, editor_nav_down,
    editor_nav_up, hit_test_separator, pane_border_inner, pane_inner, split_areas,
    split_at_path_mut,
};

// ---------------------------------------------------------------------------
// Action — returned by input handlers to signal the main loop
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
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
                    #[cfg(not(test))]
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

    // ---- Pane resize drag intercept ----
    if let Some(ref drag) = app.pane_resize_drag {
        match kind {
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                let pos = if drag.horizontal { column } else { row };
                let delta = pos as i32 - drag.start_pos as i32;
                let total = drag.start_ratios.0 + drag.start_ratios.1;
                // Compute new left/top ratio from pixel delta.
                let orig_left_px = (drag.start_ratios.0 as u32 * drag.span as u32
                    / total as u32) as i32;
                let new_left_px = (orig_left_px + delta).max(3).min(drag.span as i32 - 3);
                let new_r0 =
                    ((new_left_px as u32 * total as u32) / drag.span as u32).max(1) as u16;
                let new_r1 = total.saturating_sub(new_r0).max(1);
                // Apply to the target Split node.
                let path = drag.path.clone();
                let sep_idx = drag.sep_idx;
                if let Some(Pane::Split { ratios, .. }) =
                    split_at_path_mut(&mut app.tabs[app.selected_tab].root, &path)
                {
                    ratios[sep_idx] = new_r0;
                    ratios[sep_idx + 1] = new_r1;
                }
                app.resize_all(last_area);
                return Action::Continue;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                app.pane_resize_drag = None;
                return Action::Continue;
            }
            _ => {
                app.pane_resize_drag = None;
                return Action::Continue;
            }
        }
    }

    // ---- Separator drag start ----
    if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        let content = pane_inner(last_area);
        if let Some(hit) =
            hit_test_separator(&app.tabs[app.selected_tab].root, content, column, row)
        {
            // Look up the Split node to read current ratios and compute span.
            let (start_ratios, span) = {
                let split_pane =
                    split_at_path_mut(&mut app.tabs[app.selected_tab].root, &hit.path);
                if let Some(Pane::Split {
                    kind,
                    ratios,
                    ..
                }) = split_pane
                {
                    let r0 = ratios[hit.sep_idx];
                    let r1 = ratios[hit.sep_idx + 1];
                    let areas = split_areas(hit.split_area, kind, ratios);
                    let a0 = areas[hit.sep_idx];
                    let a1 = areas[hit.sep_idx + 1];
                    let span = if hit.horizontal {
                        a0.width + 1 + a1.width
                    } else {
                        a0.height + a1.height
                    };
                    ((r0, r1), span)
                } else {
                    return Action::Continue;
                }
            };
            let start_pos = if hit.horizontal { column } else { row };
            app.pane_resize_drag = Some(PaneResizeDrag {
                path: hit.path,
                sep_idx: hit.sep_idx,
                horizontal: hit.horizontal,
                start_pos,
                start_ratios,
                span,
                split_area: hit.split_area,
            });
            return Action::Continue;
        }
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
            pane_border_inner(pane_area)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ContextMenu;
    use crate::keybindings::KeyBindings;
    use crate::pane::{ConnectOverlay, Pane};
    use crate::ssh_config::SshHost;
    use ratatui::layout::Rect;

    fn make_app() -> App {
        let mut app = App::test_new();
        app.keybindings = KeyBindings::default();
        app
    }

    fn make_app_with_hosts(n: usize) -> App {
        let mut app = make_app();
        app.hosts = (0..n)
            .map(|i| SshHost {
                label: format!("host{}", i),
            })
            .collect();
        app
    }

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    fn key(app: &mut App, code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> Action {
        handle_key(app, code, ctrl, alt, shift, area())
    }

    // ---- Context menu dismissal ----

    #[test]
    fn context_menu_dismissed_by_any_key() {
        let mut app = make_app();
        app.context_menu = Some(ContextMenu {
            col: 10,
            row: 10,
            selected: None,
        });
        let action = key(&mut app, KeyCode::Char('x'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(app.context_menu.is_none());
    }

    #[test]
    fn context_menu_dismissed_by_esc() {
        let mut app = make_app();
        app.context_menu = Some(ContextMenu {
            col: 10,
            row: 10,
            selected: None,
        });
        let action = key(&mut app, KeyCode::Esc, false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(app.context_menu.is_none());
    }

    #[test]
    fn context_menu_dismissed_by_alt_key() {
        let mut app = make_app();
        app.context_menu = Some(ContextMenu {
            col: 10,
            row: 10,
            selected: None,
        });
        // Alt+Q would normally quit, but context menu intercepts first
        let action = key(&mut app, KeyCode::Char('q'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert!(app.context_menu.is_none());
    }

    // ---- Global shortcuts ----

    #[test]
    fn global_quit() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('q'), false, true, false);
        assert_eq!(action, Action::Quit);
    }

    #[test]
    fn global_new_tab() {
        let mut app = make_app();
        assert_eq!(app.tabs.len(), 1);
        let action = key(&mut app, KeyCode::Char('t'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.selected_tab, 1);
    }

    #[test]
    fn global_close_last_pane_closes_tab() {
        let mut app = make_app();
        app.new_tab(); // 2 tabs
        app.selected_tab = 0;
        let action = key(&mut app, KeyCode::Char('w'), false, true, false);
        assert_eq!(action, Action::Continue);
        // Closing the only pane in tab 0 closes that tab
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn global_close_one_pane_in_split() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        assert_eq!(app.tab().leaf_count(), 2);
        let action = key(&mut app, KeyCode::Char('w'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().leaf_count(), 1);
    }

    #[test]
    fn global_next_tab() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 0;
        let action = key(&mut app, KeyCode::Right, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 1);
    }

    #[test]
    fn global_next_tab_wraps() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 1;
        let action = key(&mut app, KeyCode::Right, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn global_prev_tab() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 1;
        let action = key(&mut app, KeyCode::Left, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn global_prev_tab_wraps() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 0;
        let action = key(&mut app, KeyCode::Left, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 1);
    }

    #[test]
    fn global_next_pane() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 0;
        let action = key(&mut app, KeyCode::Down, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().focus_idx, 1);
    }

    #[test]
    fn global_prev_pane() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 1;
        let action = key(&mut app, KeyCode::Up, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().focus_idx, 0);
    }

    #[test]
    fn global_split_horizontal() {
        let mut app = make_app();
        assert_eq!(app.tab().leaf_count(), 1);
        let action = key(&mut app, KeyCode::Char('-'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().leaf_count(), 2);
    }

    #[test]
    fn global_split_vertical() {
        let mut app = make_app();
        assert_eq!(app.tab().leaf_count(), 1);
        let action = key(&mut app, KeyCode::Char('+'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().leaf_count(), 2);
    }

    // ---- Connect pane: host selection ----

    #[test]
    fn connect_select_next() {
        let mut app = make_app_with_hosts(3);
        // Initial selection is 0 (select_first in new_connect)
        let action = key(&mut app, KeyCode::Down, false, false, false);
        assert_eq!(action, Action::Continue);
        if let Some(Pane::Connect { list_state, .. }) = app.tab().focused_pane() {
            assert_eq!(list_state.selected(), Some(1));
        } else {
            panic!("expected Connect pane");
        }
    }

    #[test]
    fn connect_select_prev() {
        let mut app = make_app_with_hosts(3);
        // Move to index 1 first
        key(&mut app, KeyCode::Down, false, false, false);
        let action = key(&mut app, KeyCode::Up, false, false, false);
        assert_eq!(action, Action::Continue);
        if let Some(Pane::Connect { list_state, .. }) = app.tab().focused_pane() {
            assert_eq!(list_state.selected(), Some(0));
        } else {
            panic!("expected Connect pane");
        }
    }

    // ---- Connect pane: overlays ----

    #[test]
    fn connect_open_browser_menu() {
        let mut app = make_app_with_hosts(1);
        let action = key(&mut app, KeyCode::Char('b'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::BrowserMenu(_),
                ..
            })
        ));
    }

    #[test]
    fn connect_open_manual_connect() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('c'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::ConnectInput(_),
                ..
            })
        ));
    }

    #[test]
    fn connect_open_key_editor() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('h'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            })
        ));
    }

    #[test]
    fn key_editor_h_in_nav_mode_is_noop() {
        let mut app = make_app();
        // Open key editor
        key(&mut app, KeyCode::Char('h'), false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            })
        ));
        // 'h' inside the editor (nav mode) is unrecognized — overlay stays open
        key(&mut app, KeyCode::Char('h'), false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            })
        ));
    }

    #[test]
    fn connect_esc_closes_overlay() {
        let mut app = make_app();
        // Open browser menu
        key(&mut app, KeyCode::Char('b'), false, false, false);
        let action = key(&mut app, KeyCode::Esc, false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    #[test]
    fn connect_esc_on_no_overlay_is_noop() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Esc, false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    // ---- Browser menu overlay ----

    #[test]
    fn browser_menu_navigate() {
        let mut app = make_app_with_hosts(1);
        key(&mut app, KeyCode::Char('b'), false, false, false);
        // Initial selection is 0
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(ms.selected(), Some(0));
        } else {
            panic!("expected BrowserMenu");
        }
        // Move down
        key(&mut app, KeyCode::Down, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(ms.selected(), Some(1));
        } else {
            panic!("expected BrowserMenu");
        }
        // Move up
        key(&mut app, KeyCode::Up, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(ms.selected(), Some(0));
        } else {
            panic!("expected BrowserMenu");
        }
    }

    #[test]
    fn browser_menu_esc_closes() {
        let mut app = make_app_with_hosts(1);
        key(&mut app, KeyCode::Char('b'), false, false, false);
        key(&mut app, KeyCode::Esc, false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    // ---- Connect input overlay ----

    #[test]
    fn connect_input_char_append() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        key(&mut app, KeyCode::Char('h'), false, false, false);
        key(&mut app, KeyCode::Char('o'), false, false, false);
        key(&mut app, KeyCode::Char('s'), false, false, false);
        key(&mut app, KeyCode::Char('t'), false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(input, "host");
        } else {
            panic!("expected ConnectInput");
        }
    }

    #[test]
    fn connect_input_backspace() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        key(&mut app, KeyCode::Char('a'), false, false, false);
        key(&mut app, KeyCode::Char('b'), false, false, false);
        key(&mut app, KeyCode::Backspace, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(input, "a");
        } else {
            panic!("expected ConnectInput");
        }
    }

    #[test]
    fn connect_input_esc_closes() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        key(&mut app, KeyCode::Esc, false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    #[test]
    fn connect_input_enter_empty_closes() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        // Enter on empty input closes overlay
        key(&mut app, KeyCode::Enter, false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    #[test]
    fn connect_input_ctrl_char_ignored() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        // Ctrl+char should not be appended
        key(&mut app, KeyCode::Char('a'), true, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        }) = app.tab().focused_pane()
        {
            assert_eq!(input, "");
        } else {
            panic!("expected ConnectInput");
        }
    }

    // ---- Key editor overlay: navigation ----

    #[test]
    fn key_editor_initial_selection() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            // Initial selection is 1 (first binding, index 0 is header)
            assert_eq!(editor.list_state.selected(), Some(1));
            assert!(!editor.editing);
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_nav_down_skips_header() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Navigate to index 9 (last global binding), next should skip header at 10
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab_mut().focused_pane_mut()
        {
            editor.list_state.select(Some(9));
        }
        key(&mut app, KeyCode::Down, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            // Should skip header at 10, land on 11
            assert_eq!(editor.list_state.selected(), Some(11));
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_nav_up_skips_header() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Position at index 11 (first connect binding), up should skip header at 10
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab_mut().focused_pane_mut()
        {
            editor.list_state.select(Some(11));
        }
        key(&mut app, KeyCode::Up, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            // Should skip header at 10, land on 9
            assert_eq!(editor.list_state.selected(), Some(9));
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_enter_starts_editing() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Selection starts at 1 (a binding row, not a header)
        key(&mut app, KeyCode::Enter, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            assert!(editor.editing);
            assert!(editor.status.is_none());
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_enter_on_header_noop() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Force selection to header index 0
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab_mut().focused_pane_mut()
        {
            editor.list_state.select(Some(0));
        }
        key(&mut app, KeyCode::Enter, false, false, false);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            assert!(!editor.editing);
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_esc_cancels_editing() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        key(&mut app, KeyCode::Enter, false, false, false); // start editing
        let action = key(&mut app, KeyCode::Esc, false, false, false);
        assert_eq!(action, Action::Continue);
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            assert!(!editor.editing);
            assert_eq!(editor.status.as_deref(), Some("Cancelled"));
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_esc_closes_in_nav_mode() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        key(&mut app, KeyCode::Esc, false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::None,
                ..
            })
        ));
    }

    #[test]
    fn key_editor_capture_sets_binding() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Selection is at 1 = first binding = global.quit (default: Alt+Q)
        key(&mut app, KeyCode::Enter, false, false, false); // start editing
        // Capture F5 as new binding
        key(&mut app, KeyCode::F(5), false, false, false);
        // Should no longer be editing, status = "Saved!"
        if let Some(Pane::Connect {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        }) = app.tab().focused_pane()
        {
            assert!(!editor.editing);
            assert_eq!(editor.status.as_deref(), Some("Saved!"));
        } else {
            panic!("expected KeyEditor");
        }
        // The binding should have changed
        assert_eq!(app.keybindings.global.quit.code, KeyCode::F(5));
        assert!(!app.keybindings.global.quit.alt);
    }

    #[test]
    fn key_editor_capture_bypasses_global_quit() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        key(&mut app, KeyCode::Enter, false, false, false); // start editing
        // Alt+Q would normally quit, but in capture mode it sets the binding instead
        let action = key(&mut app, KeyCode::Char('q'), false, true, false);
        assert_eq!(action, Action::Continue); // NOT Quit
        // Binding should be set to Alt+Q (same as before, but via capture)
        assert_eq!(app.keybindings.global.quit.code, KeyCode::Char('q'));
        assert!(app.keybindings.global.quit.alt);
    }

    // ---- Global shortcuts not intercepted when editor capturing ----

    #[test]
    fn editor_capture_blocks_all_global_shortcuts() {
        let mut app = make_app();
        app.new_tab(); // 2 tabs
        app.selected_tab = 0;
        key(&mut app, KeyCode::Char('h'), false, false, false);
        key(&mut app, KeyCode::Enter, false, false, false); // start editing

        // Alt+T (new tab) should be captured, not create a new tab
        let action = key(&mut app, KeyCode::Char('t'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tabs.len(), 2); // no new tab created
    }

    // ---- Tab switching ----

    #[test]
    fn prev_tab_single_tab_stays() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Left, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn next_tab_single_tab_stays() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Right, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    // ---- Focus cycling ----

    #[test]
    fn focus_next_wraps_around() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 1;
        let action = key(&mut app, KeyCode::Down, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().focus_idx, 0);
    }

    #[test]
    fn focus_prev_wraps_around() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 0;
        let action = key(&mut app, KeyCode::Up, false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.tab().focus_idx, 1);
    }

    // ---- Alt suppression on Connect pane ----

    #[test]
    fn unbound_alt_char_on_connect_pane_handled() {
        let mut app = make_app();
        // Alt+Z is not bound to anything — on a Connect pane, handle_connect_key
        // still returns Some(Action::Continue) for any unrecognized key.
        let action = key(&mut app, KeyCode::Char('z'), false, true, false);
        assert_eq!(action, Action::Continue);
    }

    // ---- Browser menu overlay absorbs keys ----

    #[test]
    fn browser_menu_unrecognized_key_noop() {
        let mut app = make_app_with_hosts(1);
        key(&mut app, KeyCode::Char('b'), false, false, false);
        // An unrecognized key in the menu is handled (returns Continue), menu stays open
        key(&mut app, KeyCode::Char('x'), false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect {
                overlay: ConnectOverlay::BrowserMenu(_),
                ..
            })
        ));
    }

    // ---- Multiple global actions in sequence ----

    #[test]
    fn new_tab_then_switch_back() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('t'), false, true, false); // new tab
        assert_eq!(app.selected_tab, 1);
        key(&mut app, KeyCode::Left, false, true, false); // prev tab
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn split_then_close_returns_to_single() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('+'), false, true, false); // split vertical
        assert_eq!(app.tab().leaf_count(), 2);
        key(&mut app, KeyCode::Char('w'), false, true, false); // close pane
        assert_eq!(app.tab().leaf_count(), 1);
    }

    // ---- Handle paste ----

    #[test]
    fn paste_ignored_on_connect_pane() {
        let mut app = make_app();
        // Should not panic — paste on non-session pane is a no-op
        handle_paste(&mut app, "hello world");
    }
}
