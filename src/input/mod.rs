//! Event → state glue: all key, mouse, and paste handling.
//!
//! `handle_key`, `handle_mouse`, and `handle_paste` are the entry points
//! called from the main loop. Per-domain handlers live in the submodules:
//! `global`, `connect`, `session`, and `browser` for keys; `mouse` for
//! everything mouse (context menu, pane-resize drags, SGR forwarding).

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::KeyCode;
use log::{debug, error};
use ratatui::layout::Rect;

use crate::app::App;
use crate::components::browser::{FileBrowser, SshBrowser};
use crate::components::connect::{ConnectOverlay, ConnectPane, KeyEditorState};
use crate::pane::Pane;

mod browser;
mod connect;
mod global;
mod mouse;
mod session;
#[cfg(test)]
mod tests;

use browser::handle_browser_key_dispatch;
use connect::handle_connect_key;
use global::handle_global_key;
use session::handle_session_key;

pub use mouse::{compute_drag_ratios, handle_mouse};

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

    // Manual-connect input overlay: insert the pasted text at the cursor.
    if let Some(Pane::Connect(p)) = app.tab_mut().focused_pane_mut()
        && let ConnectOverlay::ConnectInput(input) = &mut p.overlay
    {
        for c in text.chars().filter(|c| !c.is_control()) {
            input.insert(c);
        }
        return;
    }

    debug!("handle_paste: ignored (no paste target)");
}
