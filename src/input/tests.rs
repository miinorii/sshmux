use super::*;
use crate::app::ContextMenu;
use crate::keybindings::KeyBindings;
use crate::pane::Split;
use crate::ssh_config::SshHost;
use crossterm::event::{MouseButton, MouseEventKind};

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
        assert_eq!(input.text, "host");
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
        assert_eq!(input.text, "a");
    } else {
        panic!("expected ConnectInput");
    }
}

#[test]
fn connect_input_arrows_edit_mid_text() {
    let mut app = make_app();
    key(&mut app, KeyCode::Char('c'), false, false, false); // open input
    for ch in "host".chars() {
        key(&mut app, KeyCode::Char(ch), false, false, false);
    }
    key(&mut app, KeyCode::Left, false, false, false);
    key(&mut app, KeyCode::Left, false, false, false);
    key(&mut app, KeyCode::Char('X'), false, false, false); // hoXst
    key(&mut app, KeyCode::Home, false, false, false);
    key(&mut app, KeyCode::Delete, false, false, false); // drop leading h
    key(&mut app, KeyCode::End, false, false, false);
    key(&mut app, KeyCode::Char('!'), false, false, false);
    if let Some(Pane::Connect(ConnectPane {
        overlay: ConnectOverlay::ConnectInput(input),
        ..
    })) = app.tab().focused_pane()
    {
        assert_eq!(input.text, "oXst!");
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
        assert_eq!(input.text, "");
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
        assert_eq!(input.text, "u@h");
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
        assert_eq!(input.text, "user@host");
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
    use crate::components::browser::BrowserFocus;
    use crate::components::browser::sftp::SftpState;

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
