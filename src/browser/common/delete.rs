//! Local/remote delete flows, confirmation state, and the pending-delete queue.

use std::path::PathBuf;

use log::{info, warn};
use ratatui::style::Color;

use super::super::parse::read_local_dir;
use super::{BrowserCore, DeleteKind, DeleteLocation, DeleteTarget};

impl BrowserCore {
    pub fn local_delete_focused(&mut self) {
        if let Some(i) = self.local.sel.selected() {
            let Some(entry) = self.local.entries.get(i).cloned() else {
                return;
            };
            if entry.name == ".." {
                return;
            }
            let full_path = self.local.path.join(&entry.name);
            self.delete.confirm = Some(DeleteTarget {
                location: DeleteLocation::Local,
                kind: if entry.is_dir {
                    DeleteKind::Dir
                } else {
                    DeleteKind::File
                },
                path: full_path.to_string_lossy().into_owned(),
            });
            self.needs_redraw = true;
        }
    }

    /// Execute a confirmed local delete. Returns true if handled.
    /// A single confirmation deletes the entire local batch (no async round
    /// trip needed), matching the remote UX where one `y` covers the selection.
    pub fn local_confirm_delete(&mut self) -> bool {
        loop {
            let Some(ref target) = self.delete.confirm else {
                return false;
            };
            if target.location != DeleteLocation::Local {
                return false;
            }
            let path = PathBuf::from(&target.path);
            let result = if target.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(e) = result {
                warn!("local delete failed: {:?}: {}", path, e);
                self.status_msg = format!("Delete failed: {}", e);
                self.status_color = Color::Red;
                self.delete.pending.clear();
            } else {
                info!("local delete ok: {}", target.path);
                self.status_msg = format!("Deleted: {}", target.path);
                self.status_color = Color::Green;
                self.local.entries = read_local_dir(&self.local.path);
            }
            self.delete.confirm = None;
            self.last_duration = None;
            if !self.pop_pending_delete() {
                break;
            }
        }
        self.needs_redraw = true;
        true
    }

    /// Build delete targets for local multi-select deletion.
    /// Queues all selected items and shows confirmation for the first one.
    pub fn local_delete_selected(&mut self) {
        let indices = self.selected_indices();
        if indices.len() <= 1 {
            self.clear_selection();
            self.local_delete_focused();
            return;
        }
        let mut targets: Vec<DeleteTarget> = Vec::new();
        for &i in &indices {
            let Some(entry) = self.local.entries.get(i) else {
                continue;
            };
            if entry.name == ".." {
                continue;
            }
            let full_path = self.local.path.join(&entry.name);
            targets.push(DeleteTarget {
                location: DeleteLocation::Local,
                kind: if entry.is_dir {
                    DeleteKind::Dir
                } else {
                    DeleteKind::File
                },
                path: full_path.to_string_lossy().into_owned(),
            });
        }
        self.clear_selection();
        if let Some(first) = targets.first().cloned() {
            self.delete.pending = targets[1..].to_vec();
            self.delete.confirm = Some(first);
            self.needs_redraw = true;
        }
    }

    /// Pop the next pending delete and set it as confirm_delete.
    /// Returns true if there was a next item to delete.
    pub fn pop_pending_delete(&mut self) -> bool {
        if self.delete.pending.is_empty() {
            false
        } else {
            self.delete.confirm = Some(self.delete.pending.remove(0));
            true
        }
    }

    pub fn confirm_delete_no(&mut self) {
        self.delete.confirm = None;
        self.delete.pending.clear();
        self.status_msg = String::from("Deletion cancelled.");
        self.status_color = Color::Yellow;
        self.needs_redraw = true;
    }

    /// Build delete targets for remote multi-select or single-item deletion.
    /// Sets `confirm_delete` for the first item and queues the rest in `pending_deletes`.
    pub fn remote_delete_focused(&mut self) {
        let indices = self.selected_indices();
        if indices.len() > 1 {
            let mut targets: Vec<DeleteTarget> = Vec::new();
            for &i in &indices {
                let Some(entry) = self.remote.entries.get(i) else {
                    continue;
                };
                if entry.name == ".." {
                    continue;
                }
                let full_path =
                    format!("{}/{}", self.remote.path.trim_end_matches('/'), entry.name);
                targets.push(DeleteTarget {
                    location: DeleteLocation::Remote,
                    kind: if entry.is_dir {
                        DeleteKind::Dir
                    } else {
                        DeleteKind::File
                    },
                    path: full_path,
                });
            }
            self.clear_selection();
            if let Some(first) = targets.first().cloned() {
                self.delete.pending = targets[1..].to_vec();
                self.delete.confirm = Some(first);
                self.needs_redraw = true;
            }
        } else {
            self.clear_selection();
            if let Some(i) = self.remote.sel.selected() {
                let Some(entry) = self.remote.entries.get(i).cloned() else {
                    return;
                };
                if entry.name == ".." {
                    return;
                }
                let full_path =
                    format!("{}/{}", self.remote.path.trim_end_matches('/'), entry.name);
                self.delete.confirm = Some(DeleteTarget {
                    location: DeleteLocation::Remote,
                    kind: if entry.is_dir {
                        DeleteKind::Dir
                    } else {
                        DeleteKind::File
                    },
                    path: full_path,
                });
                self.needs_redraw = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BrowserCore, DeleteKind, DeleteLocation, DeleteTarget, dummy_entry};
    use ratatui::style::Color;

    #[test]
    fn confirm_delete_no_clears_state() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "/tmp/test.txt".to_string(),
        });
        core.confirm_delete_no();
        assert!(core.delete.confirm.is_none());
        assert_eq!(core.status_msg, "Deletion cancelled.");
        assert_eq!(core.status_color, Color::Yellow);
        assert!(core.needs_redraw);
    }

    #[test]
    fn local_delete_focused_sets_confirm() {
        let mut core = BrowserCore::new("host");
        core.local.entries = vec![dummy_entry("..", true), dummy_entry("myfile.txt", false)];
        core.local.sel.select(Some(1));
        core.local_delete_focused();
        assert!(core.delete.confirm.is_some());
        let target = core.delete.confirm.unwrap();
        assert_eq!(target.location, DeleteLocation::Local);
        assert_eq!(target.kind, DeleteKind::File);
        assert!(target.path.contains("myfile.txt"));
    }

    #[test]
    fn local_delete_focused_skips_dotdot() {
        let mut core = BrowserCore::new("host");
        core.local.entries = vec![dummy_entry("..", true)];
        core.local.sel.select(Some(0));
        core.local_delete_focused();
        assert!(core.delete.confirm.is_none());
    }

    #[test]
    fn local_delete_focused_dir_sets_dir_kind() {
        let mut core = BrowserCore::new("host");
        core.local.entries = vec![dummy_entry("..", true), dummy_entry("subdir", true)];
        core.local.sel.select(Some(1));
        core.local_delete_focused();
        let target = core.delete.confirm.unwrap();
        assert_eq!(target.location, DeleteLocation::Local);
        assert_eq!(target.kind, DeleteKind::Dir);
    }

    #[test]
    fn remote_delete_focused_single() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home/user".to_string();
        core.remote.entries = vec![dummy_entry("..", true), dummy_entry("file.txt", false)];
        core.remote.sel.select(Some(1));
        core.remote_delete_focused();
        let target = core.delete.confirm.unwrap();
        assert_eq!(target.location, DeleteLocation::Remote);
        assert_eq!(target.kind, DeleteKind::File);
        assert_eq!(target.path, "/home/user/file.txt");
    }

    #[test]
    fn remote_delete_focused_skips_dotdot() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home".to_string();
        core.remote.entries = vec![dummy_entry("..", true)];
        core.remote.sel.select(Some(0));
        core.remote_delete_focused();
        assert!(core.delete.confirm.is_none());
    }

    #[test]
    fn pop_pending_delete_cycles_targets() {
        let mut core = BrowserCore::new("host");
        core.delete.pending = vec![
            DeleteTarget {
                location: DeleteLocation::Remote,
                kind: DeleteKind::File,
                path: "/a".to_string(),
            },
            DeleteTarget {
                location: DeleteLocation::Remote,
                kind: DeleteKind::Dir,
                path: "/b".to_string(),
            },
        ];
        assert!(core.pop_pending_delete());
        assert_eq!(core.delete.confirm.as_ref().unwrap().path, "/a");
        assert_eq!(core.delete.pending.len(), 1);
        assert!(core.pop_pending_delete());
        assert_eq!(core.delete.confirm.as_ref().unwrap().path, "/b");
        assert!(core.delete.pending.is_empty());
        assert!(!core.pop_pending_delete());
    }
}
