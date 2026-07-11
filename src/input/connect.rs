//! Connect pane keys: host list, browser menu, manual-connect input, and the
//! key-editor overlay.

use crossterm::event::KeyCode;
use log::error;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use super::Action;
use crate::app::App;
use crate::components::connect::{
    ConnectOverlay, ConnectPane, InputField, KeyEditorState, editor_binding_index, editor_nav_down,
    editor_nav_up,
};
use crate::keybindings::KeyBinding;
use crate::pane::Pane;

/// Returns `Some(Action)` if the focused pane is a Connect pane.
pub(super) fn handle_connect_key(
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
        // Helper: get &mut InputField of the open ConnectInput overlay.
        macro_rules! input_mut {
            () => {
                match &mut connect_mut!().overlay {
                    ConnectOverlay::ConnectInput(input) => input,
                    _ => return Some(Action::Continue),
                }
            };
        }
        match code {
            // Accept plain chars and AltGr chars (reported as Ctrl+Alt on
            // Windows/Linux, e.g. `@` on AZERTY/QWERTZ). Reject Ctrl-only
            // combos so shortcuts like Ctrl+A aren't typed into the field.
            KeyCode::Char(c) if !ctrl || alt => input_mut!().insert(c),
            KeyCode::Backspace => input_mut!().backspace(),
            KeyCode::Delete => input_mut!().delete(),
            KeyCode::Left => input_mut!().move_left(),
            KeyCode::Right => input_mut!().move_right(),
            KeyCode::Home => input_mut!().move_home(),
            KeyCode::End => input_mut!().move_end(),
            KeyCode::Enter => {
                let args = if let Some(Pane::Connect(p)) = app.tab().focused_pane()
                    && let ConnectOverlay::ConnectInput(input) = &p.overlay
                {
                    let trimmed = input.text.trim().to_string();
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
        connect_mut!().overlay = ConnectOverlay::ConnectInput(InputField::new());
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
