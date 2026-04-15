//! Click, drag, and release handling across both browser panels.

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::super::browser_layout;
use super::{BrowserCore, BrowserFocus, DragAction, DragState};
use crate::pane::pane_border_inner;

impl BrowserCore {
    /// Build a drag label from the current selection. Returns None if nothing to drag.
    pub fn drag_label(&self) -> Option<String> {
        let indices = self.selected_indices();
        if indices.is_empty() {
            return None;
        }
        let entries = match self.focus {
            BrowserFocus::Local => &self.local.entries,
            BrowserFocus::Remote => &self.remote.entries,
        };
        if indices.len() > 1 {
            Some(format!("{} files", indices.len()))
        } else {
            entries.get(indices[0]).map(|e| e.name.clone())
        }
    }

    pub fn click_select(&mut self, col: u16, row: u16, pane_area: Rect, leaf_count: usize) {
        let outer_inner = if leaf_count > 1 {
            pane_border_inner(pane_area)
        } else {
            pane_area
        };

        let layout = browser_layout(outer_inner);
        let in_remote = col >= layout.remote_panel.x;
        let panel_area = if in_remote {
            layout.remote_panel
        } else {
            layout.local_panel
        };

        // Each panel has its own block border (1-cell inset)
        let list_y = panel_area.y + 1;
        let list_height = panel_area.height.saturating_sub(2);

        if row < list_y || row >= list_y + list_height {
            self.deselect_all(in_remote);
            return;
        }

        let click_row = (row - list_y) as usize;

        if in_remote {
            let offset = self.remote.sel.offset();
            let idx = offset + click_row;
            if idx < self.remote.entries.len() {
                self.remote.sel.select(Some(idx));
                self.needs_redraw = true;
            } else {
                self.deselect_all(in_remote);
            }
        } else if let Some((drives, drive_sel)) = &mut self.drive_picker {
            let offset = drive_sel.offset();
            let idx = offset + click_row;
            if idx < drives.len() {
                drive_sel.select(Some(idx));
                self.needs_redraw = true;
            }
        } else {
            let offset = self.local.sel.offset();
            let idx = offset + click_row;
            if idx < self.local.entries.len() {
                self.local.sel.select(Some(idx));
                self.needs_redraw = true;
            } else {
                self.deselect_all(false);
            }
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16, pane_area: Rect, leaf_count: usize) {
        let outer_inner = if leaf_count > 1 {
            pane_border_inner(pane_area)
        } else {
            pane_area
        };
        let layout = browser_layout(outer_inner);
        self.focus = if col >= layout.remote_panel.x {
            BrowserFocus::Remote
        } else {
            BrowserFocus::Local
        };
        self.click_select(col, row, pane_area, leaf_count);
    }

    /// Handle all mouse events for browser panes. Returns `Some(DragAction)`
    /// on mouse-up when the drag crossed panels (caller should trigger transfer).
    pub fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        col: u16,
        row: u16,
        pane_area: Rect,
        leaf_count: usize,
    ) -> Option<DragAction> {
        if self.drop_confirm.is_some() {
            return None;
        }
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_click(col, row, pane_area, leaf_count);
                if let Some(label) = self.drag_label() {
                    self.drag = Some(DragState {
                        origin: self.focus,
                        label,
                        mouse_col: col,
                        mouse_row: row,
                    });
                }
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(ref mut d) = self.drag {
                    d.mouse_col = col;
                    d.mouse_row = row;
                    self.needs_redraw = true;
                }
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                let indices = self.selected_indices();
                if indices.len() > 1 {
                    self.queue_transfers_from_indices(&indices);
                    self.clear_selection();
                }
                self.handle_drag_release(col, pane_area, leaf_count)
            }
            _ => None,
        }
    }

    pub fn handle_drag_release(
        &mut self,
        col: u16,
        pane_area: Rect,
        leaf_count: usize,
    ) -> Option<DragAction> {
        let outer_inner = if leaf_count > 1 {
            pane_border_inner(pane_area)
        } else {
            pane_area
        };
        let layout = browser_layout(outer_inner);
        let in_remote = col >= layout.remote_panel.x;
        let drag_from = self.focus;
        self.focus = if in_remote {
            BrowserFocus::Remote
        } else {
            BrowserFocus::Local
        };
        if in_remote && drag_from == BrowserFocus::Local {
            Some(DragAction::LocalToRemote)
        } else if !in_remote && drag_from == BrowserFocus::Remote {
            Some(DragAction::RemoteToLocal)
        } else {
            None
        }
    }
}
