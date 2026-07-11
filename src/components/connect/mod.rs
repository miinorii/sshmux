//! Connect pane component: host-list and overlay state (`state`) and its
//! renderer (`view`).

mod state;
mod view;

pub use state::{
    ConnectOverlay, ConnectPane, EDITOR_ROW_COUNT, HEADER_BROWSER, HEADER_CONNECT, HEADER_GLOBAL,
    InputField, KeyEditorState, editor_binding_index, editor_nav_down, editor_nav_up,
    is_editor_header,
};
pub use view::ConnectView;
