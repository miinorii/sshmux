//! Renderer for the tiling split tree: separators, junction glyphs, and
//! per-leaf title bars. Leaf *content* is drawn by the caller's closure.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use super::tree::{Node, Split, SplitTree, split_areas};

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
