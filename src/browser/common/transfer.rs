//! Pending-transfer queue and last-direction accessor.

use log::info;

use super::{BrowserCore, BrowserFocus, PendingTransfer, TransferDirection};

impl BrowserCore {
    /// Direction of the queued transfers. Every path that fills the queue
    /// records a direction; as a safety net fall back to the in-flight
    /// transfer's direction, then Upload.
    pub fn pending_direction(&self) -> TransferDirection {
        self.transfer
            .pending_direction
            .or_else(|| self.transfer.last.as_ref().map(|t| t.direction))
            .unwrap_or(TransferDirection::Upload)
    }

    /// Convert selected indices to transfer entries and store in `pending_transfers`.
    /// Records the direction implied by the focused panel.
    pub fn queue_transfers_from_indices(&mut self, indices: &[usize]) {
        self.transfer.pending_direction = Some(match self.focus {
            BrowserFocus::Local => TransferDirection::Upload,
            BrowserFocus::Remote => TransferDirection::Download,
        });
        let entries = match self.focus {
            BrowserFocus::Local => &self.local.entries,
            BrowserFocus::Remote => &self.remote.entries,
        };
        self.transfer.pending = indices
            .iter()
            .filter_map(|&i| entries.get(i))
            .filter(|e| e.name != "..")
            .map(|e| {
                let path = match self.focus {
                    BrowserFocus::Local => self
                        .local
                        .path
                        .join(&e.name)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    BrowserFocus::Remote => {
                        format!("{}/{}", self.remote.path.trim_end_matches('/'), e.name)
                    }
                };
                PendingTransfer {
                    path,
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                }
            })
            .collect();
        info!(
            "queued {} transfers: {:?}",
            self.transfer.pending.len(),
            self.transfer.pending
        );
    }

    /// Pop the next pending transfer from the queue.
    pub fn pop_pending(&mut self) -> Option<PendingTransfer> {
        if self.transfer.pending.is_empty() {
            None
        } else {
            Some(self.transfer.pending.remove(0))
        }
    }
}
