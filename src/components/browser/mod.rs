//! File browser component (SFTP and SCP): shared state-machine core
//! (`state`, with behaviour spread over `navigation`, `selection`, `delete`,
//! `transfer`, `keys`, and `mouse` as `impl BrowserCore` blocks), the two
//! protocol drivers (`sftp`, `ssh`), output parsing (`parse`), and the
//! dual-panel renderer (`view`).

use ratatui::layout::Rect;

mod delete;
mod keys;
mod mouse;
mod navigation;
mod selection;
mod state;
mod transfer;
mod view;

pub mod parse;
pub mod sftp;
pub mod ssh;

pub use keys::handle_browser_key;
pub use sftp::FileBrowser;
pub use ssh::{SshBrowser, SshBrowserState};
pub use state::*;
pub use view::{FileBrowserView, StatusKind};

/// Layout areas for the dual-pane browser UI.
pub struct BrowserLayout {
    pub local_panel: Rect,
    pub remote_panel: Rect,
    pub status: Rect,
}

/// Compute the dual-pane + status bar layout from a content area.
pub fn browser_layout(inner: Rect) -> BrowserLayout {
    let status_h = 1u16;
    let panels_area = Rect {
        height: inner.height.saturating_sub(status_h),
        ..inner
    };
    let status = Rect {
        y: inner.y + inner.height.saturating_sub(status_h),
        height: status_h,
        ..inner
    };
    let half = (panels_area.width as f32 / 2.0).round() as u16;
    let local_panel = Rect {
        width: half,
        ..panels_area
    };
    let remote_panel = Rect {
        x: panels_area.x + half,
        width: panels_area.width - half,
        ..panels_area
    };
    BrowserLayout {
        local_panel,
        remote_panel,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_splits_evenly() {
        let inner = Rect::new(0, 0, 100, 40);
        let layout = browser_layout(inner);
        assert_eq!(layout.local_panel.x, 0);
        assert_eq!(layout.local_panel.width, 50);
        assert_eq!(layout.remote_panel.x, 50);
        assert_eq!(layout.remote_panel.width, 50);
    }

    #[test]
    fn layout_odd_width_gives_extra_to_local() {
        let inner = Rect::new(0, 0, 101, 40);
        let layout = browser_layout(inner);
        // round(101/2) = 51 → local gets the extra pixel
        assert_eq!(layout.local_panel.width, 51);
        assert_eq!(layout.remote_panel.width, 50);
    }

    #[test]
    fn layout_status_bar_one_row() {
        let inner = Rect::new(0, 0, 80, 30);
        let layout = browser_layout(inner);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.status.y, 29);
        assert_eq!(layout.status.width, 80);
    }

    #[test]
    fn layout_panels_exclude_status() {
        let inner = Rect::new(0, 0, 80, 30);
        let layout = browser_layout(inner);
        assert_eq!(layout.local_panel.height, 29);
        assert_eq!(layout.remote_panel.height, 29);
    }

    #[test]
    fn layout_offset_area() {
        let inner = Rect::new(5, 3, 80, 30);
        let layout = browser_layout(inner);
        assert_eq!(layout.local_panel.x, 5);
        assert_eq!(layout.remote_panel.x, 45);
        assert_eq!(layout.status.x, 5);
        assert_eq!(layout.status.y, 32);
    }

    #[test]
    fn layout_zero_height_saturates() {
        let inner = Rect::new(0, 0, 80, 0);
        let layout = browser_layout(inner);
        assert_eq!(layout.local_panel.height, 0);
        // Status bar always claims 1 row; at zero height, it still has height 1
        // but sits at y=0 (overlapping). This is fine since ratatui handles the
        // zero-area case gracefully.
        assert_eq!(layout.status.height, 1);
    }

    #[test]
    fn layout_height_one() {
        let inner = Rect::new(0, 0, 80, 1);
        let layout = browser_layout(inner);
        // panels_area height = 1 - 1 = 0, status takes the last row
        assert_eq!(layout.local_panel.height, 0);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.status.y, 0);
    }
}
