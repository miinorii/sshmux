//! Multi-selection state on `BrowserCore`: anchor, index set, and range update.

use super::super::parse::FsEntry;
use super::{BrowserCore, BrowserFocus};

impl BrowserCore {
    pub fn clear_selection(&mut self) {
        if !self.selected.is_empty() || self.select_anchor.is_some() {
            self.selected.clear();
            self.select_anchor = None;
            self.needs_redraw = true;
        }
    }

    /// Clear multi-selection and deselect the cursor in the clicked panel.
    pub(super) fn deselect_all(&mut self, in_remote: bool) {
        self.clear_selection();
        if in_remote {
            self.remote.sel.select(None);
        } else {
            self.local.sel.select(None);
        }
        self.needs_redraw = true;
    }

    /// Returns the currently focused index for the active panel.
    pub fn focused_index(&self) -> Option<usize> {
        match self.focus {
            BrowserFocus::Local => self.local.sel.selected(),
            BrowserFocus::Remote => self.remote.sel.selected(),
        }
    }

    /// Returns the entry under the cursor in the active panel.
    pub fn focused_entry(&self) -> Option<&FsEntry> {
        let entries = match self.focus {
            BrowserFocus::Local => &self.local.entries,
            BrowserFocus::Remote => &self.remote.entries,
        };
        self.focused_index().and_then(|i| entries.get(i))
    }

    /// Returns the indices to operate on: the multi-select set if non-empty,
    /// otherwise the single focused index (excluding `..` at index 0).
    ///
    /// Index 0 being `..` is an invariant upheld by both listers
    /// (`parse_ls` / `read_local_dir` always insert a synthetic parent link).
    pub fn selected_indices(&self) -> Vec<usize> {
        if !self.selected.is_empty() {
            self.selected.iter().copied().collect()
        } else if let Some(i) = self.focused_index() {
            if i == 0 { vec![] } else { vec![i] }
        } else {
            vec![]
        }
    }

    /// Update selection range between anchor and current cursor position.
    /// Skips index 0 (`..`).
    pub fn update_selection(&mut self) {
        let Some(anchor) = self.select_anchor else {
            return;
        };
        let Some(cursor) = self.focused_index() else {
            return;
        };
        let lo = anchor.min(cursor).max(1); // skip ".."
        let hi = anchor.max(cursor);
        self.selected.clear();
        for i in lo..=hi {
            self.selected.insert(i);
        }
        self.needs_redraw = true;
    }
}
