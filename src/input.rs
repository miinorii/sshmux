use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use log::{debug, error, trace};
use ratatui::{layout::Rect, widgets::ListState};

use crate::app::{App, CONTEXT_MENU_ITEMS, ContextMenu, PaneResizeDrag, context_menu_rect};
use crate::browser::{BrowserKeyAction, DragAction, FileBrowser, SshBrowser, handle_browser_key};
use crate::keybindings::KeyBinding;
use crate::pane::connect::{
    ConnectOverlay, ConnectPane, KeyEditorState, editor_binding_index, editor_nav_down,
    editor_nav_up,
};
use crate::pane::{
    FocusDir, Pane, Split, hit_test_separator, pane_border_inner, pane_inner, split_areas,
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
        Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(KeyEditorState { editing: true, .. }),
            ..
        }))
    );

    if !editor_capturing
        && let Some(action) = handle_global_key(app, code, ctrl, alt, shift, last_area)
    {
        return action;
    }

    // ---- Connect pane ----
    if let Some(action) = handle_connect_key(app, code, ctrl, alt, shift, last_area) {
        return action;
    }

    // ---- Exit overlay (exited session or browser) ----
    // Must run before the browser dispatch: the browser key path consumes
    // every key on a browser pane, which would make Reconnect / Close pane
    // unreachable once its PTY has exited.
    if handle_exit_overlay_key(app, code, last_area) {
        return Action::Continue;
    }

    // ---- Browser pane (SFTP or SCP) ----
    if let Some(action) = handle_browser_key_dispatch(app, code, ctrl, alt, shift) {
        return action;
    }

    // Unbound Alt+Char combos fall through to the session as an ESC prefix
    // (Meta) so remote readline/emacs bindings (Alt+B, Alt+F, Alt+.) work —
    // bound combos were already consumed by the global shortcut layer above.
    // Other unbound Alt combos (Alt+Enter, Alt+arrows…) are still suppressed.
    if !editor_capturing && alt && !ctrl && !matches!(code, KeyCode::Char(_)) {
        return Action::Continue;
    }

    handle_session_key(app, code, ctrl, alt, focused_pane_has_app_cursor);
    Action::Continue
}

// ---------------------------------------------------------------------------
// Global shortcuts
// ---------------------------------------------------------------------------

/// Returns `Some(Action)` if a global shortcut was matched and handled.
fn handle_global_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
    last_area: Rect,
) -> Option<Action> {
    let g = &app.keybindings.global;
    if g.quit.matches(code, ctrl, alt, shift) {
        return Some(Action::Quit);
    }
    if g.prev_tab.matches(code, ctrl, alt, shift) {
        if app.selected_tab > 0 {
            app.selected_tab -= 1;
        } else {
            app.selected_tab = app.tabs.len() - 1;
        }
        app.pane_resize_drag = None;
        return Some(Action::Continue);
    }
    if g.next_tab.matches(code, ctrl, alt, shift) {
        app.selected_tab = (app.selected_tab + 1) % app.tabs.len();
        app.pane_resize_drag = None;
        return Some(Action::Continue);
    }
    let focus_dir = if g.focus_left.matches(code, ctrl, alt, shift) {
        Some(FocusDir::Left)
    } else if g.focus_right.matches(code, ctrl, alt, shift) {
        Some(FocusDir::Right)
    } else if g.focus_up.matches(code, ctrl, alt, shift) {
        Some(FocusDir::Up)
    } else if g.focus_down.matches(code, ctrl, alt, shift) {
        Some(FocusDir::Down)
    } else {
        None
    };
    if let Some(dir) = focus_dir {
        app.tab_mut().focus_dir(dir, pane_inner(last_area));
        if app.tab().zoom {
            app.resize_all(last_area);
        }
        return Some(Action::Continue);
    }
    if g.close.matches(code, ctrl, alt, shift) {
        app.close_focused_or_tab(last_area);
        return Some(Action::Continue);
    }
    if g.new_tab.matches(code, ctrl, alt, shift) {
        app.new_tab();
        return Some(Action::Continue);
    }
    if g.split_horizontal.matches(code, ctrl, alt, shift) {
        app.tab_mut().zoom = false;
        app.tab_mut().split(Split::TopBottom, pane_inner(last_area));
        return Some(Action::Continue);
    }
    if g.split_vertical.matches(code, ctrl, alt, shift) {
        app.tab_mut().zoom = false;
        app.tab_mut().split(Split::LeftRight, pane_inner(last_area));
        return Some(Action::Continue);
    }
    if g.zoom.matches(code, ctrl, alt, shift) {
        if app.tab().leaf_count() > 1 {
            app.tab_mut().zoom = !app.tab().zoom;
            app.pane_resize_drag = None;
            let content = pane_inner(last_area);
            if app.tab().zoom {
                let focus_idx = app.tab().focus_idx;
                if let Some(pane) = app.tab_mut().root.leaf_mut(focus_idx) {
                    pane.resize_all(content, false);
                }
            } else {
                app.resize_all(last_area);
            }
        }
        return Some(Action::Continue);
    }
    None
}

// ---------------------------------------------------------------------------
// Session key passthrough
// ---------------------------------------------------------------------------

/// Translate a key into the matching escape sequence and forward it to the
/// focused session pane. Also resets scrollback on any keypress.
fn handle_session_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    focused_pane_has_app_cursor: bool,
) {
    if let Some(Pane::Session { terminal, .. }) = app.tab_mut().focused_pane_mut() {
        terminal.reset_scroll();
    }

    // Ctrl+Arrow word-jump
    if ctrl && !alt {
        let seq = match code {
            KeyCode::Left => Some("\x1b[1;5D"),
            KeyCode::Right => Some("\x1b[1;5C"),
            KeyCode::Up => Some("\x1b[1;5A"),
            KeyCode::Down => Some("\x1b[1;5B"),
            _ => None,
        };
        if let Some(s) = seq {
            app.send_str(s);
            return;
        }
    }

    match code {
        KeyCode::Char(c) if ctrl && !alt => {
            // Convert Ctrl+<key> to its control byte. Some terminals report
            // Ctrl+C as Char('\x03') with CONTROL modifier instead of
            // Char('c') with CONTROL — handle both forms. Keys without a
            // control-byte meaning are dropped (never `wrapping_sub`, which
            // produced invalid >0x7F bytes for digits and punctuation).
            let byte = match c {
                c if c.is_ascii_control() => Some(c as u8),
                ' ' | '@' => Some(0x00), // Ctrl+Space / Ctrl+@ → NUL
                'a'..='z' => Some(c as u8 - b'a' + 1),
                'A'..='Z' => Some(c as u8 - b'A' + 1),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' => Some(0x1f),
                '?' => Some(0x7f),
                _ => None,
            };
            trace!(
                "ctrl+char: c={:?} (0x{:02X}) -> byte={:02X?}",
                c, c as u32, byte
            );
            if let Some(b) = byte {
                // All control bytes are < 0x80, so the char cast is a
                // single-byte UTF-8 encoding.
                app.send_char(b as char);
            }
        }
        // Unbound Alt+char → ESC prefix (Meta). AltGr chars arrive as
        // Ctrl+Alt and must fall through to the plain-char arm below.
        KeyCode::Char(c) if alt && !ctrl => {
            app.send_str(&format!("\x1b{c}"));
        }
        KeyCode::Char(c) => app.send_char(c),
        KeyCode::Enter => app.send_str("\r"),
        KeyCode::Backspace => app.send_str("\x7f"),
        KeyCode::Delete => app.send_str("\x1b[3~"),
        KeyCode::Tab => app.send_str("\t"),
        KeyCode::BackTab => app.send_str("\x1b[Z"),
        KeyCode::Left => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOD"
        } else {
            "\x1b[D"
        }),
        KeyCode::Right => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOC"
        } else {
            "\x1b[C"
        }),
        KeyCode::Up => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOA"
        } else {
            "\x1b[A"
        }),
        KeyCode::Down => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOB"
        } else {
            "\x1b[B"
        }),
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
        Some(Pane::Connect(_))
    ) {
        return None;
    }

    // Helper: get &mut ConnectPane for the focused pane.
    macro_rules! connect_mut {
        () => {
            match app.tab_mut().focused_pane_mut() {
                Some(Pane::Connect(p)) => p,
                _ => return Some(Action::Continue),
            }
        };
    }

    // Browser menu overlay
    if matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::BrowserMenu(_),
            ..
        }))
    ) {
        match code {
            KeyCode::Up => {
                if let ConnectOverlay::BrowserMenu(ms) = &mut connect_mut!().overlay {
                    ms.select_previous();
                }
            }
            KeyCode::Down => {
                if let ConnectOverlay::BrowserMenu(ms) = &mut connect_mut!().overlay {
                    ms.select_next();
                }
            }
            KeyCode::Enter => {
                let (host_idx, menu_idx) = if let Some(Pane::Connect(p)) = app.tab().focused_pane()
                    && let ConnectOverlay::BrowserMenu(ms) = &p.overlay
                {
                    (p.list_state.selected(), ms.selected())
                } else {
                    (None, None)
                };
                connect_mut!().overlay = ConnectOverlay::None;
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
            KeyCode::Esc => connect_mut!().overlay = ConnectOverlay::None,
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Connect input overlay
    if matches!(
        app.tab().focused_pane(),
        Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(_),
            ..
        }))
    ) {
        match code {
            // Accept plain chars and AltGr chars (reported as Ctrl+Alt on
            // Windows/Linux, e.g. `@` on AZERTY/QWERTZ). Reject Ctrl-only
            // combos so shortcuts like Ctrl+A aren't typed into the field.
            KeyCode::Char(c) if !ctrl || alt => {
                if let ConnectOverlay::ConnectInput(input) = &mut connect_mut!().overlay {
                    input.push(c);
                }
            }
            KeyCode::Backspace => {
                if let ConnectOverlay::ConnectInput(input) = &mut connect_mut!().overlay {
                    input.pop();
                }
            }
            KeyCode::Enter => {
                let args = if let Some(Pane::Connect(p)) = app.tab().focused_pane()
                    && let ConnectOverlay::ConnectInput(input) = &p.overlay
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
                    connect_mut!().overlay = ConnectOverlay::None;
                }
            }
            KeyCode::Esc => connect_mut!().overlay = ConnectOverlay::None,
            _ => {}
        }
        return Some(Action::Continue);
    }

    // Key editor overlay
    if let Some(Pane::Connect(p)) = app.tab().focused_pane()
        && let ConnectOverlay::KeyEditor(editor) = &p.overlay
    {
        let is_editing = editor.editing;
        let display_idx = editor.list_state.selected().unwrap_or(0);

        if is_editing {
            if code == KeyCode::Esc {
                if let ConnectOverlay::KeyEditor(ed) = &mut connect_mut!().overlay {
                    ed.editing = false;
                    ed.status = Some("Cancelled".into());
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
                if let ConnectOverlay::KeyEditor(ed) = &mut connect_mut!().overlay {
                    ed.editing = false;
                    ed.status = Some("Saved!".into());
                }
            }
        } else {
            match code {
                KeyCode::Up => {
                    if let ConnectOverlay::KeyEditor(ed) = &mut connect_mut!().overlay {
                        editor_nav_up(&mut ed.list_state);
                    }
                }
                KeyCode::Down => {
                    if let ConnectOverlay::KeyEditor(ed) = &mut connect_mut!().overlay {
                        editor_nav_down(&mut ed.list_state);
                    }
                }
                KeyCode::Enter => {
                    if editor_binding_index(display_idx).is_some()
                        && let ConnectOverlay::KeyEditor(ed) = &mut connect_mut!().overlay
                    {
                        ed.editing = true;
                        ed.status = None;
                    }
                }
                KeyCode::Esc => connect_mut!().overlay = ConnectOverlay::None,
                _ => {}
            }
        }
        return Some(Action::Continue);
    }

    // Normal connect pane
    let cb = &app.keybindings.connect;
    if cb.select_prev.matches(code, ctrl, alt, shift) {
        connect_mut!().list_state.select_previous();
    } else if cb.select_next.matches(code, ctrl, alt, shift) {
        connect_mut!().list_state.select_next();
    } else if cb.connect.matches(code, ctrl, alt, shift) {
        let selected = connect_mut!().list_state.selected();
        if let Some(idx) = selected {
            if let Err(e) = app.open_session(idx, last_area) {
                error!("open_session: {}", e);
            }
            app.resize_all(last_area);
        }
    } else if cb.browser_menu.matches(code, ctrl, alt, shift) {
        let mut ms = ListState::default();
        ms.select(Some(0));
        connect_mut!().overlay = ConnectOverlay::BrowserMenu(ms);
    } else if cb.manual_connect.matches(code, ctrl, alt, shift) {
        connect_mut!().overlay = ConnectOverlay::ConnectInput(String::new());
    } else if cb.help.matches(code, ctrl, alt, shift) {
        let pane = connect_mut!();
        pane.overlay = if matches!(pane.overlay, ConnectOverlay::KeyEditor(_)) {
            ConnectOverlay::None
        } else {
            ConnectOverlay::KeyEditor(KeyEditorState::new())
        };
    } else if code == KeyCode::Esc {
        connect_mut!().overlay = ConnectOverlay::None;
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
        browser.handle_password_key(code);
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
// Exit overlay keys (exited session or browser)
// ---------------------------------------------------------------------------

/// Get the Reconnect/Close selection of the focused exited pane, if the pane
/// shows an exit overlay.
fn exit_selection_mut(pane: &mut Pane) -> Option<&mut u8> {
    match pane {
        Pane::Session { exit_selection, .. } => Some(exit_selection),
        p => p.as_browser_mut().map(|b| &mut b.core_mut().exit_selection),
    }
}

/// Returns true if the focused pane is an exited session/browser and the
/// event was consumed by its Reconnect / Close overlay.
fn handle_exit_overlay_key(app: &mut App, code: KeyCode, last_area: Rect) -> bool {
    let exited = match app.tab().focused_pane() {
        Some(Pane::Session { terminal, .. }) => terminal.process_exited(),
        Some(p) => p.as_browser().is_some_and(|b| b.process_exited()),
        None => false,
    };
    if !exited {
        return false;
    }

    match code {
        KeyCode::Left | KeyCode::Right => {
            if let Some(sel) = app
                .tab_mut()
                .focused_pane_mut()
                .and_then(exit_selection_mut)
            {
                *sel ^= 1;
            }
        }
        KeyCode::Enter => {
            let sel = app
                .tab_mut()
                .focused_pane_mut()
                .and_then(exit_selection_mut)
                .map(|s| *s)
                .unwrap_or(0);
            if sel == 0 {
                reconnect_focused(app, last_area);
            } else {
                app.close_focused_or_tab(last_area);
            }
        }
        _ => {}
    }
    true
}

/// Reconnect the focused exited pane, replacing it with a fresh one of the
/// same kind (session, SFTP browser, or SCP browser).
fn reconnect_focused(app: &mut App, last_area: Rect) {
    match app.tab().focused_pane() {
        Some(Pane::Session { ssh_args, .. }) => {
            let args = ssh_args.clone();
            if let Err(e) = app.open_session_raw(&args, last_area) {
                error!("reconnect: {}", e);
            }
            app.resize_all(last_area);
        }
        Some(Pane::FileBrowser { browser }) => {
            let host = browser.core.host.clone();
            replace_focused_pane(
                app,
                FileBrowser::new(&host).map(|b| Pane::FileBrowser { browser: b }),
                last_area,
            );
        }
        Some(Pane::SshBrowser { browser }) => {
            let host = browser.core.host.clone();
            replace_focused_pane(
                app,
                SshBrowser::new(&host).map(|b| Pane::SshBrowser { browser: b }),
                last_area,
            );
        }
        _ => {}
    }
}

fn replace_focused_pane(app: &mut App, result: Result<Pane>, last_area: Rect) {
    match result {
        Ok(new_pane) => {
            if let Some(pane) = app.tab_mut().focused_pane_mut() {
                *pane = new_pane;
            }
            app.resize_all(last_area);
        }
        Err(e) => error!("browser reconnect: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Context menu mouse
// ---------------------------------------------------------------------------

fn context_menu_hit(col: u16, row: u16, last_area: Rect, menu: &ContextMenu) -> Option<usize> {
    let rect = context_menu_rect(menu.col, menu.row, last_area);
    // Inner area (inside border)
    let inner_x = rect.x + 1;
    let inner_y = rect.y + 1;
    let inner_w = rect.width.saturating_sub(2);
    let inner_h = CONTEXT_MENU_ITEMS.len() as u16;
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
        1 => app.close_focused_or_tab(last_area),
        2 => {
            app.tab_mut().zoom = false;
            app.tab_mut().split(Split::LeftRight, pane_inner(last_area));
        }
        3 => {
            app.tab_mut().zoom = false;
            app.tab_mut().split(Split::TopBottom, pane_inner(last_area));
        }
        4 => return Action::Quit,
        _ => {}
    }
    Action::Continue
}

// ---------------------------------------------------------------------------
// Drag ratio computation
// ---------------------------------------------------------------------------

/// Given the two adjacent ratios at drag start, the combined pixel span, and
/// the cursor delta, return the new pair of ratios. Enforces a 3-pixel minimum
/// on each side; spans too small to honour that minimum (< 7) are returned
/// unchanged — clamping there would go negative and overflow the ratio math.
pub fn compute_drag_ratios(start_ratios: (u16, u16), span: u16, delta: i32) -> (u16, u16) {
    let total = start_ratios.0 as u32 + start_ratios.1 as u32;
    if span < 7 || total == 0 {
        return start_ratios;
    }
    let orig_left_px = (start_ratios.0 as u32 * span as u32 / total) as i32;
    let new_left_px = (orig_left_px + delta).clamp(3, span as i32 - 3);
    let new_r0 = ((new_left_px as u32 * total) / span as u32).max(1) as u16;
    let new_r1 = (total as u16).saturating_sub(new_r0).max(1);
    (new_r0, new_r1)
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

    // ---- Pane resize drag intercept + start ----
    if handle_pane_resize_mouse(app, kind, column, row, last_area) {
        return Action::Continue;
    }

    let content = pane_inner(last_area);
    let zoom_active = app.tab().zoom && app.tabs[app.selected_tab].root.leaf_count() > 1;

    // In zoom mode the focused pane fills the entire content area; otherwise find
    // which pane the pointer is over using the real layout.
    let (pane_idx, pane_area) = if zoom_active {
        (app.tabs[app.selected_tab].focus_idx, content)
    } else {
        let areas = app.tabs[app.selected_tab].root.leaf_areas(content);
        match areas
            .iter()
            .enumerate()
            .find(|(_, area)| {
                column >= area.x
                    && column < area.x + area.width
                    && row >= area.y
                    && row < area.y + area.height
            })
            .map(|(i, area)| (i, *area))
        {
            Some(v) => v,
            None => return Action::Continue,
        }
    };

    let prev_focus = app.tabs[app.selected_tab].focus_idx;

    if matches!(kind, MouseEventKind::Down(_)) {
        app.tabs[app.selected_tab].focus_idx = pane_idx;
    }

    // Right-click: open context menu (after focus is set)
    if matches!(kind, MouseEventKind::Down(MouseButton::Right)) {
        app.context_menu = Some(ContextMenu {
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
        let effective_leaf_count = if zoom_active {
            1
        } else {
            app.tabs[app.selected_tab].root.leaf_count()
        };
        handle_browser_mouse(app, kind, column, row, pane_area, effective_leaf_count);
        return Action::Continue;
    }

    handle_session_mouse(
        app,
        kind,
        column,
        row,
        SessionMouseCtx {
            pane_idx,
            pane_area,
            prev_focus,
            zoom_active,
        },
    );
    Action::Continue
}

// ---------------------------------------------------------------------------
// Pane resize drag
// ---------------------------------------------------------------------------

/// Handle the pane-resize drag lifecycle (in-progress drag updates and new
/// drag starts from a separator hit). Returns true if the event was consumed.
fn handle_pane_resize_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    last_area: Rect,
) -> bool {
    if let Some(ref drag) = app.pane_resize_drag {
        match kind {
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                let pos = if drag.horizontal { column } else { row };
                let delta = pos as i32 - drag.start_pos as i32;
                let (new_r0, new_r1) = compute_drag_ratios(drag.start_ratios, drag.span, delta);
                let path = drag.path.clone();
                let sep_idx = drag.sep_idx;
                if let Some(Pane::Split { ratios, .. }) =
                    split_at_path_mut(&mut app.tabs[app.selected_tab].root, &path)
                {
                    ratios[sep_idx] = new_r0;
                    ratios[sep_idx + 1] = new_r1;
                }
                app.resize_all(last_area);
            }
            _ => {
                app.pane_resize_drag = None;
            }
        }
        return true;
    }

    // Separator drag start (disabled in zoom mode — no visible separators)
    if !app.tab().zoom && matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        let content = pane_inner(last_area);
        if let Some(hit) =
            hit_test_separator(&app.tabs[app.selected_tab].root, content, column, row)
        {
            let split_pane = split_at_path_mut(&mut app.tabs[app.selected_tab].root, &hit.path);
            let Some(Pane::Split { kind, ratios, .. }) = split_pane else {
                return true;
            };
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
            if span == 0 {
                return true;
            }
            let start_pos = if hit.horizontal { column } else { row };
            app.pane_resize_drag = Some(PaneResizeDrag {
                path: hit.path,
                sep_idx: hit.sep_idx,
                horizontal: hit.horizontal,
                start_pos,
                start_ratios: (r0, r1),
                span,
                split_area: hit.split_area,
            });
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Session mouse forwarding
// ---------------------------------------------------------------------------

struct SessionMouseCtx {
    pane_idx: usize,
    pane_area: Rect,
    prev_focus: usize,
    zoom_active: bool,
}

/// Forward a mouse event to the focused session pane. Handles scrollback
/// translation, alternate-screen arrow translation, and SGR mouse encoding.
fn handle_session_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    ctx: SessionMouseCtx,
) {
    let SessionMouseCtx {
        pane_idx,
        pane_area,
        prev_focus,
        zoom_active,
    } = ctx;
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
        let is_scroll = matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown);
        if is_scroll {
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

    if !(same_pane && pane_wants_mouse) {
        return;
    }

    let inner = if !zoom_active && app.tabs[app.selected_tab].root.leaf_count() > 1 {
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
        MouseEventKind::Down(MouseButton::Left) => format!("\x1b[<0;{};{}M", col + 1, r + 1),
        MouseEventKind::Up(MouseButton::Left) => format!("\x1b[<0;{};{}m", col + 1, r + 1),
        MouseEventKind::Down(MouseButton::Middle) => format!("\x1b[<1;{};{}M", col + 1, r + 1),
        MouseEventKind::Up(MouseButton::Middle) => format!("\x1b[<1;{};{}m", col + 1, r + 1),
        MouseEventKind::ScrollUp => format!("\x1b[<64;{};{}M", col + 1, r + 1),
        MouseEventKind::ScrollDown => format!("\x1b[<65;{};{}M", col + 1, r + 1),
        MouseEventKind::Drag(MouseButton::Left) => format!("\x1b[<32;{};{}M", col + 1, r + 1),
        MouseEventKind::Drag(MouseButton::Middle) => format!("\x1b[<33;{};{}M", col + 1, r + 1),
        MouseEventKind::Moved if wants_motion => format!("\x1b[<35;{};{}M", col + 1, r + 1),
        _ => String::new(),
    };
    if !seq.is_empty() {
        app.send_str(&seq);
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
    leaf_count: usize,
) {
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

    // Session: forward as bracketed paste.
    if matches!(app.tab().focused_pane(), Some(Pane::Session { .. })) {
        debug!("handle_paste: forwarding to session as bracketed paste");
        let bracketed = format!("\x1b[200~{}\x1b[201~", text);
        app.send_str(&bracketed);
        return;
    }

    // SCP password prompt: paste the password (control chars stripped so a
    // trailing newline doesn't auto-submit).
    if let Some(Pane::SshBrowser { browser }) = app.tab_mut().focused_pane_mut()
        && browser.waiting_password
    {
        for c in text.chars().filter(|c| !c.is_control()) {
            browser.password_char(c);
        }
        return;
    }

    // Browser: feed the drag-and-drop detection buffer. Terminals that
    // deliver file drops as bracketed paste (e.g. Windows Terminal) arrive
    // here instead of as individual key events.
    if let Some(browser) = app
        .tab_mut()
        .focused_pane_mut()
        .and_then(|p| p.as_browser_mut())
    {
        let core = browser.core_mut();
        core.paste_buf.push_str(text);
        core.paste_deadline = Some(Instant::now() + Duration::from_millis(50));
        debug!(
            "handle_paste: {} chars into browser paste buffer",
            text.len()
        );
        return;
    }

    // Manual-connect input overlay: append the pasted text.
    if let Some(Pane::Connect(p)) = app.tab_mut().focused_pane_mut()
        && let ConnectOverlay::ConnectInput(input) = &mut p.overlay
    {
        input.extend(text.chars().filter(|c| !c.is_control()));
        return;
    }

    debug!("handle_paste: ignored (no paste target)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ContextMenu;
    use crate::keybindings::KeyBindings;
    use crate::pane::Pane;
    use crate::pane::connect::{ConnectOverlay, ConnectPane};
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
        let action = key(&mut app, KeyCode::Char('k'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 1);
    }

    #[test]
    fn global_next_tab_wraps() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 1;
        let action = key(&mut app, KeyCode::Char('k'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn global_prev_tab() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 1;
        let action = key(&mut app, KeyCode::Char('j'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn global_prev_tab_wraps() {
        let mut app = make_app();
        app.new_tab();
        app.selected_tab = 0;
        let action = key(&mut app, KeyCode::Char('j'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 1);
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

    // ---- Zoom ----

    #[test]
    fn zoom_toggle_on_single_pane_is_noop() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('z'), false, true, false);
        assert!(!app.tab().zoom);
    }

    #[test]
    fn zoom_toggles_on_split() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        key(&mut app, KeyCode::Char('z'), false, true, false);
        assert!(app.tab().zoom);
        key(&mut app, KeyCode::Char('z'), false, true, false);
        assert!(!app.tab().zoom);
    }

    #[test]
    fn zoom_cleared_on_split_horizontal() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().zoom = true;
        key(&mut app, KeyCode::Char('-'), false, true, false);
        assert!(!app.tab().zoom);
    }

    #[test]
    fn zoom_cleared_on_split_vertical() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().zoom = true;
        key(&mut app, KeyCode::Char('+'), false, true, false);
        assert!(!app.tab().zoom);
    }

    #[test]
    fn zoom_cleared_on_close_pane() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().zoom = true;
        key(&mut app, KeyCode::Char('w'), false, true, false);
        assert!(!app.tab().zoom);
    }

    #[test]
    fn zoom_is_per_tab() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        key(&mut app, KeyCode::Char('z'), false, true, false);
        assert!(app.tabs[0].zoom);
        app.new_tab();
        app.tabs[1].split(Split::LeftRight, area());
        assert!(!app.tabs[1].zoom); // new tab starts unzoomed
        app.selected_tab = 0;
        assert!(app.tabs[0].zoom); // original tab still zoomed
    }

    #[test]
    fn zoom_toggling_one_tab_does_not_affect_other() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.new_tab();
        app.tabs[1].split(Split::LeftRight, area());
        app.tabs[1].zoom = true;
        app.selected_tab = 0;
        key(&mut app, KeyCode::Char('z'), false, true, false); // zoom tab 0
        assert!(app.tabs[0].zoom);
        assert!(app.tabs[1].zoom); // tab 1 unaffected
    }

    #[test]
    fn zoom_focus_dir_changes_focus() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 0;
        app.tab_mut().zoom = true;
        key(&mut app, KeyCode::Right, false, true, false); // focus_right
        assert_eq!(app.tab().focus_idx, 1);
        assert!(app.tab().zoom); // zoom stays on
    }

    #[test]
    fn zoom_focus_dir_noop_when_no_neighbor() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 1;
        app.tab_mut().zoom = true;
        key(&mut app, KeyCode::Right, false, true, false); // no right neighbor
        assert_eq!(app.tab().focus_idx, 1); // unchanged
        assert!(app.tab().zoom);
    }

    #[test]
    fn zoom_mouse_click_maps_to_focused_pane() {
        let mut app = make_app();
        app.tab_mut().split(Split::LeftRight, area());
        app.tab_mut().focus_idx = 0;
        app.tab_mut().zoom = true;
        // Click on the right half — in normal mode this would focus pane 1,
        // in zoom mode it should stay on pane 0 (the zoomed pane fills the whole area).
        let full = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 51,
        };
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            150,
            20,
            full,
        );
        assert_eq!(app.tab().focus_idx, 0);
    }

    // ---- Connect pane: host selection ----

    #[test]
    fn connect_select_next() {
        let mut app = make_app_with_hosts(3);
        // Initial selection is 0 (select_first in new_connect)
        let action = key(&mut app, KeyCode::Down, false, false, false);
        assert_eq!(action, Action::Continue);
        if let Some(Pane::Connect(p)) = app.tab().focused_pane() {
            assert_eq!(p.list_state.selected(), Some(1));
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
        if let Some(Pane::Connect(p)) = app.tab().focused_pane() {
            assert_eq!(p.list_state.selected(), Some(0));
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::BrowserMenu(_),
                ..
            }))
        ));
    }

    #[test]
    fn connect_open_manual_connect() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('c'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::ConnectInput(_),
                ..
            }))
        ));
    }

    #[test]
    fn connect_open_key_editor() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('h'), false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            }))
        ));
    }

    #[test]
    fn key_editor_h_in_nav_mode_is_noop() {
        let mut app = make_app();
        // Open key editor
        key(&mut app, KeyCode::Char('h'), false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            }))
        ));
        // 'h' inside the editor (nav mode) is unrecognized — overlay stays open
        key(&mut app, KeyCode::Char('h'), false, false, false);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::KeyEditor(_),
                ..
            }))
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
        ));
    }

    #[test]
    fn connect_esc_on_no_overlay_is_noop() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Esc, false, false, false);
        assert_eq!(action, Action::Continue);
        assert!(matches!(
            app.tab().focused_pane(),
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
        ));
    }

    // ---- Browser menu overlay ----

    #[test]
    fn browser_menu_navigate() {
        let mut app = make_app_with_hosts(1);
        key(&mut app, KeyCode::Char('b'), false, false, false);
        // Initial selection is 0
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        })) = app.tab().focused_pane()
        {
            assert_eq!(ms.selected(), Some(0));
        } else {
            panic!("expected BrowserMenu");
        }
        // Move down
        key(&mut app, KeyCode::Down, false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        })) = app.tab().focused_pane()
        {
            assert_eq!(ms.selected(), Some(1));
        } else {
            panic!("expected BrowserMenu");
        }
        // Move up
        key(&mut app, KeyCode::Up, false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::BrowserMenu(ms),
            ..
        })) = app.tab().focused_pane()
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        })) = app.tab().focused_pane()
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        })) = app.tab().focused_pane()
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
        ));
    }

    #[test]
    fn connect_input_ctrl_char_ignored() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        // Ctrl+char should not be appended
        key(&mut app, KeyCode::Char('a'), true, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        })) = app.tab().focused_pane()
        {
            assert_eq!(input, "");
        } else {
            panic!("expected ConnectInput");
        }
    }

    #[test]
    fn connect_input_altgr_char_appended() {
        // AltGr is reported as Ctrl+Alt on Windows/Linux. Characters like `@`
        // on AZERTY/QWERTZ layouts arrive with both modifiers set and must
        // still be typed into the input.
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false);
        key(&mut app, KeyCode::Char('u'), false, false, false);
        key(&mut app, KeyCode::Char('@'), true, true, false);
        key(&mut app, KeyCode::Char('h'), false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        })) = app.tab().focused_pane()
        {
            assert_eq!(input, "u@h");
        } else {
            panic!("expected ConnectInput");
        }
    }

    // ---- Key editor overlay: navigation ----

    #[test]
    fn key_editor_initial_selection() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab_mut().focused_pane_mut()
        {
            // Last global binding before HEADER_CONNECT (now at 13)
            editor.list_state.select(Some(12));
        }
        key(&mut app, KeyCode::Down, false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
        {
            // Should skip header at 13, land on 14
            assert_eq!(editor.list_state.selected(), Some(14));
        } else {
            panic!("expected KeyEditor");
        }
    }

    #[test]
    fn key_editor_nav_up_skips_header() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('h'), false, false, false);
        // Position at index 14 (first connect binding after HEADER_CONNECT at 13)
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab_mut().focused_pane_mut()
        {
            editor.list_state.select(Some(14));
        }
        key(&mut app, KeyCode::Up, false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
        {
            // Should skip header at 13, land on 12
            assert_eq!(editor.list_state.selected(), Some(12));
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab_mut().focused_pane_mut()
        {
            editor.list_state.select(Some(0));
        }
        key(&mut app, KeyCode::Enter, false, false, false);
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::None,
                ..
            }))
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
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::KeyEditor(editor),
            ..
        })) = app.tab().focused_pane()
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
        let action = key(&mut app, KeyCode::Char('j'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
    }

    #[test]
    fn next_tab_single_tab_stays() {
        let mut app = make_app();
        let action = key(&mut app, KeyCode::Char('k'), false, true, false);
        assert_eq!(action, Action::Continue);
        assert_eq!(app.selected_tab, 0);
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
            Some(Pane::Connect(ConnectPane {
                overlay: ConnectOverlay::BrowserMenu(_),
                ..
            }))
        ));
    }

    // ---- Multiple global actions in sequence ----

    #[test]
    fn new_tab_then_switch_back() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('t'), false, true, false); // new tab
        assert_eq!(app.selected_tab, 1);
        key(&mut app, KeyCode::Char('j'), false, true, false); // prev tab
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
        // Should not panic — paste with no overlay open is a no-op
        handle_paste(&mut app, "hello world");
    }

    #[test]
    fn paste_into_connect_input_appends_without_control_chars() {
        let mut app = make_app();
        key(&mut app, KeyCode::Char('c'), false, false, false); // open manual connect
        handle_paste(&mut app, "user@host\n");
        if let Some(Pane::Connect(ConnectPane {
            overlay: ConnectOverlay::ConnectInput(input),
            ..
        })) = app.tab().focused_pane()
        {
            assert_eq!(input, "user@host");
        } else {
            panic!("expected ConnectInput");
        }
    }

    #[test]
    fn paste_into_browser_feeds_drop_buffer() {
        let mut app = make_app();
        let (fb, _h) = FileBrowser::with_mock();
        *app.tab_mut().focused_pane_mut().unwrap() = Pane::FileBrowser { browser: fb };
        handle_paste(&mut app, "C:/tmp/dropped file.txt");
        let core = app
            .tab()
            .focused_pane()
            .unwrap()
            .as_browser()
            .unwrap()
            .core();
        assert_eq!(core.paste_buf, "C:/tmp/dropped file.txt");
        assert!(core.paste_deadline.is_some());
    }

    #[test]
    fn paste_into_scp_password_prompt_fills_buffer() {
        let mut app = make_app();
        let (mut sb, _h) = SshBrowser::with_mock();
        sb.waiting_password = true;
        *app.tab_mut().focused_pane_mut().unwrap() = Pane::SshBrowser { browser: sb };
        handle_paste(&mut app, "hunter2\n");
        if let Some(Pane::SshBrowser { browser }) = app.tab().focused_pane() {
            assert_eq!(browser.password_buf, "hunter2");
        } else {
            panic!("expected SshBrowser");
        }
    }

    // ---- Exit overlay (H7 regression: keys must reach exited browsers) ----

    #[test]
    fn exited_browser_close_pane_via_overlay_keys() {
        let mut app = make_app();
        app.new_tab(); // 2 tabs, so closing the pane closes this tab
        let (fb, h) = FileBrowser::with_mock();
        h.set_exited(true);
        *app.tab_mut().focused_pane_mut().unwrap() = Pane::FileBrowser { browser: fb };

        // Right selects "Close pane", Enter executes it.
        key(&mut app, KeyCode::Right, false, false, false);
        key(&mut app, KeyCode::Enter, false, false, false);

        assert_eq!(
            app.tabs.len(),
            1,
            "exited browser pane should close its tab"
        );
    }

    #[test]
    fn exited_session_overlay_toggle_selection() {
        // The unified handler must still serve session panes: Left/Right
        // toggles between Reconnect and Close pane.
        let mut app = make_app();
        let (fb, h) = FileBrowser::with_mock();
        h.set_exited(true);
        *app.tab_mut().focused_pane_mut().unwrap() = Pane::FileBrowser { browser: fb };
        key(&mut app, KeyCode::Right, false, false, false);
        if let Some(b) = app.tab().focused_pane().unwrap().as_browser() {
            assert_eq!(b.core().exit_selection, 1);
        } else {
            panic!("expected browser pane");
        }
        key(&mut app, KeyCode::Left, false, false, false);
        if let Some(b) = app.tab().focused_pane().unwrap().as_browser() {
            assert_eq!(b.core().exit_selection, 0);
        }
    }

    #[test]
    fn live_browser_keys_still_dispatch_after_exit_check() {
        use crate::browser::common::BrowserFocus;
        use crate::browser::sftp::SftpState;

        let mut app = make_app();
        let (mut fb, _h) = FileBrowser::with_mock();
        fb.sftp_state = SftpState::Idle;
        *app.tab_mut().focused_pane_mut().unwrap() = Pane::FileBrowser { browser: fb };

        key(&mut app, KeyCode::Tab, false, false, false);

        let core = app
            .tab()
            .focused_pane()
            .unwrap()
            .as_browser()
            .unwrap()
            .core();
        assert_eq!(core.focus, BrowserFocus::Remote);
    }

    // ---- compute_drag_ratios ------------------------------------------------

    #[test]
    fn drag_ratios_zero_delta_unchanged() {
        let (r0, r1) = compute_drag_ratios((100, 100), 100, 0);
        assert_eq!(r0, 100);
        assert_eq!(r1, 100);
    }

    #[test]
    fn drag_ratios_positive_delta_grows_left() {
        // span=100, equal ratios, push right by 10px → left gets ~60%
        let (r0, r1) = compute_drag_ratios((100, 100), 100, 10);
        assert!(r0 > 100, "left ratio should grow with positive delta");
        assert_eq!(r0 + r1, 200, "total ratio must be preserved");
    }

    #[test]
    fn drag_ratios_negative_delta_shrinks_left() {
        let (r0, r1) = compute_drag_ratios((100, 100), 100, -20);
        assert!(r0 < 100, "left ratio should shrink with negative delta");
        assert_eq!(r0 + r1, 200);
    }

    #[test]
    fn drag_ratios_clamps_minimum_left() {
        // Pushing far left: left pane must keep at least 3px worth of ratio.
        let (r0, r1) = compute_drag_ratios((100, 100), 100, -200);
        // At 3px minimum: r0 = 3/100 * 200 = 6 (at least 1 after max())
        assert!(r0 >= 1, "left ratio must be at least 1");
        assert!(r1 >= 1, "right ratio must be at least 1");
        assert_eq!(r0 + r1, 200);
    }

    #[test]
    fn drag_ratios_clamps_minimum_right() {
        let (r0, r1) = compute_drag_ratios((100, 100), 100, 200);
        assert!(r0 >= 1);
        assert!(r1 >= 1);
        assert_eq!(r0 + r1, 200);
    }

    #[test]
    fn drag_ratios_asymmetric_start() {
        // Start at 200:100 ratio, zero delta → should return the same proportions.
        let (r0, r1) = compute_drag_ratios((200, 100), 90, 0);
        assert_eq!(r0 + r1, 300);
        assert!(r0 > r1, "left should still be larger than right");
    }

    #[test]
    fn drag_ratios_total_always_preserved() {
        for delta in [-50i32, -10, 0, 10, 50] {
            let (r0, r1) = compute_drag_ratios((150, 50), 80, delta);
            assert_eq!(r0 + r1, 200, "total must be preserved for delta={delta}");
        }
    }

    #[test]
    fn drag_ratios_tiny_span_returns_start_unchanged() {
        // Spans below 7 cannot honour the 3px minimum on both sides; the old
        // clamp went negative and overflowed the ratio math in debug builds.
        for span in 1..7u16 {
            for delta in [-100i32, -1, 0, 1, 100] {
                assert_eq!(
                    compute_drag_ratios((100, 100), span, delta),
                    (100, 100),
                    "span={span} delta={delta}"
                );
            }
        }
    }
}
