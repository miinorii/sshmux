use ratatui::layout::Rect;

use super::{FocusDir, Pane, Split, split_areas};

// ---------------------------------------------------------------------------
// Separator hit-testing and resize support
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

/// Walk the pane tree and check if `(col, row)` lands on a separator.
/// Returns the innermost separator hit, or `None`.
pub fn hit_test_separator(pane: &Pane, area: Rect, col: u16, row: u16) -> Option<SeparatorHit> {
    match pane {
        Pane::Split {
            kind,
            children,
            ratios,
        } => {
            let areas = split_areas(area, kind, ratios);
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
                    if let Some(mut hit) = hit_test_separator(child, *a, col, row) {
                        hit.path.insert(0, i);
                        return Some(hit);
                    }
                    break;
                }
            }
            None
        }
        _ => None,
    }
}

/// Navigate the pane tree to the Split node identified by `path`.
pub fn split_at_path_mut<'a>(pane: &'a mut Pane, path: &[usize]) -> Option<&'a mut Pane> {
    if path.is_empty() {
        return Some(pane);
    }
    if let Pane::Split { children, .. } = pane {
        children
            .get_mut(path[0])
            .and_then(|child| split_at_path_mut(child, &path[1..]))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// remove_leaf
// ---------------------------------------------------------------------------

pub fn remove_leaf(pane: &mut Pane, n: usize) {
    match pane {
        Pane::Connect(_)
        | Pane::Session { .. }
        | Pane::FileBrowser { .. }
        | Pane::SshBrowser { .. } => {}
        Pane::Split {
            children, ratios, ..
        } => {
            let mut offset = 0;
            let mut to_remove = None;
            for (i, child) in children.iter_mut().enumerate() {
                let count = child.leaf_count();
                if n < offset + count {
                    if count == 1 {
                        to_remove = Some(i);
                    } else {
                        remove_leaf(child, n - offset);
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
    }
    // Collapse a Split that is down to a single child into that child directly.
    if let Pane::Split { children, .. } = pane
        && children.len() == 1
    {
        *pane = children.remove(0);
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

/// Given all leaf rects (from `Pane::leaf_areas`), the index of the focused leaf,
/// and a direction, return the index of the best spatial neighbor, or `None`.
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }
    fn connect() -> Pane {
        Pane::new_connect()
    }
    fn hsplit() -> Pane {
        Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        }
    }
    fn vsplit() -> Pane {
        Pane::Split {
            kind: Split::TopBottom,
            children: vec![connect(), connect()],
            ratios: vec![100, 100],
        }
    }

    // ---- remove_leaf -------------------------------------------------------

    #[test]
    fn remove_leaf_first() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        assert_eq!(p.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_second() {
        let mut p = hsplit();
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        remove_leaf(&mut p, 1);
        assert_eq!(p.leaf_count(), 2);
    }

    #[test]
    fn remove_leaf_noop_on_single() {
        let mut p = connect();
        remove_leaf(&mut p, 0);
        assert_eq!(p.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_collapses_to_single() {
        let mut p = hsplit();
        remove_leaf(&mut p, 0);
        assert!(matches!(p, Pane::Connect(_)));
    }

    #[test]
    fn remove_leaf_deep_nested() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![
                        connect(),
                        Pane::Split {
                            kind: Split::LeftRight,
                            children: vec![connect(), connect()],
                            ratios: vec![100, 100],
                        },
                    ],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        assert_eq!(p.leaf_count(), 4);
        remove_leaf(&mut p, 3);
        assert_eq!(p.leaf_count(), 3);
    }

    // ---- hit_test_separator -------------------------------------------------

    #[test]
    fn hit_test_lr_hits_separator() {
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        let hit = hit_test_separator(&p, r(21, 10), sep_x, 5);
        assert!(hit.is_some(), "should hit LR separator at x={sep_x}");
        let hit = hit.unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(hit.horizontal);
        assert_eq!(hit.path, vec![]);
    }

    #[test]
    fn hit_test_lr_misses_left_of_separator() {
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        assert!(hit_test_separator(&p, r(21, 10), sep_x - 1, 5).is_none());
    }

    #[test]
    fn hit_test_lr_misses_right_of_separator() {
        let p = hsplit();
        let areas = split_areas(r(21, 10), &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        assert!(hit_test_separator(&p, r(21, 10), sep_x + 1, 5).is_none());
    }

    #[test]
    fn hit_test_tb_hits_separator() {
        let p = vsplit();
        let areas = split_areas(r(80, 20), &Split::TopBottom, &[100, 100]);
        let sep_y = areas[1].y;
        let hit = hit_test_separator(&p, r(80, 20), 40, sep_y);
        assert!(hit.is_some(), "should hit TB separator at y={sep_y}");
        let hit = hit.unwrap();
        assert_eq!(hit.sep_idx, 0);
        assert!(!hit.horizontal);
        assert_eq!(hit.path, vec![]);
    }

    #[test]
    fn hit_test_tb_misses_above_separator() {
        let p = vsplit();
        let areas = split_areas(r(80, 20), &Split::TopBottom, &[100, 100]);
        let sep_y = areas[1].y;
        assert!(hit_test_separator(&p, r(80, 20), 40, sep_y - 1).is_none());
    }

    #[test]
    fn hit_test_leaf_returns_none() {
        assert!(hit_test_separator(&connect(), r(80, 20), 10, 10).is_none());
    }

    #[test]
    fn hit_test_lr_three_panes_second_separator() {
        let p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), connect(), connect()],
            ratios: vec![100, 100, 100],
        };
        let area = r(62, 10);
        let areas = split_areas(area, &Split::LeftRight, &[100, 100, 100]);
        let sep1_x = areas[1].right();
        let hit = hit_test_separator(&p, area, sep1_x, 5);
        assert!(hit.is_some(), "should hit second LR separator");
        assert_eq!(hit.unwrap().sep_idx, 1);
    }

    #[test]
    fn hit_test_nested_prepends_path() {
        let inner_area_r = r(80, 20);
        let outer = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![connect(), connect()],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        let outer_areas = split_areas(inner_area_r, &Split::LeftRight, &[100, 100]);
        let right_area = outer_areas[1];
        let inner_areas = split_areas(right_area, &Split::TopBottom, &[100, 100]);
        let sep_y = inner_areas[1].y;
        let hit = hit_test_separator(&outer, inner_area_r, right_area.x + 2, sep_y);
        assert!(hit.is_some(), "should hit nested TB separator");
        let hit = hit.unwrap();
        assert_eq!(hit.path, vec![1], "path should point to child[1]");
        assert!(!hit.horizontal);
        assert_eq!(hit.sep_idx, 0);
    }

    #[test]
    fn hit_test_split_area_is_containing_node_area() {
        let p = hsplit();
        let area = r(21, 10);
        let areas = split_areas(area, &Split::LeftRight, &[100, 100]);
        let sep_x = areas[0].right();
        let hit = hit_test_separator(&p, area, sep_x, 5).unwrap();
        assert_eq!(hit.split_area, area);
    }

    // ---- split_at_path_mut --------------------------------------------------

    #[test]
    fn split_at_path_empty_returns_root() {
        let mut p = hsplit();
        let result = split_at_path_mut(&mut p, &[]);
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap(),
            Pane::Split {
                kind: Split::LeftRight,
                ..
            }
        ));
    }

    #[test]
    fn split_at_path_single_step_child0() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![vsplit(), connect()],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[0]);
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap(),
            Pane::Split {
                kind: Split::TopBottom,
                ..
            }
        ));
    }

    #[test]
    fn split_at_path_single_step_child1() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![connect(), vsplit()],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[1]);
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap(),
            Pane::Split {
                kind: Split::TopBottom,
                ..
            }
        ));
    }

    #[test]
    fn split_at_path_nested_two_levels() {
        let mut p = Pane::Split {
            kind: Split::LeftRight,
            children: vec![
                connect(),
                Pane::Split {
                    kind: Split::TopBottom,
                    children: vec![
                        connect(),
                        Pane::Split {
                            kind: Split::LeftRight,
                            children: vec![connect(), connect()],
                            ratios: vec![100, 100],
                        },
                    ],
                    ratios: vec![100, 100],
                },
            ],
            ratios: vec![100, 100],
        };
        let result = split_at_path_mut(&mut p, &[1, 1]);
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap(),
            Pane::Split {
                kind: Split::LeftRight,
                ..
            }
        ));
    }

    #[test]
    fn split_at_path_out_of_bounds_returns_none() {
        let mut p = hsplit();
        assert!(split_at_path_mut(&mut p, &[5]).is_none());
    }

    #[test]
    fn split_at_path_into_leaf_returns_none() {
        let mut p = hsplit();
        assert!(split_at_path_mut(&mut p, &[0, 0]).is_none());
    }

    #[test]
    fn split_at_path_mut_modifies_ratios() {
        let mut p = hsplit();
        if let Some(Pane::Split { ratios, .. }) = split_at_path_mut(&mut p, &[]) {
            ratios[0] = 200;
        }
        if let Pane::Split { ratios, .. } = &p {
            assert_eq!(
                ratios[0], 200,
                "mutation through split_at_path_mut should be visible"
            );
        }
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
    fn directional_nav_lr_right() {
        let areas = vec![rect(0, 0, 49, 20), rect(50, 0, 50, 20)];
        assert_eq!(
            find_directional_neighbor(&areas, 0, FocusDir::Right),
            Some(1)
        );
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Right), None);
    }

    #[test]
    fn directional_nav_lr_left() {
        let areas = vec![rect(0, 0, 49, 20), rect(50, 0, 50, 20)];
        assert_eq!(
            find_directional_neighbor(&areas, 1, FocusDir::Left),
            Some(0)
        );
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Left), None);
    }

    #[test]
    fn directional_nav_tb_down() {
        let areas = vec![rect(0, 0, 100, 10), rect(0, 10, 100, 10)];
        assert_eq!(
            find_directional_neighbor(&areas, 0, FocusDir::Down),
            Some(1)
        );
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Down), None);
    }

    #[test]
    fn directional_nav_tb_up() {
        let areas = vec![rect(0, 0, 100, 10), rect(0, 10, 100, 10)];
        assert_eq!(find_directional_neighbor(&areas, 1, FocusDir::Up), Some(0));
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Up), None);
    }

    #[test]
    fn directional_nav_single_pane_returns_none() {
        let areas = vec![rect(0, 0, 100, 40)];
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Left), None);
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Right), None);
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Up), None);
        assert_eq!(find_directional_neighbor(&areas, 0, FocusDir::Down), None);
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
}
