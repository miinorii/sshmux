//! Global shortcuts: quit, tab switching, focus movement, split, close, zoom.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;

use super::Action;
use crate::app::App;
use crate::pane::{FocusDir, Split, pane_inner};

/// Returns `Some(Action)` if a global shortcut was matched and handled.
pub(super) fn handle_global_key(
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
