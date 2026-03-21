use ratatui::layout::Rect;

pub mod parse;
pub mod sftp;
pub mod ssh;

pub use sftp::{BrowserFocus, FileBrowser, SftpState};
pub use ssh::{SshBrowser, SshBrowserState};

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
    let half = panels_area.width / 2;
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
