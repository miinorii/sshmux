//! Tiling pane tree component: the generic layout structure (`tree`) and
//! its renderer (`view`).

mod tree;
mod view;

pub use tree::{
    FocusDir, Node, SeparatorHit, Split, SplitTree, find_directional_neighbor, split_areas,
};
pub use view::PaneTreeView;
