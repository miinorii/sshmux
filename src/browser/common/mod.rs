//! Shared types, state, and utilities for the SFTP and SCP browsers.
//!
//! This module is split across several files for navigability; all items
//! are re-exported at this module root so external callers can continue to
//! use paths like `browser::common::BrowserCore`.
//!
//! Submodules:
//! - [`navigation`] — focus, scroll, directory navigation, timer, paste detection
//! - [`selection`] — multi-select state (indices, anchor, update)
//! - [`delete`] — local/remote delete flows and confirmation
//! - [`transfer`] — pending-transfer queue
//! - [`mouse`] — click, drag, and release handling
//! - [`render`] — all panel/overlay/status rendering
//! - [`keys`] — the `handle_browser_key` dispatch

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::{layout::Rect, style::Color, widgets::ListState};

use super::parse::{FsEntry, read_local_dir};

// ---------------------------------------------------------------------------
// Panel — shared state for local and remote file listings.
// ---------------------------------------------------------------------------

/// One side of the dual-pane browser (local or remote). `P` is the path type
/// (`PathBuf` for local, `String` for remote).
pub struct Panel<P> {
    pub path: P,
    pub entries: Vec<FsEntry>,
    pub sel: ListState,
    pub scroll_x: usize,
}

impl<P> Panel<P> {
    /// Return the index of the currently-highlighted row.
    pub fn focused_index(&self) -> Option<usize> {
        self.sel.selected()
    }

    /// Move the cursor up one row.
    pub fn nav_up(&mut self) {
        self.sel.select_previous();
    }

    /// Move the cursor down one row.
    pub fn nav_down(&mut self) {
        self.sel.select_next();
    }

    /// Scroll the view left by 4 columns. Returns true if the scroll changed.
    pub fn scroll_left(&mut self) -> bool {
        if self.scroll_x > 0 {
            self.scroll_x = self.scroll_x.saturating_sub(4);
            true
        } else {
            false
        }
    }

    /// Scroll the view right by 4 columns.
    pub fn scroll_right(&mut self) {
        self.scroll_x += 4;
    }
}

// ---------------------------------------------------------------------------
// TransferState — active and pending transfers.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct TransferState {
    pub last: Option<TransferStatus>,
    pub pending: Vec<PendingTransfer>,
    pub start: Option<Instant>,
    pub batch_total: usize,
    pub batch_done: usize,
}

// ---------------------------------------------------------------------------
// DeleteState — confirmation and pending-delete queue.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct DeleteState {
    pub confirm: Option<DeleteTarget>,
    pub pending: Vec<DeleteTarget>,
    pub pending_name: Option<String>,
}

mod delete;
mod keys;
mod mouse;
mod navigation;
mod render;
mod selection;
mod transfer;

pub use keys::handle_browser_key;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserFocus {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Clone)]
pub struct TransferStatus {
    pub filename: String,
    pub direction: TransferDirection,
    pub is_dir: bool,
    pub done: bool,
    pub progress: u8, // 0–100
    pub file_count: usize,
}

#[derive(Clone, Debug)]
pub struct PendingTransfer {
    pub path: String, // full source path (local for upload, remote for download)
    pub name: String, // filename component
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteLocation {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteKind {
    File,
    Dir,
}

#[derive(Clone, Debug)]
pub struct DeleteTarget {
    pub location: DeleteLocation,
    pub kind: DeleteKind,
    pub path: String,
}

impl DeleteTarget {
    pub fn is_dir(&self) -> bool {
        self.kind == DeleteKind::Dir
    }

    pub fn display_side(&self) -> &'static str {
        match self.location {
            DeleteLocation::Local => "local",
            DeleteLocation::Remote => "remote",
        }
    }
}

/// Bytes to scan from the end of raw PTY output for prompt detection.
pub const PROMPT_TAIL_BYTES: usize = 64;

/// Seconds before a waiting command (ls, pwd, delete) is considered timed out.
pub const COMMAND_TIMEOUT_SECS: u64 = 30;

/// Consecutive stable ticks required before a prompt is considered ready.
pub const PROMPT_STABLE_TICKS: u8 = 2;

/// Action returned by `handle_browser_key` for browser-specific operations.
pub enum BrowserKeyAction {
    Handled,
    Enter,
    GoUp,
    Download,
    Upload,
    Delete,
    ConfirmDeleteYes,
}

/// Result of a drag-release gesture across panels.
pub enum DragAction {
    LocalToRemote,
    RemoteToLocal,
}

/// State tracked during a left-button drag gesture in a browser pane.
pub struct DragState {
    pub origin: BrowserFocus,
    pub label: String,
    pub mouse_col: u16,
    pub mouse_row: u16,
}

/// Shared interface for browser panes (SFTP and SCP).
pub trait Browser {
    fn core(&self) -> &BrowserCore;
    fn core_mut(&mut self) -> &mut BrowserCore;
    fn upload(&mut self);
    fn download(&mut self);
    fn enter(&mut self);
    fn go_up(&mut self);
    fn delete_focused(&mut self);
    fn confirm_delete_yes(&mut self);
    /// True when the browser is in a connecting/auth state that should forward raw keys.
    fn is_connecting(&self) -> bool;
    /// Forward a key during the connecting phase.
    fn send_connect_key(&mut self, code: KeyCode);
    /// True when the underlying PTY process has exited.
    fn process_exited(&self) -> bool;

    /// Chain the next queued action: start a pending transfer if any, otherwise
    /// trigger a pending delete. Called at the end of a state arm that returns
    /// control to `Idle`. Returns true if an action was chained.
    fn chain_next_queued(&mut self) -> bool {
        if !self.core().transfer.pending.is_empty() {
            match self.core().last_transfer_direction() {
                TransferDirection::Upload => self.upload(),
                TransferDirection::Download => self.download(),
            }
            true
        } else if self.core_mut().pop_pending_delete() {
            self.confirm_delete_yes();
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// BrowserCore — shared state for both SFTP and SCP browsers
// ---------------------------------------------------------------------------

pub struct BrowserCore {
    pub host: String,

    pub local: Panel<PathBuf>,
    pub remote: Panel<String>,

    pub focus: BrowserFocus,
    pub status_msg: String,
    pub raw_snapshot: Vec<String>,
    pub needs_redraw: bool,
    pub drive_picker: Option<(Vec<PathBuf>, ListState)>,
    pub status_color: Color,
    pub cmd_start: Option<Instant>,
    pub last_duration: Option<Duration>,
    pub prompt_stable: u8,
    pub prev_raw_len: usize,
    // ---- multi-select ----
    pub selected: BTreeSet<usize>,
    pub select_anchor: Option<usize>,
    // ---- transfer and delete state ----
    pub transfer: TransferState,
    pub delete: DeleteState,
    // ---- drag-and-drop paste detection ----
    pub paste_buf: String,
    pub paste_deadline: Option<Instant>,
    pub drop_confirm: Option<Vec<PathBuf>>,
    pub drop_scroll_x: usize,
    pub drop_scroll_y: usize,
    pub last_inner: Rect,
    // ---- drag visual feedback ----
    pub drag: Option<DragState>,
    // ---- exit overlay ----
    pub exit_selection: u8, // 0 = Reconnect, 1 = Close pane
}

impl BrowserCore {
    pub fn new(host: &str) -> Self {
        let local_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local_entries = read_local_dir(&local_path);
        let mut local_sel = ListState::default();
        local_sel.select_first();
        let mut remote_sel = ListState::default();
        remote_sel.select_first();

        BrowserCore {
            host: host.to_string(),
            local: Panel {
                path: local_path,
                entries: local_entries,
                sel: local_sel,
                scroll_x: 0,
            },
            remote: Panel {
                path: String::from("."),
                entries: vec![],
                sel: remote_sel,
                scroll_x: 0,
            },
            focus: BrowserFocus::Local,
            status_msg: String::from("Connecting…"),
            raw_snapshot: vec![],
            needs_redraw: false,
            drive_picker: None,
            status_color: Color::Yellow,
            cmd_start: None,
            last_duration: None,
            prompt_stable: 0,
            prev_raw_len: 0,
            selected: BTreeSet::new(),
            select_anchor: None,
            transfer: TransferState::default(),
            delete: DeleteState::default(),
            paste_buf: String::new(),
            paste_deadline: None,
            drop_confirm: None,
            drop_scroll_x: 0,
            drop_scroll_y: 0,
            last_inner: Rect::default(),
            drag: None,
            exit_selection: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared test helpers, visible to submodule test modules.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(super) fn dummy_entry(name: &str, is_dir: bool) -> FsEntry {
    FsEntry {
        name: name.to_string(),
        is_dir,
        size: "0".to_string(),
        perms: "drwxr-xr-x".to_string(),
        modified: "2025-01-01".to_string(),
    }
}

#[cfg(test)]
pub(super) fn core_with_remote_entries(names: &[(&str, bool)]) -> BrowserCore {
    let mut core = BrowserCore::new("test-host");
    core.remote.entries = names.iter().map(|(n, d)| dummy_entry(n, *d)).collect();
    core.remote.sel.select(Some(0));
    core
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_core_defaults() {
        let core = BrowserCore::new("myhost");
        assert_eq!(core.host, "myhost");
        assert_eq!(core.remote.path, ".");
        assert!(core.remote.entries.is_empty());
        assert_eq!(core.focus, BrowserFocus::Local);
        assert!(core.delete.confirm.is_none());
        assert!(core.drive_picker.is_none());
        assert_eq!(core.local.scroll_x, 0);
        assert_eq!(core.remote.scroll_x, 0);
        assert_eq!(core.prompt_stable, 0);
        assert_eq!(core.status_msg, "Connecting…");
    }

    #[test]
    fn delete_target_display_side() {
        let local = DeleteTarget {
            location: DeleteLocation::Local,
            kind: DeleteKind::File,
            path: "/x".to_string(),
        };
        let remote = DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::Dir,
            path: "/y".to_string(),
        };
        assert_eq!(local.display_side(), "local");
        assert_eq!(remote.display_side(), "remote");
        assert!(!local.is_dir());
        assert!(remote.is_dir());
    }
}
