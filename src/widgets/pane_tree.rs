//! Generic tiling split tree — the pane layout's data structure.
//!
//! `SplitTree<T>` is a recursive layout of leaves split horizontally
//! (`Split::LeftRight`, with a 1-column separator between children) or
//! vertically (`Split::TopBottom`, where each child's title row acts as the
//! separator). Leaves are addressed by their depth-first index. The tree
//! knows nothing about what a leaf *is* — sshmux instantiates it with
//! `T = Pane`.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

// ---------------------------------------------------------------------------
// Split direction / focus direction
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// Tree structure
// ---------------------------------------------------------------------------

pub enum Node<T> {
    Leaf(T),
    Split {
        kind: Split,
        ratios: Vec<u16>,
        children: Vec<Node<T>>,
    },
}

impl<T> Node<T> {
    /// Placeholder used while a node is being restructured; never observable.
    fn hole(kind: Split) -> Self {
        Node::Split {
            kind,
            ratios: vec![],
            children: vec![],
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    pub fn leaf(&self, n: usize) -> Option<&T> {
        match self {
            Node::Leaf(v) => (n == 0).then_some(v),
            Node::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut T> {
        match self {
            Node::Leaf(v) => (n == 0).then_some(v),
            Node::Split { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let count = child.leaf_count();
                    if n < offset + count {
                        return child.leaf_mut(n - offset);
                    }
                    offset += count;
                }
                None
            }
        }
    }

    fn collect_leaves_mut<'a>(&'a mut self, out: &mut Vec<&'a mut T>) {
        match self {
            Node::Leaf(v) => out.push(v),
            Node::Split { children, .. } => {
                for child in children {
                    child.collect_leaves_mut(out);
                }
            }
        }
    }

    fn leaf_areas_into(&self, area: Rect, out: &mut Vec<Rect>) {
        match self {
            Node::Leaf(_) => out.push(area),
            Node::Split {
                kind,
                children,
                ratios,
            } => {
                let areas = split_areas(area, *kind, ratios);
                for (child, a) in children.iter().zip(areas) {
                    child.leaf_areas_into(a, out);
                }
            }
        }
    }

    /// Split leaf `n` in `kind` direction; `new` (carried in an Option so the
    /// recursion can move it exactly once) becomes the second child.
    fn split_leaf(&mut self, n: usize, kind: Split, new: &mut Option<T>) -> bool {
        let Node::Split { children, .. } = self else {
            return false;
        };
        let mut offset = 0;
        for child in children.iter_mut() {
            let count = child.leaf_count();
            if n < offset + count {
                if count == 1 {
                    let old = std::mem::replace(child, Node::hole(kind));
                    *child = Node::Split {
                        kind,
                        children: vec![old, Node::Leaf(new.take().expect("split value"))],
                        ratios: vec![100, 100],
                    };
                } else {
                    child.split_leaf(n - offset, kind, new);
                }
                return true;
            }
            offset += count;
        }
        false
    }

    fn remove_leaf(&mut self, n: usize) {
        if let Node::Split {
            children, ratios, ..
        } = self
        {
            let mut offset = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 {
                        to_remove = Some(i);
                    } else {
                        child.remove_leaf(n - offset);
                    }
                    break;
                }
                offset += count;
            }
            if let Some(i) = to_remove {
                children.remove(i);
                ratios.remove(i);
            }
        }
        // Collapse a Split that is down to a single child into that child.
        if let Node::Split { children, .. } = self
            && children.len() == 1
        {
            let child = children.remove(0);
            *self = child;
        }
    }
}

/// A tiling layout of leaves. The root is public so leaf-type-specific
/// operations (rendering, ticking) can traverse it directly.
pub struct SplitTree<T> {
    pub root: Node<T>,
}

impl<T> SplitTree<T> {
    pub fn new(leaf: T) -> Self {
        SplitTree {
            root: Node::Leaf(leaf),
        }
    }

    pub fn leaf_count(&self) -> usize {
        self.root.leaf_count()
    }

    pub fn leaf(&self, n: usize) -> Option<&T> {
        self.root.leaf(n)
    }

    pub fn leaf_mut(&mut self, n: usize) -> Option<&mut T> {
        self.root.leaf_mut(n)
    }

    /// All leaves in depth-first (display) order.
    pub fn leaves_mut(&mut self) -> Vec<&mut T> {
        let mut out = Vec::new();
        self.root.collect_leaves_mut(&mut out);
        out
    }

    /// The screen rectangle of every leaf, in depth-first order.
    pub fn leaf_areas(&self, area: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        self.root.leaf_areas_into(area, &mut out);
        out
    }

    /// Split leaf `n`, inserting `new` as its sibling. Splitting the root
    /// leaf wraps it in a new Split node.
    pub fn split_leaf(&mut self, n: usize, kind: Split, new: T) {
        if matches!(self.root, Node::Leaf(_)) {
            let old = std::mem::replace(&mut self.root, Node::hole(kind));
            self.root = Node::Split {
                kind,
                children: vec![old, Node::Leaf(new)],
                ratios: vec![100, 100],
            };
        } else {
            let mut carrier = Some(new);
            self.root.split_leaf(n, kind, &mut carrier);
        }
    }

    /// Remove leaf `n`; single-child splits collapse. Removing the only
    /// remaining leaf is a no-op.
    pub fn remove_leaf(&mut self, n: usize) {
        self.root.remove_leaf(n);
    }

    /// Navigate to the Split node identified by `path` (child indices from
    /// the root; empty = root).
    pub fn node_at_path_mut(&mut self, path: &[usize]) -> Option<&mut Node<T>> {
        fn walk<'a, T>(node: &'a mut Node<T>, path: &[usize]) -> Option<&'a mut Node<T>> {
            if path.is_empty() {
                return Some(node);
            }
            if let Node::Split { children, .. } = node {
                children
                    .get_mut(path[0])
                    .and_then(|child| walk(child, &path[1..]))
            } else {
                None
            }
        }
        walk(&mut self.root, path)
    }

    /// Walk the tree and check if `(col, row)` lands on a separator.
    /// Returns the innermost separator hit, or `None`.
    pub fn hit_test_separator(&self, area: Rect, col: u16, row: u16) -> Option<SeparatorHit> {
        hit_test_node(&self.root, area, col, row)
    }
}

// ---------------------------------------------------------------------------
// PaneTreeView — rendering
// ---------------------------------------------------------------------------

/// Renders the tiling layout: 1-column separators with junction glyphs
/// between LeftRight children, junction fixups where TopBottom title bars
/// meet them, and — in multi-pane mode — each leaf's title bar. Leaf
/// *content* is drawn by the caller's closure, which receives the leaf's
/// depth-first index, the leaf, its inner area (below the title bar), and
/// whether it is focused.
pub struct PaneTreeView<'a, T> {
    pub focus_idx: usize,
    /// Title shown in a leaf's top border (multi-pane mode only).
    pub title_for: &'a dyn Fn(&T) -> String,
}

impl<T> PaneTreeView<'_, T> {
    pub fn render_with(
        self,
        tree: &mut SplitTree<T>,
        area: Rect,
        buf: &mut Buffer,
        mut draw_leaf: impl FnMut(usize, &mut T, Rect, &mut Buffer, bool),
    ) {
        let multi = tree.leaf_count() > 1;
        let mut next_idx = 0usize;
        render_node(
            &mut tree.root,
            area,
            buf,
            multi,
            self.focus_idx,
            &mut next_idx,
            self.title_for,
            &mut draw_leaf,
        );
    }
}

#[allow(clippy::too_many_arguments)] // internal recursion carrier
fn render_node<T>(
    node: &mut Node<T>,
    area: Rect,
    buf: &mut Buffer,
    multi: bool,
    focus_idx: usize,
    next_idx: &mut usize,
    title_for: &dyn Fn(&T) -> String,
    draw_leaf: &mut impl FnMut(usize, &mut T, Rect, &mut Buffer, bool),
) {
    match node {
        Node::Leaf(value) => {
            let idx = *next_idx;
            *next_idx += 1;
            let is_focus = idx == focus_idx;
            let inner = if multi {
                draw_title_bar(area, buf, &title_for(value), is_focus)
            } else {
                area
            };
            draw_leaf(idx, value, inner, buf, is_focus);
        }
        Node::Split {
            kind,
            children,
            ratios,
        } => {
            let areas = split_areas(area, *kind, ratios);
            match kind {
                Split::LeftRight => {
                    let sep_style = Style::default().fg(Color::DarkGray);
                    for pair in areas.windows(2) {
                        let x = pair[0].right();
                        if area.height > 0 {
                            let top_sym = if area.y > 0 {
                                buf[(x, area.y - 1)].symbol()
                            } else {
                                ""
                            };
                            let top_connects = matches!(top_sym, "│" | "┬" | "├" | "┤" | "┼");
                            let junction = if top_connects { '┼' } else { '┬' };
                            buf[(x, area.y)].set_char(junction).set_style(sep_style);
                        }
                        for y in (area.y + 1)..(area.y + area.height) {
                            buf[(x, y)].set_char('│').set_style(sep_style);
                        }
                    }
                    for (child, a) in children.iter_mut().zip(areas.iter()) {
                        render_node(
                            child, *a, buf, multi, focus_idx, next_idx, title_for, draw_leaf,
                        );
                    }
                }
                Split::TopBottom => {
                    for (i, (child, a)) in children.iter_mut().zip(areas.iter()).enumerate() {
                        render_node(
                            child, *a, buf, multi, focus_idx, next_idx, title_for, draw_leaf,
                        );
                        if i > 0 {
                            let ty = a.y;
                            let buf_right = buf.area().x + buf.area().width;
                            if a.x > 0 {
                                let lx = a.x - 1;
                                let s = buf[(lx, ty)].style();
                                match buf[(lx, ty)].symbol() {
                                    "│" => {
                                        buf[(lx, ty)].set_char('├').set_style(s);
                                    }
                                    "┬" => {
                                        buf[(lx, ty)].set_char('┼').set_style(s);
                                    }
                                    _ => {}
                                }
                            }
                            let rx = a.x + a.width;
                            if rx < buf_right {
                                let s = buf[(rx, ty)].style();
                                match buf[(rx, ty)].symbol() {
                                    "│" => {
                                        buf[(rx, ty)].set_char('┤').set_style(s);
                                    }
                                    "┬" => {
                                        buf[(rx, ty)].set_char('┼').set_style(s);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Draw a leaf's top-border title line and return the content area below it.
fn draw_title_bar(area: Rect, buf: &mut Buffer, title: &str, is_focus: bool) -> Rect {
    let border_style = Style::default().fg(Color::DarkGray);
    let name_style = if is_focus {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(border_style)
        .title(Line::from(vec![
            Span::styled("── ", border_style),
            Span::styled(title, name_style),
            Span::styled(" ", border_style),
        ]));
    let inner = block.inner(area);
    block.render(area, buf);
    // Post-process title-bar row: replace ─ with ┴ where a vertical separator
    // from a sibling above ends at this pane's top edge.
    if area.y > 0 {
        let ty = area.y;
        for x in area.x..area.x + area.width {
            if buf[(x, ty)].symbol() == "─" {
                let above = buf[(x, ty - 1)].symbol();
                if matches!(above, "│" | "┬" | "├" | "┤" | "┼") {
                    let s = buf[(x, ty)].style();
                    buf[(x, ty)].set_char('┴').set_style(s);
                }
            }
        }
    }
    inner
}

// ---------------------------------------------------------------------------
// Separator hit-testing
// ---------------------------------------------------------------------------

/// Identifies which separator was hit by a mouse click.
pub struct SeparatorHit {
    /// Path from root to the Split node, as child indices.
    pub path: Vec<usize>,
    /// Index of the separator: between `children[sep_idx]` and `children[sep_idx + 1]`.
    pub sep_idx: usize,
    /// True for LeftRight (drag horizontally), false for TopBottom (drag vertically).
    pub horizontal: bool,
    /// The screen area of the Split node that owns this separator.
    pub split_area: Rect,
}

fn hit_test_node<T>(node: &Node<T>, area: Rect, col: u16, row: u16) -> Option<SeparatorHit> {
    let Node::Split {
        kind,
        children,
        ratios,
    } = node
    else {
        return None;
    };
    let areas = split_areas(area, *kind, ratios);
    match kind {
        Split::LeftRight => {
            // Check explicit 1-cell separator columns first.
            for (i, pair) in areas.windows(2).enumerate() {
                let sep_x = pair[0].right();
                if col == sep_x && row >= area.y && row < area.y + area.height {
                    return Some(SeparatorHit {
                        path: vec![],
                        sep_idx: i,
                        horizontal: true,
                        split_area: area,
                    });
                }
            }
        }
        Split::TopBottom => {
            // The separator is the title-bar row of child[i] for i > 0.
            for (i, a) in areas.iter().enumerate().skip(1) {
                if row == a.y && col >= a.x && col < a.x + a.width {
                    return Some(SeparatorHit {
                        path: vec![],
                        sep_idx: i - 1,
                        horizontal: false,
                        split_area: area,
                    });
                }
            }
        }
    }
    // Not a separator at this level — recurse into the child that contains
    // the point and prepend our child index to the path.
    for (i, (child, a)) in children.iter().zip(areas.iter()).enumerate() {
        if col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height {
            if let Some(mut hit) = hit_test_node(child, *a, col, row) {
                hit.path.insert(0, i);
                return Some(hit);
            }
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// split_areas
// ---------------------------------------------------------------------------

pub fn split_areas(area: Rect, kind: Split, ratios: &[u16]) -> Vec<Rect> {
    if ratios.is_empty() {
        return vec![];
    }
    let direction = match kind {
        Split::LeftRight => Direction::Horizontal,
        Split::TopBottom => Direction::Vertical,
    };
    match kind {
        Split::LeftRight => {
            // Interleave Fill(ratio) pane constraints with Length(1) separator constraints so
            // a single-cell │ can be drawn between adjacent panes.
            let mut constraints = Vec::with_capacity(ratios.len() * 2 - 1);
            for (i, &r) in ratios.iter().enumerate() {
                if i > 0 {
                    constraints.push(Constraint::Length(1));
                }
                constraints.push(Constraint::Fill(r));
            }
            let all_areas = Layout::default()
                .direction(direction)
                .constraints(constraints)
                .split(area)
                .to_vec();
            // Return only the even-indexed areas (the pane areas, not the separators).
            all_areas.into_iter().step_by(2).collect()
        }
        Split::TopBottom => {
            // No spacing: each pane's own top-border title line acts as the visual
            // separator between stacked panes.
            let constraints: Vec<Constraint> =
                ratios.iter().map(|&r| Constraint::Fill(r)).collect();
            Layout::default()
                .direction(direction)
                .constraints(constraints)
                .split(area)
                .to_vec()
        }
    }
}

// ---------------------------------------------------------------------------
// Directional neighbor lookup
// ---------------------------------------------------------------------------

fn h_overlap(a: Rect, b: Rect) -> u16 {
    let start = a.x.max(b.x);
    let end = (a.x + a.width).min(b.x + b.width);
    end.saturating_sub(start)
}

fn v_overlap(a: Rect, b: Rect) -> u16 {
    let start = a.y.max(b.y);
    let end = (a.y + a.height).min(b.y + b.height);
    end.saturating_sub(start)
}

/// Given all leaf rects (from `SplitTree::leaf_areas`), the index of the
/// focused leaf, and a direction, return the index of the best spatial
/// neighbor, or `None`.
///
/// "Best" means: smallest gap along the primary axis, with largest perpendicular
/// overlap as a tie-breaker. Returns `None` if no candidate exists in that direction.
pub fn find_directional_neighbor(
    areas: &[Rect],
    focused_idx: usize,
    dir: FocusDir,
) -> Option<usize> {
    let src = areas.get(focused_idx)?;
    let mut best: Option<(i32, i32, usize)> = None; // (primary_gap, neg_overlap, idx)

    for (idx, cand) in areas.iter().enumerate() {
        if idx == focused_idx {
            continue;
        }
        let (primary_gap, perp_overlap) = match dir {
            FocusDir::Left => (
                src.x as i32 - (cand.x + cand.width) as i32,
                v_overlap(*src, *cand) as i32,
            ),
            FocusDir::Right => (
                cand.x as i32 - (src.x + src.width) as i32,
                v_overlap(*src, *cand) as i32,
            ),
            FocusDir::Up => (
                src.y as i32 - (cand.y + cand.height) as i32,
                h_overlap(*src, *cand) as i32,
            ),
            FocusDir::Down => (
                cand.y as i32 - (src.y + src.height) as i32,
                h_overlap(*src, *cand) as i32,
            ),
        };
        if primary_gap < 0 || perp_overlap <= 0 {
            continue;
        }
        let score = (primary_gap, -perp_overlap);
        match best {
            None => best = Some((score.0, score.1, idx)),
            Some((bg, bo, _)) if score < (bg, bo) => best = Some((score.0, score.1, idx)),
            _ => {}
        }
    }
    best.map(|(_, _, idx)| idx)
}

// ---------------------------------------------------------------------------
// Tests (generic — leaves are chars)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn r(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    fn hsplit() -> SplitTree<char> {
        SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![Node::Leaf('a'), Node::Leaf('b')],
                ratios: vec![100, 100],
            },
        }
    }

    fn vsplit_node(a: char, b: char) -> Node<char> {
        Node::Split {
            kind: Split::TopBottom,
            children: vec![Node::Leaf(a), Node::Leaf(b)],
            ratios: vec![100, 100],
        }
    }

    fn nested() -> SplitTree<char> {
        SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![Node::Leaf('a'), vsplit_node('b', 'c')],
                ratios: vec![100, 100],
            },
        }
    }

    // ---- structure ----------------------------------------------------------

    #[test]
    fn leaf_count_and_indexing() {
        let t = nested();
        assert_eq!(t.leaf_count(), 3);
        assert_eq!(t.leaf(0), Some(&'a'));
        assert_eq!(t.leaf(1), Some(&'b'));
        assert_eq!(t.leaf(2), Some(&'c'));
        assert_eq!(t.leaf(3), None);
    }

    #[test]
    fn single_leaf_tree() {
        let t = SplitTree::new('x');
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.leaf(0), Some(&'x'));
        assert_eq!(t.leaf(1), None);
    }

    #[test]
    fn leaf_mut_modifies() {
        let mut t = hsplit();
        *t.leaf_mut(1).unwrap() = 'z';
        assert_eq!(t.leaf(1), Some(&'z'));
    }

    #[test]
    fn leaves_mut_dfs_order() {
        let mut t = nested();
        let leaves: Vec<char> = t.leaves_mut().into_iter().map(|c| *c).collect();
        assert_eq!(leaves, vec!['a', 'b', 'c']);
    }

    // ---- split_leaf ----------------------------------------------------------

    #[test]
    fn split_root_leaf_wraps() {
        let mut t = SplitTree::new('a');
        t.split_leaf(0, Split::LeftRight, 'b');
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.leaf(0), Some(&'a'));
        assert_eq!(t.leaf(1), Some(&'b'));
    }

    #[test]
    fn split_leaf_increases_count() {
        let mut t = hsplit();
        t.split_leaf(0, Split::TopBottom, 'c');
        assert_eq!(t.leaf_count(), 3);
        assert_eq!(t.leaf(1), Some(&'c'));
    }

    #[test]
    fn split_leaf_nested() {
        let mut t = nested();
        t.split_leaf(2, Split::LeftRight, 'd');
        assert_eq!(t.leaf_count(), 4);
        assert_eq!(t.leaf(3), Some(&'d'));
    }

    // ---- remove_leaf ---------------------------------------------------------

    #[test]
    fn remove_leaf_collapses_to_single() {
        let mut t = hsplit();
        t.remove_leaf(0);
        assert_eq!(t.leaf_count(), 1);
        assert!(matches!(t.root, Node::Leaf('b')));
    }

    #[test]
    fn remove_leaf_nested() {
        let mut t = nested();
        t.remove_leaf(1);
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.leaf(1), Some(&'c'));
    }

    #[test]
    fn remove_last_leaf_is_noop() {
        let mut t = SplitTree::new('a');
        t.remove_leaf(0);
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_deep_nested() {
        let mut t = SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![
                    Node::Leaf('a'),
                    Node::Split {
                        kind: Split::TopBottom,
                        children: vec![
                            Node::Leaf('b'),
                            Node::Split {
                                kind: Split::LeftRight,
                                children: vec![Node::Leaf('c'), Node::Leaf('d')],
                                ratios: vec![100, 100],
                            },
                        ],
                        ratios: vec![100, 100],
                    },
                ],
                ratios: vec![100, 100],
            },
        };
        assert_eq!(t.leaf_count(), 4);
        t.remove_leaf(3);
        assert_eq!(t.leaf_count(), 3);
    }

    // ---- leaf_areas / split_areas ---------------------------------------------

    #[test]
    fn split_areas_horizontal_even() {
        let a = split_areas(r(100, 20), Split::LeftRight, &[100, 100]);
        assert_eq!(a[0], Rect::new(0, 0, 50, 20));
        assert_eq!(a[1], Rect::new(51, 0, 49, 20));
    }

    #[test]
    fn split_areas_vertical_even() {
        let a = split_areas(r(80, 40), Split::TopBottom, &[100, 100]);
        assert_eq!(a[0], Rect::new(0, 0, 80, 20));
        assert_eq!(a[1], Rect::new(0, 20, 80, 20));
    }

    #[test]
    fn split_areas_empty() {
        assert!(split_areas(r(80, 40), Split::LeftRight, &[]).is_empty());
    }

    #[test]
    fn split_areas_unequal_ratios() {
        let a = split_areas(r(90, 20), Split::LeftRight, &[200, 100]);
        assert!(a[0].width > a[1].width);
        assert_eq!(a[0].width + a[1].width, 89);
    }

    #[test]
    fn leaf_areas_count_matches_leaf_count() {
        let t = nested();
        assert_eq!(t.leaf_areas(r(120, 60)).len(), t.leaf_count());
    }

    #[test]
    fn leaf_areas_single_leaf_covers_all() {
        let t = SplitTree::new('x');
        assert_eq!(t.leaf_areas(r(100, 50)), vec![r(100, 50)]);
    }

    // ---- node_at_path_mut ------------------------------------------------------

    #[test]
    fn node_at_path_empty_is_root() {
        let mut t = hsplit();
        assert!(matches!(
            t.node_at_path_mut(&[]),
            Some(Node::Split {
                kind: Split::LeftRight,
                ..
            })
        ));
    }

    #[test]
    fn node_at_path_walks_children() {
        let mut t = nested();
        assert!(matches!(
            t.node_at_path_mut(&[1]),
            Some(Node::Split {
                kind: Split::TopBottom,
                ..
            })
        ));
        assert!(matches!(t.node_at_path_mut(&[0]), Some(Node::Leaf('a'))));
        assert!(t.node_at_path_mut(&[5]).is_none());
        assert!(t.node_at_path_mut(&[0, 0]).is_none());
    }

    #[test]
    fn node_at_path_mut_modifies_ratios() {
        let mut t = hsplit();
        if let Some(Node::Split { ratios, .. }) = t.node_at_path_mut(&[]) {
            ratios[0] = 200;
        }
        if let Node::Split { ratios, .. } = &t.root {
            assert_eq!(ratios[0], 200);
        }
    }

    // ---- hit_test_separator ------------------------------------------------------

    #[test]
    fn hit_test_lr_hits_separator() {
        let t = hsplit();
        let areas = split_areas(r(21, 10), Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        let hit = t.hit_test_separator(r(21, 10), sep_x, 5).unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(hit.horizontal);
        assert_eq!(hit.path, Vec::<usize>::new());
        assert!(t.hit_test_separator(r(21, 10), sep_x - 1, 5).is_none());
        assert!(t.hit_test_separator(r(21, 10), sep_x + 1, 5).is_none());
    }

    #[test]
    fn hit_test_tb_hits_separator() {
        let t = SplitTree {
            root: vsplit_node('a', 'b'),
        };
        let areas = split_areas(r(80, 20), Split::TopBottom, &[100, 100]);
        let sep_y = areas[1].y;
        let hit = t.hit_test_separator(r(80, 20), 40, sep_y).unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(!hit.horizontal);
        assert!(t.hit_test_separator(r(80, 20), 40, sep_y - 1).is_none());
    }

    #[test]
    fn hit_test_leaf_returns_none() {
        let t = SplitTree::new('a');
        assert!(t.hit_test_separator(r(80, 20), 10, 10).is_none());
    }

    #[test]
    fn hit_test_nested_prepends_path() {
        let t = nested();
        let outer_areas = split_areas(r(80, 20), Split::LeftRight, &[100, 100]);
        let right_area = outer_areas[1];
        let inner_areas = split_areas(right_area, Split::TopBottom, &[100, 100]);
        let sep_y = inner_areas[1].y;
        let hit = t
            .hit_test_separator(r(80, 20), right_area.x + 2, sep_y)
            .unwrap();
        assert_eq!(hit.path, vec![1]);
        assert!(!hit.horizontal);
        assert_eq!(hit.sep_idx, 0);
        assert_eq!(hit.split_area, right_area);
    }

    #[test]
    fn hit_test_lr_three_panes_second_separator() {
        let t = SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![Node::Leaf('a'), Node::Leaf('b'), Node::Leaf('c')],
                ratios: vec![100, 100, 100],
            },
        };
        let area = r(62, 10);
        let areas = split_areas(area, Split::LeftRight, &[100, 100, 100]);
        let sep1_x = areas[1].right();
        let hit = t.hit_test_separator(area, sep1_x, 5).unwrap();
        assert_eq!(hit.sep_idx, 1);
    }

    // ---- find_directional_neighbor -----------------------------------------

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn directional_nav_lr() {
        let areas = vec![rect(0, 0, 49, 20), rect(50, 0, 50, 20)];
        assert_eq!(
            find_directional_neighbor(&areas, 0, FocusDir::Right),
            Some(1)
        );
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Right), None);
        assert_eq!(
            find_directional_neighbor(&areas, 1, FocusDir::Left),
            Some(0)
        );
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Left), None);
    }

    #[test]
    fn directional_nav_tb() {
        let areas = vec![rect(0, 0, 100, 10), rect(0, 10, 100, 10)];
        assert_eq!(
            find_directional_neighbor(&areas, 0, FocusDir::Down),
            Some(1)
        );
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Up), Some(0));
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Up), None);
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Down), None);
    }

    #[test]
    fn directional_nav_single_pane_returns_none() {
        let areas = vec![rect(0, 0, 100, 40)];
        for dir in [
            FocusDir::Left,
            FocusDir::Right,
            FocusDir::Up,
            FocusDir::Down,
        ] {
            assert_eq!(find_directional_neighbor(&areas, 0, dir), None);
        }
    }

    #[test]
    fn directional_nav_l_shape_from_top_right_down() {
        let areas = vec![
            rect(0, 0, 49, 40),
            rect(50, 0, 50, 20),
            rect(50, 20, 50, 20),
        ];
        assert_eq!(
            find_directional_neighbor(&areas, 1, FocusDir::Down),
            Some(2)
        );
        assert_eq!(
            find_directional_neighbor(&areas, 1, FocusDir::Left),
            Some(0)
        );
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Up), None);
    }

    #[test]
    fn directional_nav_diagonal_no_overlap_not_reachable() {
        let areas = vec![rect(0, 0, 50, 20), rect(51, 21, 50, 20)];
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Right), None);
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Down), None);
    }

    #[test]
    fn directional_nav_picks_closest_of_two_candidates() {
        let areas = vec![rect(0, 0, 49, 20), rect(50, 0, 20, 20), rect(60, 0, 20, 20)];
        assert_eq!(
            find_directional_neighbor(&areas, 0, FocusDir::Right),
            Some(1)
        );
    }

    // ---- junction rendering (title bars drawn by the widget) ----------------

    fn render_junctions(tree: &mut SplitTree<char>, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let title_for = |_: &char| "t".to_string();
        PaneTreeView {
            focus_idx: 0,
            title_for: &title_for,
        }
        .render_with(tree, area, &mut buf, |_, _, _, _, _| {});
        buf
    }

    #[test]
    fn junction_lr_separator_is_tee_down_at_top() {
        let mut tree = hsplit();
        let buf = render_junctions(&mut tree, r(21, 5));
        assert_eq!(buf[(10u16, 0u16)].symbol(), "┬");
        for y in 1..5u16 {
            assert_eq!(buf[(10u16, y)].symbol(), "│");
        }
    }

    #[test]
    fn junction_title_bar_below_lr_separator_gets_tee_up() {
        let mut tree = SplitTree {
            root: Node::Split {
                kind: Split::TopBottom,
                children: vec![
                    Node::Split {
                        kind: Split::LeftRight,
                        children: vec![Node::Leaf('a'), Node::Leaf('b')],
                        ratios: vec![100, 100],
                    },
                    Node::Leaf('c'),
                ],
                ratios: vec![100, 100],
            },
        };
        let buf = render_junctions(&mut tree, r(41, 10));
        assert_eq!(buf[(20u16, 0u16)].symbol(), "┬");
        for y in 1..5u16 {
            assert_eq!(buf[(20u16, y)].symbol(), "│");
        }
        assert_eq!(buf[(20u16, 5u16)].symbol(), "┴");
    }

    #[test]
    fn junction_lr_separator_left_of_inner_tb_becomes_tee_right() {
        let mut tree = SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![Node::Leaf('a'), vsplit_node('b', 'c')],
                ratios: vec![100, 100],
            },
        };
        let buf = render_junctions(&mut tree, r(21, 10));
        assert_eq!(buf[(10u16, 5u16)].symbol(), "├");
    }

    #[test]
    fn junction_lr_separator_right_of_inner_tb_becomes_tee_left() {
        let mut tree = SplitTree {
            root: Node::Split {
                kind: Split::LeftRight,
                children: vec![vsplit_node('a', 'b'), Node::Leaf('c')],
                ratios: vec![100, 100],
            },
        };
        let buf = render_junctions(&mut tree, r(21, 10));
        assert_eq!(buf[(10u16, 5u16)].symbol(), "┤");
    }

    #[test]
    fn junction_stacked_lr_separators_at_same_column_produce_cross() {
        let mut tree = SplitTree {
            root: Node::Split {
                kind: Split::TopBottom,
                children: vec![
                    Node::Split {
                        kind: Split::LeftRight,
                        children: vec![Node::Leaf('a'), Node::Leaf('b')],
                        ratios: vec![100, 100],
                    },
                    Node::Split {
                        kind: Split::LeftRight,
                        children: vec![Node::Leaf('c'), Node::Leaf('d')],
                        ratios: vec![100, 100],
                    },
                ],
                ratios: vec![100, 100],
            },
        };
        let buf = render_junctions(&mut tree, r(41, 10));
        assert_eq!(buf[(20u16, 5u16)].symbol(), "┼");
    }

    #[test]
    fn single_leaf_gets_no_title_bar_and_full_area() {
        let mut tree = SplitTree::new('a');
        let mut seen = None;
        let title_for = |_: &char| "t".to_string();
        let area = r(20, 6);
        let mut buf = Buffer::empty(area);
        PaneTreeView {
            focus_idx: 0,
            title_for: &title_for,
        }
        .render_with(&mut tree, area, &mut buf, |idx, _, inner, _, focus| {
            seen = Some((idx, inner, focus));
        });
        assert_eq!(seen, Some((0, area, true)));
    }

    #[test]
    fn leaves_receive_inner_below_title_and_focus_flag() {
        let mut tree = hsplit();
        let mut calls: Vec<(usize, Rect, bool)> = Vec::new();
        let title_for = |_: &char| "t".to_string();
        let area = r(21, 6);
        let mut buf = Buffer::empty(area);
        PaneTreeView {
            focus_idx: 1,
            title_for: &title_for,
        }
        .render_with(&mut tree, area, &mut buf, |idx, _, inner, _, focus| {
            calls.push((idx, inner, focus));
        });
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, 0);
        assert!(!calls[0].2);
        assert!(calls[1].2);
        // Inner areas start one row below the pane top (title bar).
        assert_eq!(calls[0].1.y, 1);
        assert_eq!(calls[1].1.y, 1);
    }
}
