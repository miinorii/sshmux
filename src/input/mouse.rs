//! Mouse handling: context menu, pane-resize drags, browser panels, and SGR
//! mouse forwarding to session panes.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::Action;
use crate::app::{App, CONTEXT_MENU_ITEMS, ContextMenu, PaneResizeDrag, context_menu_rect};
use crate::components::browser::DragAction;
use crate::pane::{Node, Pane, Split, pane_border_inner, pane_inner, split_areas};

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
                if let Some(Node::Split { ratios, .. }) =
                    app.tabs[app.selected_tab].root.node_at_path_mut(&path)
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
        if let Some(hit) = app.tabs[app.selected_tab]
            .root
            .hit_test_separator(content, column, row)
        {
            let split_node = app.tabs[app.selected_tab].root.node_at_path_mut(&hit.path);
            let Some(Node::Split { kind, ratios, .. }) = split_node else {
                return true;
            };
            let r0 = ratios[hit.sep_idx];
            let r1 = ratios[hit.sep_idx + 1];
            let areas = split_areas(hit.split_area, *kind, ratios);
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
