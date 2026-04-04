use ratatui::layout::Rect;

use crate::pane::{Pane, Split, pane_border_inner, remove_leaf};

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
            self.focus_idx = self.leaf_count().saturating_sub(1);
        } else {
            self.focus_idx -= 1;
        }
    }

    pub fn display_name(&self) -> &str {
        if self.leaf_count() == 1 && matches!(self.root, Pane::Connect { .. }) {
            "<connect>"
        } else {
            &self.name
        }
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.root.leaf_mut(self.focus_idx)
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.root.leaf(self.focus_idx)
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
        self.focus_idx = n + 1;
        self.root.resize_all(area, self.leaf_count() > 1);
    }

    pub fn close_focused(&mut self) {
        let target = self.focus_idx;
        remove_leaf(&mut self.root, target);
        let count = self.leaf_count();
        if self.focus_idx >= count {
            self.focus_idx = count.saturating_sub(1);
        }
    }

    pub fn focused_cursor(&self, content: Rect) -> Option<(u16, u16)> {
        let areas = self.root.leaf_areas(content);
        let pane_area = areas.get(self.focus_idx)?;
        let leaf_count = self.leaf_count();
        let inner = if leaf_count > 1 {
            pane_border_inner(*pane_area)
        } else {
            *pane_area
        };
        if let Some(Pane::Session { terminal, .. }) = self.root.leaf(self.focus_idx)
            && let Some((cx, cy)) = terminal.cursor_pos()
        {
            let sx = inner.x + cx;
            let sy = inner.y + cy;
            if sx < inner.x + inner.width && sy < inner.y + inner.height {
                return Some((sx, sy));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{Pane, Split};
    use ratatui::layout::Rect;

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn tab_initial_state() {
        let t = Tab::new("1");
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.focus_idx, 0);
        assert!(matches!(t.root, Pane::Connect { .. }));
        assert_eq!(t.display_name(), "<connect>");
    }

    #[test]
    fn tab_display_name_connect_shows_connect() {
        // a fresh tab with no session yet shows "<connect>"
        let t = Tab::new("vps");
        assert_eq!(t.display_name(), "<connect>");
    }

    #[test]
    fn tab_display_name_multi_pane_shows_number() {
        let mut t = Tab::new("myhost");
        t.split(Split::LeftRight, r(200, 50));
        // multi-pane tabs show the tab name (number), not the host
        assert_eq!(t.display_name(), "myhost");
    }

    #[test]
    fn tab_split_horizontal() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        assert_eq!(t.leaf_count(), 2);
    }

    #[test]
    fn tab_split_vertical() {
        let mut t = Tab::new("1");
        t.split(Split::TopBottom, r(200, 50));
        assert_eq!(t.leaf_count(), 2);
    }

    #[test]
    fn tab_double_split_gives_three_panes() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        // focus is already on the new pane (idx 1)
        t.split(Split::TopBottom, r(200, 50));
        assert_eq!(t.leaf_count(), 3);
    }

    #[test]
    fn tab_focus_next_wraps() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        // split moves focus to the new pane (idx 1)
        assert_eq!(t.focus_idx, 1);
        t.focus_next();
        assert_eq!(t.focus_idx, 0);
        t.focus_next();
        assert_eq!(t.focus_idx, 1);
    }

    #[test]
    fn tab_focus_prev_wraps() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        // split moves focus to the new pane (idx 1)
        assert_eq!(t.focus_idx, 1);
        t.focus_prev();
        assert_eq!(t.focus_idx, 0);
        t.focus_prev();
        assert_eq!(t.focus_idx, 1);
    }

    #[test]
    fn tab_close_focused_reduces_count() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        t.close_focused();
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn tab_close_focused_clamps_focus_index() {
        let mut t = Tab::new("1");
        t.split(Split::LeftRight, r(200, 50));
        t.focus_idx = 1;
        t.close_focused();
        assert_eq!(t.focus_idx, 0);
    }
}
