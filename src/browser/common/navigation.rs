//! Focus toggling, horizontal scroll, directory navigation, drive picker,
//! remote-path mutation, command timer, and paste/drag-drop deadline handling.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use log::{debug, info};
use ratatui::{style::Color, widgets::ListState};

use super::super::parse::{list_drives, read_local_dir};
use super::{BrowserCore, BrowserFocus, PROMPT_STABLE_TICKS};

impl BrowserCore {
    pub fn toggle_focus(&mut self) {
        self.dismiss_drive_picker();
        self.clear_selection();
        self.focus = match self.focus {
            BrowserFocus::Local => BrowserFocus::Remote,
            BrowserFocus::Remote => BrowserFocus::Local,
        };
    }

    pub fn scroll_left(&mut self) {
        let changed = match self.focus {
            BrowserFocus::Local => self.local.scroll_left(),
            BrowserFocus::Remote => self.remote.scroll_left(),
        };
        if changed {
            self.needs_redraw = true;
        }
    }

    pub fn scroll_right(&mut self) {
        match self.focus {
            BrowserFocus::Local => self.local.scroll_right(),
            BrowserFocus::Remote => self.remote.scroll_right(),
        }
        self.needs_redraw = true;
    }

    pub fn nav_up(&mut self) {
        if let Some((_, sel)) = &mut self.drive_picker {
            sel.select_previous();
            self.needs_redraw = true;
            return;
        }
        match self.focus {
            BrowserFocus::Local => self.local.sel.select_previous(),
            BrowserFocus::Remote => self.remote.sel.select_previous(),
        }
    }

    pub fn nav_down(&mut self) {
        if let Some((_, sel)) = &mut self.drive_picker {
            sel.select_next();
            self.needs_redraw = true;
            return;
        }
        match self.focus {
            BrowserFocus::Local => self.local.sel.select_next(),
            BrowserFocus::Remote => self.remote.sel.select_next(),
        }
    }

    pub fn dismiss_drive_picker(&mut self) {
        if self.drive_picker.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// Handle Enter on the local panel (drive picker or directory navigation).
    pub fn local_enter(&mut self) {
        if self.drive_picker.is_some() {
            if let Some((drives, sel)) = self.drive_picker.take()
                && let Some(i) = sel.selected()
                && let Some(drive) = drives.get(i).cloned()
            {
                self.local.path = drive;
                self.local.entries = read_local_dir(&self.local.path);
                self.local.sel.select_first();
            }
            self.needs_redraw = true;
            return;
        }

        if let Some(i) = self.local.sel.selected() {
            let Some(entry) = self.local.entries.get(i).cloned() else {
                return;
            };
            if entry.name == ".." {
                if let Some(p) = self.local.path.parent() {
                    self.local.path = p.to_path_buf();
                } else {
                    self.show_drive_picker();
                    return;
                }
            } else if entry.is_dir {
                self.local.path.push(&entry.name);
            } else {
                return;
            }
            self.local.entries = read_local_dir(&self.local.path);
            self.local.sel.select_first();
            self.status_msg = format!("Local: {}", self.local.path.to_string_lossy());
            self.status_color = Color::Green;
            self.last_duration = None;
            self.needs_redraw = true;
        }
    }

    /// Handle Backspace on the local panel.
    pub fn local_go_up(&mut self) {
        if self.drive_picker.is_some() {
            self.dismiss_drive_picker();
            return;
        }
        if let Some(p) = self.local.path.parent() {
            self.local.path = p.to_path_buf();
            self.local.entries = read_local_dir(&self.local.path);
            self.local.sel.select_first();
            self.status_msg = format!("Local: {}", self.local.path.to_string_lossy());
            self.status_color = Color::Green;
            self.last_duration = None;
            self.needs_redraw = true;
        } else {
            self.show_drive_picker();
        }
    }

    fn show_drive_picker(&mut self) {
        let drives = list_drives();
        let mut drive_sel = ListState::default();
        drive_sel.select_first();
        self.drive_picker = Some((drives, drive_sel));
        self.needs_redraw = true;
    }

    pub fn apply_cd(&mut self, name: &str) {
        if name == ".." {
            if let Some(pos) = self.remote.path.rfind('/') {
                self.remote.path = if pos == 0 {
                    "/".to_string()
                } else {
                    self.remote.path[..pos].to_string()
                };
            }
        } else {
            let base = self.remote.path.trim_end_matches('/');
            self.remote.path = format!("{}/{}", base, name);
        }
    }

    /// Update prompt-stability tracking for a tick. Returns true once the
    /// raw PTY byte count has been unchanged AND the expected prompt was
    /// detected for `PROMPT_STABLE_TICKS` consecutive ticks.
    pub fn update_prompt_stability(&mut self, cur_len: usize, has_prompt: bool) -> bool {
        if cur_len != self.prev_raw_len {
            self.prompt_stable = 0;
            self.prev_raw_len = cur_len;
        } else if has_prompt {
            self.prompt_stable = self.prompt_stable.saturating_add(1);
        } else {
            self.prompt_stable = 0;
        }
        self.prompt_stable >= PROMPT_STABLE_TICKS
    }

    pub fn stop_timer(&mut self) {
        if let Some(start) = self.cmd_start.take() {
            self.last_duration = Some(start.elapsed());
        }
    }

    pub fn format_duration(d: Duration) -> String {
        let ms = d.as_millis();
        if ms < 1000 {
            format!("{}ms", ms)
        } else {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}m", secs / 60)
            } else {
                format!("{}h", secs / 3600)
            }
        }
    }

    /// Called each tick. If the paste deadline has expired, parse the buffer
    /// for valid file paths and populate `drop_confirm`.
    pub fn check_paste_deadline(&mut self) {
        let expired = self
            .paste_deadline
            .map(|d| Instant::now() >= d)
            .unwrap_or(false);
        if !expired {
            return;
        }
        let text = std::mem::take(&mut self.paste_buf);
        self.paste_deadline = None;
        debug!(
            "paste deadline expired: {} chars, text={:?}",
            text.len(),
            &text[..text.len().min(200)]
        );

        let parse_start = Instant::now();
        let paths = parse_dropped_paths(&text);
        let parse_elapsed = parse_start.elapsed();
        debug!(
            "parse_dropped_paths took {:?}, found {} path(s)",
            parse_elapsed,
            paths.len()
        );

        if paths.is_empty() {
            self.status_msg.clear();
            self.needs_redraw = true;
            return;
        }
        info!(
            "drag-drop detected: {} file(s): {:?}",
            paths.len(),
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );
        let count = paths.len();
        self.drop_confirm = Some(paths);
        self.drop_scroll_x = 0;
        self.drop_scroll_y = 0;
        self.status_msg = format!("{} file(s) ready to upload", count);
        self.status_color = Color::Cyan;
        self.needs_redraw = true;
    }
}

// ---------------------------------------------------------------------------
// Path parsing for drag-and-drop detection
// ---------------------------------------------------------------------------

/// Parse file paths from text pasted by the OS (drag-and-drop).
/// Handles quoted paths (for names with spaces) and multiple paths.
/// Only returns paths that actually exist on disk.
fn parse_dropped_paths(text: &str) -> Vec<PathBuf> {
    let text = text.trim();
    let mut paths = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let token = if rest.starts_with('"') {
            let inner = &rest[1..];
            if let Some(end) = inner.find('"') {
                let tok = &inner[..end];
                rest = &inner[end + 1..];
                tok
            } else {
                rest = "";
                inner
            }
        } else if let Some(split_pos) = find_path_boundary(rest) {
            let tok = &rest[..split_pos];
            rest = &rest[split_pos..];
            tok.trim_end()
        } else {
            let tok = rest;
            rest = "";
            tok
        };

        let path = std::path::Path::new(token);
        if path.exists() {
            paths.push(path.to_path_buf());
        }
    }

    paths
}

/// Find where one unquoted path ends and the next begins.
/// Looks for ` X:\` or ` /` boundaries that signal a new absolute path.
fn find_path_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b' ' && i + 1 < bytes.len() {
            let after = &s[i + 1..];
            if (after.len() >= 3 && after.as_bytes()[1] == b':' && after.as_bytes()[2] == b'\\')
                || after.starts_with('/')
            {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::{BrowserCore, BrowserFocus, core_with_remote_entries};
    use ratatui::widgets::ListState;
    use std::time::Duration;

    #[test]
    fn nav_down_advances_remote_selection() {
        let mut core =
            core_with_remote_entries(&[("..", true), ("dir1", true), ("file.txt", false)]);
        core.focus = BrowserFocus::Remote;
        core.remote.sel.select(Some(0));
        core.nav_down();
        assert_eq!(core.remote.sel.selected(), Some(1));
    }

    #[test]
    fn nav_up_retreats_remote_selection() {
        let mut core =
            core_with_remote_entries(&[("..", true), ("dir1", true), ("file.txt", false)]);
        core.focus = BrowserFocus::Remote;
        core.remote.sel.select(Some(2));
        core.nav_up();
        assert_eq!(core.remote.sel.selected(), Some(1));
    }

    #[test]
    fn toggle_focus_switches_local_to_remote() {
        let mut core = BrowserCore::new("host");
        assert_eq!(core.focus, BrowserFocus::Local);
        core.toggle_focus();
        assert_eq!(core.focus, BrowserFocus::Remote);
    }

    #[test]
    fn toggle_focus_switches_remote_to_local() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        core.toggle_focus();
        assert_eq!(core.focus, BrowserFocus::Local);
    }

    #[test]
    fn toggle_focus_dismisses_drive_picker() {
        let mut core = BrowserCore::new("host");
        core.drive_picker = Some((vec![], ListState::default()));
        core.toggle_focus();
        assert!(core.drive_picker.is_none());
    }

    #[test]
    fn scroll_right_increments_by_four() {
        let mut core = BrowserCore::new("host");
        core.scroll_right();
        assert_eq!(core.local.scroll_x, 4);
        assert!(core.needs_redraw);
    }

    #[test]
    fn scroll_left_decrements_by_four() {
        let mut core = BrowserCore::new("host");
        core.local.scroll_x = 8;
        core.scroll_left();
        assert_eq!(core.local.scroll_x, 4);
    }

    #[test]
    fn scroll_left_saturates_at_zero() {
        let mut core = BrowserCore::new("host");
        core.local.scroll_x = 2;
        core.scroll_left();
        assert_eq!(core.local.scroll_x, 0);
    }

    #[test]
    fn scroll_left_noop_when_zero() {
        let mut core = BrowserCore::new("host");
        core.needs_redraw = false;
        core.scroll_left();
        assert!(!core.needs_redraw);
    }

    #[test]
    fn scroll_affects_remote_when_focused() {
        let mut core = BrowserCore::new("host");
        core.focus = BrowserFocus::Remote;
        core.scroll_right();
        assert_eq!(core.remote.scroll_x, 4);
        assert_eq!(core.local.scroll_x, 0);
    }

    #[test]
    fn apply_cd_subdir() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home/user".to_string();
        core.apply_cd("docs");
        assert_eq!(core.remote.path, "/home/user/docs");
    }

    #[test]
    fn apply_cd_parent() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home/user/docs".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote.path, "/home/user");
    }

    #[test]
    fn apply_cd_parent_at_root() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote.path, "/");
    }

    #[test]
    fn apply_cd_parent_from_top_level_dir() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home".to_string();
        core.apply_cd("..");
        assert_eq!(core.remote.path, "/");
    }

    #[test]
    fn apply_cd_no_double_slash() {
        let mut core = BrowserCore::new("host");
        core.remote.path = "/home/user/".to_string();
        core.apply_cd("docs");
        assert_eq!(core.remote.path, "/home/user/docs");
    }

    #[test]
    fn format_duration_millis() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(42)),
            "42ms"
        );
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(BrowserCore::format_duration(Duration::from_secs(5)), "5s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(BrowserCore::format_duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_secs(7200)),
            "2h"
        );
    }

    #[test]
    fn format_duration_boundary_999ms() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(999)),
            "999ms"
        );
    }

    #[test]
    fn format_duration_boundary_1000ms() {
        assert_eq!(
            BrowserCore::format_duration(Duration::from_millis(1000)),
            "1s"
        );
    }

    #[test]
    fn stop_timer_records_duration() {
        let mut core = BrowserCore::new("host");
        core.cmd_start = Some(std::time::Instant::now());
        std::thread::sleep(Duration::from_millis(5));
        core.stop_timer();
        assert!(core.cmd_start.is_none());
        assert!(core.last_duration.is_some());
    }

    #[test]
    fn stop_timer_noop_without_start() {
        let mut core = BrowserCore::new("host");
        core.stop_timer();
        assert!(core.last_duration.is_none());
    }

    #[test]
    fn dismiss_drive_picker_when_active() {
        let mut core = BrowserCore::new("host");
        core.drive_picker = Some((vec![], ListState::default()));
        core.needs_redraw = false;
        core.dismiss_drive_picker();
        assert!(core.drive_picker.is_none());
        assert!(core.needs_redraw);
    }

    #[test]
    fn dismiss_drive_picker_noop_when_none() {
        let mut core = BrowserCore::new("host");
        core.needs_redraw = false;
        core.dismiss_drive_picker();
        assert!(!core.needs_redraw);
    }

    #[test]
    fn update_prompt_stability_resets_on_byte_change() {
        let mut core = BrowserCore::new("host");
        core.prompt_stable = 5;
        core.prev_raw_len = 10;
        assert!(!core.update_prompt_stability(20, true));
        assert_eq!(core.prompt_stable, 0);
        assert_eq!(core.prev_raw_len, 20);
    }

    #[test]
    fn update_prompt_stability_counts_up_when_stable_with_prompt() {
        let mut core = BrowserCore::new("host");
        core.prev_raw_len = 10;
        assert!(!core.update_prompt_stability(10, true));
        assert_eq!(core.prompt_stable, 1);
        assert!(core.update_prompt_stability(10, true));
        assert_eq!(core.prompt_stable, 2);
    }

    #[test]
    fn update_prompt_stability_resets_when_stable_without_prompt() {
        let mut core = BrowserCore::new("host");
        core.prev_raw_len = 10;
        core.prompt_stable = 1;
        assert!(!core.update_prompt_stability(10, false));
        assert_eq!(core.prompt_stable, 0);
    }
}
