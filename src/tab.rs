use ratatui::layout::Rect;

use crate::pane::{Pane, Split, pane_inner, remove_leaf};

pub struct Tab {
    pub name: String,
    pub root: Pane,
    pub focus_idx: usize,
}

impl Tab {
    pub fn new(name: &str) -> Self {
        Tab {
            name: name.to_string(),
            root: Pane::new_connect(),
            focus_idx: 0,
        }
    }

    pub fn leaf_count(&self) -> usize {
        self.root.leaf_count()
    }

    pub fn focus_next(&mut self) {
        self.focus_idx = (self.focus_idx + 1) % self.leaf_count();
    }

    pub fn focus_prev(&mut self) {
        if self.focus_idx == 0 {
            self.focus_idx = self.leaf_count() - 1;
        } else {
            self.focus_idx -= 1;
        }
    }

    pub fn display_name(&self) -> &str {
        if self.leaf_count() == 1 {
            match &self.root {
                Pane::Connect { .. } => "<connect>",
                Pane::Session { .. } => &self.name,
                Pane::FileBrowser { .. } => &self.name,
                _ => &self.name,
            }
        } else {
            &self.name
        }
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.root.leaf_mut(self.focus_idx)
    }

    pub fn split(&mut self, kind: Split, area: Rect) {
        let n = self.focus_idx;
        let count = self.leaf_count();
        if count == 1 {
            let old = std::mem::replace(&mut self.root, Pane::new_connect());
            self.root = Pane::Split {
                kind,
                children: vec![old, Pane::new_connect()],
            };
        } else {
            self.root.split_leaf(n, kind);
        }
        self.root.resize_all(area, self.leaf_count() > 1);
    }

    pub fn close_focused(&mut self) {
        let target = self.focus_idx;
        remove_leaf(&mut self.root, target);
        if self.focus_idx >= self.leaf_count().max(1) {
            self.focus_idx = self.leaf_count().saturating_sub(1);
        }
    }

    pub fn focused_cursor(&self, content: Rect) -> Option<(u16, u16)> {
        let areas = self.root.leaf_areas(content);
        let pane_area = areas.get(self.focus_idx)?;
        let leaf_count = self.leaf_count();
        let inner = if leaf_count > 1 {
            pane_inner(*pane_area)
        } else {
            *pane_area
        };
        if let Some(Pane::Session { terminal }) = self.root.leaf(self.focus_idx) {
            if let Some((cx, cy)) = terminal.cursor_pos() {
                let sx = inner.x + cx;
                let sy = inner.y + cy;
                if sx < inner.x + inner.width && sy < inner.y + inner.height {
                    return Some((sx, sy));
                }
            }
        }
        None
    }
}
