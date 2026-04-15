//! The `handle_browser_key` dispatch for browsers in idle mode.

use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use log::debug;
use ratatui::style::Color;

use super::{BrowserCore, BrowserFocus, BrowserKeyAction, PendingTransfer};
use crate::keybindings::{BrowserBindings, KeyBinding};

/// Handle a key event for a browser in idle mode (not connecting, not waiting
/// for password). Navigation keys are handled directly on `core`; actions that
/// need browser-specific logic are returned as a `BrowserKeyAction`.
pub fn handle_browser_key(
    core: &mut BrowserCore,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
    bindings: &BrowserBindings,
) -> BrowserKeyAction {
    // ---- Drop upload confirmation overlay ----
    if let Some(paths) = core.drop_confirm.as_ref() {
        match code {
            KeyCode::Up => {
                core.drop_scroll_y = core.drop_scroll_y.saturating_sub(1);
                core.needs_redraw = true;
            }
            KeyCode::Down => {
                let max_rows = 5.min((core.last_inner.height as usize).saturating_sub(6));
                let max_y = paths.len().saturating_sub(max_rows);
                if core.drop_scroll_y < max_y {
                    core.drop_scroll_y += 1;
                    core.needs_redraw = true;
                }
            }
            KeyCode::Left => {
                core.drop_scroll_x = core.drop_scroll_x.saturating_sub(1);
                core.needs_redraw = true;
            }
            KeyCode::Right => {
                let box_w = 60u16.min(core.last_inner.width.saturating_sub(4));
                let content_w = (box_w as usize).saturating_sub(2);
                let longest = paths
                    .iter()
                    .map(|p| format!("  {}", p.display()).len())
                    .max()
                    .unwrap_or(0);
                let max_scroll = longest.saturating_sub(content_w);
                if core.drop_scroll_x < max_scroll {
                    core.drop_scroll_x += 1;
                    core.needs_redraw = true;
                }
            }
            KeyCode::Char('y') => {
                if let Some(paths) = core.drop_confirm.take() {
                    core.transfer.pending = paths
                        .iter()
                        .map(|p| PendingTransfer {
                            path: p.to_string_lossy().replace('\\', "/"),
                            name: p
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                            is_dir: p.is_dir(),
                        })
                        .collect();
                    core.drop_scroll_x = 0;
                    core.drop_scroll_y = 0;
                }
                return BrowserKeyAction::Upload;
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                core.drop_confirm = None;
                core.drop_scroll_x = 0;
                core.drop_scroll_y = 0;
                core.status_msg = "File drop canceled".to_string();
                core.status_color = Color::Yellow;
                core.needs_redraw = true;
            }
            _ => {}
        }
        return BrowserKeyAction::Handled;
    }

    // ---- Paste accumulation: capture all chars while buffer is active ----
    if !core.paste_buf.is_empty()
        && let KeyCode::Char(c) = code
    {
        core.paste_buf.push(c);
        core.paste_deadline = Some(Instant::now() + Duration::from_millis(150));
        debug!("paste accumulating: {} chars total", core.paste_buf.len());
        return BrowserKeyAction::Handled;
    }

    // ---- Delete confirmation ----
    if core.delete.confirm.is_some() {
        match code {
            KeyCode::Char('y') => return BrowserKeyAction::ConfirmDeleteYes,
            KeyCode::Char('n') | KeyCode::Esc => {
                core.confirm_delete_no();
                return BrowserKeyAction::Handled;
            }
            _ => return BrowserKeyAction::Handled,
        }
    }

    // ---- Normal mode ----
    let m = |kb: &KeyBinding| kb.matches(code, ctrl, alt, shift);

    if m(&bindings.toggle_focus) {
        core.toggle_focus();
        BrowserKeyAction::Handled
    } else if code == KeyCode::Esc {
        core.dismiss_drive_picker();
        BrowserKeyAction::Handled
    } else if bindings.navigate_up.matches_ignore_shift(code, ctrl, alt) {
        if shift {
            if core.select_anchor.is_none() {
                core.select_anchor = core.focused_index();
            }
            core.nav_up();
            core.update_selection();
        } else {
            core.clear_selection();
            core.nav_up();
        }
        BrowserKeyAction::Handled
    } else if bindings.navigate_down.matches_ignore_shift(code, ctrl, alt) {
        if shift {
            if core.select_anchor.is_none() {
                core.select_anchor = core.focused_index();
            }
            core.nav_down();
            core.update_selection();
        } else {
            core.clear_selection();
            core.nav_down();
        }
        BrowserKeyAction::Handled
    } else if m(&bindings.scroll_left) {
        core.scroll_left();
        BrowserKeyAction::Handled
    } else if m(&bindings.scroll_right) {
        core.scroll_right();
        BrowserKeyAction::Handled
    } else if m(&bindings.enter) {
        core.clear_selection();
        BrowserKeyAction::Enter
    } else if m(&bindings.go_up) {
        core.clear_selection();
        BrowserKeyAction::GoUp
    } else if m(&bindings.transfer) {
        let indices = core.selected_indices();
        if indices.len() > 1 {
            core.queue_transfers_from_indices(&indices);
            core.clear_selection();
        }
        match core.focus {
            BrowserFocus::Remote => BrowserKeyAction::Download,
            BrowserFocus::Local => BrowserKeyAction::Upload,
        }
    } else if m(&bindings.delete) {
        BrowserKeyAction::Delete
    } else if let KeyCode::Char(c) = code {
        // Unrecognized char: start paste accumulation (no redraw to avoid
        // hundreds of draws while characters stream in from a file drop)
        debug!("paste accumulation started with char {:?}", c);
        core.paste_buf.push(c);
        core.paste_deadline = Some(Instant::now() + Duration::from_millis(150));
        BrowserKeyAction::Handled
    } else {
        BrowserKeyAction::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        BrowserCore, BrowserFocus, BrowserKeyAction, DeleteKind, DeleteLocation, DeleteTarget,
    };
    use super::handle_browser_key;
    use crate::keybindings::BrowserBindings;
    use crossterm::event::KeyCode;

    #[test]
    fn key_tab_toggles_focus() {
        let mut core = BrowserCore::new("host");
        assert_eq!(core.focus, BrowserFocus::Local);
        let action = handle_browser_key(
            &mut core,
            KeyCode::Tab,
            false,
            false,
            false,
            &BrowserBindings::default(),
        );
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert_eq!(core.focus, BrowserFocus::Remote);
    }

    #[test]
    fn key_enter_returns_enter_action() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Enter,
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::Enter
        ));
    }

    #[test]
    fn key_backspace_returns_go_up() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Backspace,
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::GoUp
        ));
    }

    #[test]
    fn key_t_remote_returns_download() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Char('t'),
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::Download
        ));
    }

    #[test]
    fn key_t_local_returns_upload() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Local;
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Char('t'),
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::Upload
        ));
    }

    #[test]
    fn key_delete_returns_delete() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Delete,
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::Delete
        ));
    }

    #[test]
    fn key_unknown_returns_handled() {
        let mut core = BrowserCore::new("host");
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::F(5),
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::Handled
        ));
    }

    #[test]
    fn key_y_during_confirm_returns_confirm_yes() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/tmp/x".to_string(),
        });
        assert!(matches!(
            handle_browser_key(
                &mut core,
                KeyCode::Char('y'),
                false,
                false,
                false,
                &BrowserBindings::default()
            ),
            BrowserKeyAction::ConfirmDeleteYes
        ));
    }

    #[test]
    fn key_n_during_confirm_cancels() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/tmp/x".to_string(),
        });
        let action = handle_browser_key(
            &mut core,
            KeyCode::Char('n'),
            false,
            false,
            false,
            &BrowserBindings::default(),
        );
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert!(core.delete.confirm.is_none());
    }

    #[test]
    fn key_esc_during_confirm_cancels() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/tmp/x".to_string(),
        });
        let action = handle_browser_key(
            &mut core,
            KeyCode::Esc,
            false,
            false,
            false,
            &BrowserBindings::default(),
        );
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert!(core.delete.confirm.is_none());
    }

    #[test]
    fn key_random_during_confirm_is_swallowed() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/tmp/x".to_string(),
        });
        let action = handle_browser_key(
            &mut core,
            KeyCode::Char('z'),
            false,
            false,
            false,
            &BrowserBindings::default(),
        );
        assert!(matches!(action, BrowserKeyAction::Handled));
        assert!(core.delete.confirm.is_some());
    }
}
