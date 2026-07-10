//! Dual-panel file browser rendering (SFTP and SCP panes).

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, StatefulWidget, Widget},
};

use crate::browser::browser_layout;
use crate::browser::common::{BrowserCore, BrowserFocus, TransferDirection};
use crate::keybindings::BrowserBindings;

impl BrowserCore {
    /// Render both panels. Returns the status bar area.
    fn render_panels(&mut self, area: Rect, buf: &mut Buffer, is_focus: bool) -> Rect {
        let layout = browser_layout(area);
        self.render_panel(layout.local_panel, buf, BrowserFocus::Local, is_focus);
        self.render_panel(layout.remote_panel, buf, BrowserFocus::Remote, is_focus);
        layout.status
    }

    fn render_panel(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        side: BrowserFocus,
        pane_focused: bool,
    ) {
        let is_active = self.focus == side && pane_focused;

        let title = match side {
            BrowserFocus::Local if self.drive_picker.is_some() => " select drive ",
            BrowserFocus::Local => " local ",
            BrowserFocus::Remote => " remote ",
        };
        let path_str = match side {
            BrowserFocus::Local => self.local.path.to_string_lossy().to_string(),
            BrowserFocus::Remote => self.remote.path.clone(),
        };

        let border_col = if is_active {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_col))
            .title_top(Line::from(Span::styled(
                format!(" {} ", path_str),
                Style::default().fg(Color::DarkGray),
            )))
            .title_top(
                Line::from(Span::styled(
                    format!(" {} ", title),
                    Style::default().fg(Color::Yellow),
                ))
                .right_aligned(),
            );
        let inner = block.inner(area);
        block.render(area, buf);

        // Drive picker: shown in local panel instead of the normal file list.
        if side == BrowserFocus::Local
            && let Some((drives, drive_sel)) = &mut self.drive_picker
        {
            let items: Vec<String> = drives
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let list = List::new(items).highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            StatefulWidget::render(list, inner, buf, drive_sel);
            return;
        }

        let (entries, list_state) = match side {
            BrowserFocus::Local => (&self.local.entries, &mut self.local.sel),
            BrowserFocus::Remote => (&self.remote.entries, &mut self.remote.sel),
        };

        let only_dotdot =
            entries.len() <= 1 && entries.first().map(|e| e.name == "..").unwrap_or(true);
        if side == BrowserFocus::Remote && only_dotdot && !self.raw_snapshot.is_empty() {
            let visible: Vec<&String> = self
                .raw_snapshot
                .iter()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let start = visible.len().saturating_sub(inner.height as usize);
            for (i, line) in visible[start..].iter().enumerate() {
                let y = inner.y + i as u16;
                if y >= inner.y + inner.height {
                    break;
                }
                let span = Span::styled(
                    line.chars().take(inner.width as usize).collect::<String>(),
                    Style::default().fg(Color::DarkGray),
                );
                buf.set_span(inner.x, y, &span, inner.width);
            }
            return;
        }

        let w = inner.width as usize;
        let meta_width: usize = 9 + 1 + 16 + 1 + 10;

        let max_name_len = entries
            .iter()
            .map(|e| {
                if e.is_dir {
                    e.name.len() + 1
                } else {
                    e.name.len()
                }
            })
            .max()
            .unwrap_or(0);

        let virtual_width = (max_name_len + 1 + meta_width).max(w);

        let scroll_x = match side {
            BrowserFocus::Local => &mut self.local.scroll_x,
            BrowserFocus::Remote => &mut self.remote.scroll_x,
        };
        let max_scroll = virtual_width.saturating_sub(w);
        if *scroll_x > max_scroll {
            *scroll_x = max_scroll;
        }
        let sx = *scroll_x;

        let is_sel_panel = self.focus == side;
        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(idx, e)| {
                let selected = is_sel_panel && self.selected.contains(&idx);
                let name_col = if e.is_dir { Color::Cyan } else { Color::White };
                let display_name = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };

                let meta = format!("{:>9} {:<16} {:<10}", e.size, e.modified, e.perms);
                let name_len = display_name.chars().count();
                let gap = virtual_width - meta_width - name_len;
                let full = format!("{}{:gap$}{}", display_name, "", meta, gap = gap);

                let scrolled: String = full.chars().skip(sx).take(w).collect();
                let padded = format!("{:<width$}", scrolled, width = w);

                let visible_name_chars = if sx < name_len {
                    (name_len - sx).min(w)
                } else {
                    0
                };

                let sel_style = Style::default()
                    .fg(Color::Black)
                    .bg(if is_active {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })
                    .add_modifier(Modifier::BOLD);
                if visible_name_chars == 0 {
                    let style = if selected {
                        sel_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    ListItem::new(Line::from(Span::styled(padded, style)))
                } else {
                    let name_part: String = padded.chars().take(visible_name_chars).collect();
                    let rest: String = padded.chars().skip(visible_name_chars).collect();
                    if selected {
                        ListItem::new(Line::from(vec![
                            Span::styled(name_part, sel_style),
                            Span::styled(rest, sel_style),
                        ]))
                    } else {
                        ListItem::new(Line::from(vec![
                            Span::styled(name_part, Style::default().fg(name_col)),
                            Span::styled(rest, Style::default().fg(Color::DarkGray)),
                        ]))
                    }
                }
            })
            .collect();

        let list = List::new(items).highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(if is_active {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        );
        StatefulWidget::render(list, inner, buf, list_state);
    }

    /// Render delete confirmation bar. Returns true if rendered.
    fn render_confirm_delete(&self, area: Rect, buf: &mut Buffer) -> bool {
        let Some(ref target) = self.delete.confirm else {
            return false;
        };
        let side = target.display_side();
        let name = &target.path;
        let remaining = self.delete.pending.len();
        let msg = if remaining > 0 {
            format!(
                "  Delete {} '{}' (+{} more)?  [y] Yes   [n] No",
                side, name, remaining
            )
        } else {
            format!("  Delete {} '{}'?  [y] Yes   [n] No", side, name)
        };
        let span = Span::styled(
            msg,
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_span(area.x, area.y, &span, area.width);
        true
    }

    /// Render the upload confirmation overlay when files are pending.
    /// Returns true if the overlay was rendered (callers can skip status bar).
    fn render_upload_confirm(&mut self, inner: Rect, buf: &mut Buffer) -> bool {
        let Some(paths) = self.drop_confirm.as_ref() else {
            return false;
        };

        let count = paths.len();
        let scroll_x = self.drop_scroll_x;
        let scroll_y = self.drop_scroll_y;

        self.last_inner = inner;
        let box_w = 60u16.min(inner.width.saturating_sub(4));
        // Fixed lines: title + blank + indicator/blank + hints = 4, borders = 2
        let max_file_rows = 5.min((inner.height as usize).saturating_sub(6));
        let visible_files: Vec<_> = paths
            .iter()
            .skip(scroll_y)
            .take(max_file_rows.max(1))
            .collect();

        // Build file list lines
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!(" Upload {} file(s)? ", count),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for path in &visible_files {
            let full = format!("  {}", path.display());
            let char_count = full.chars().count();
            let display = if scroll_x > 0 && scroll_x < char_count {
                let rest: String = full.chars().skip(scroll_x + 1).collect();
                format!("…{}", rest)
            } else {
                full
            };
            lines.push(
                Line::from(Span::styled(display, Style::default().fg(Color::Cyan)))
                    .alignment(Alignment::Left),
            );
        }
        // Show scroll indicator if not all files are visible
        if count > visible_files.len() {
            let indicator = format!(
                "  [{}-{} of {}] ↑↓ to scroll",
                scroll_y + 1,
                scroll_y + visible_files.len(),
                count
            );
            lines.push(Line::from(Span::styled(
                indicator,
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(""));
        }

        let hint_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled("[y]", hint_style),
            Span::styled(" Upload  ", dim),
            Span::styled("[n]", hint_style),
            Span::styled(" Cancel  ", dim),
            Span::styled("[←→]", hint_style),
            Span::styled(" Scroll", dim),
        ]));

        let box_h = (lines.len() as u16 + 2).min(inner.height);
        let cx = inner.x + inner.width.saturating_sub(box_w) / 2;
        let cy = inner.y + inner.height.saturating_sub(box_h) / 2;
        let overlay = Rect {
            x: cx,
            y: cy,
            width: box_w,
            height: box_h,
        };

        // Clear the area
        for y in overlay.y..overlay.y + overlay.height {
            for x in overlay.x..overlay.x + overlay.width {
                if x < buf.area().width && y < buf.area().height {
                    buf[(x, y)].reset();
                }
            }
        }

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(Color::Black))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                    .title(" Drop Upload ")
                    .title_alignment(Alignment::Center),
            );
        paragraph.render(overlay, buf);
        true
    }

    /// Render the transfer progress overlay during an active transfer.
    /// `transferring` must be true (driven by the concrete browser state) for the overlay to show.
    /// Returns `true` if the overlay was drawn.
    fn render_transfer_progress(&self, inner: Rect, buf: &mut Buffer, transferring: bool) -> bool {
        let t = match self.transfer.last.as_ref() {
            Some(t) if transferring => t,
            _ => return false,
        };

        let elapsed = self.transfer.start.map(|s| s.elapsed()).unwrap_or_default();
        let elapsed_str = Self::format_duration(elapsed);

        let direction_arrow = match t.direction {
            TransferDirection::Download => "↓",
            TransferDirection::Upload => "↑",
        };

        // Gauge ratio: combine batch progress with per-file progress.
        // For dirs, file_pct stays 0 so the gauge advances step-wise between files.
        // When done, the file is already counted in batch_done — don't double-count.
        let file_pct = if t.done || t.is_dir {
            0.0
        } else {
            t.progress as f64
        };
        let ratio = if self.transfer.batch_total == 0 {
            file_pct / 100.0
        } else {
            (self.transfer.batch_done as f64 + file_pct / 100.0) / self.transfer.batch_total as f64
        }
        .clamp(0.0, 1.0);

        let show_batch = self.transfer.batch_total > 1;
        let content_rows: u16 = if show_batch { 3 } else { 2 };
        let box_w = 56u16.min(inner.width.saturating_sub(4));
        let box_h = content_rows + 2; // content + 2 borders

        let cx = inner.x + inner.width.saturating_sub(box_w) / 2;
        let cy = inner.y + inner.height.saturating_sub(box_h) / 2;
        let overlay = Rect {
            x: cx,
            y: cy,
            width: box_w,
            height: box_h,
        };

        // Clear area
        for y in overlay.y..overlay.y + overlay.height {
            for x in overlay.x..overlay.x + overlay.width {
                if x < buf.area().width && y < buf.area().height {
                    buf[(x, y)].reset();
                }
            }
        }

        let border_style = Style::default().fg(Color::Cyan).bg(Color::Black);
        let inner_area = Rect {
            x: overlay.x + 1,
            y: overlay.y + 1,
            width: overlay.width.saturating_sub(2),
            height: overlay.height.saturating_sub(2),
        };

        // Draw border box
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(" Transfer ")
            .title_alignment(Alignment::Center)
            .style(Style::default().bg(Color::Black))
            .render(overlay, buf);

        // Row 0: direction + filename (left) + elapsed (right)
        let fname_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let elapsed_str_padded = format!(" {}", elapsed_str);
        let available = inner_area.width as usize;
        let elapsed_len = elapsed_str_padded.chars().count();
        let name_prefix = format!("{}  {} ", direction_arrow, t.filename);
        let name_max = available.saturating_sub(elapsed_len);
        let name_display = if name_prefix.chars().count() > name_max {
            let truncated: String = name_prefix
                .chars()
                .take(name_max.saturating_sub(1))
                .collect();
            format!("{}…", truncated)
        } else {
            let pad = name_max - name_prefix.chars().count();
            format!("{}{}", name_prefix, " ".repeat(pad))
        };
        let row0 = Line::from(vec![
            Span::styled(name_display, fname_style),
            Span::styled(elapsed_str_padded, dim),
        ]);
        buf.set_line(inner_area.x, inner_area.y, &row0, inner_area.width);

        // Row 1: Gauge — label reflects the combined batch ratio, not the raw per-file %
        let gauge_label = if t.is_dir {
            String::new()
        } else {
            format!(" {}% ", (ratio * 100.0) as u8)
        };
        let gauge_area = Rect {
            x: inner_area.x,
            y: inner_area.y + 1,
            width: inner_area.width,
            height: 1,
        };
        Gauge::default()
            .ratio(ratio)
            .label(gauge_label)
            .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
            .style(Style::default().fg(Color::White).bg(Color::Black))
            .render(gauge_area, buf);

        // Row 2 (batch only): file count info
        if show_batch {
            let batch_line = if t.is_dir {
                format!(
                    "  {} files  ·  File {} of {}",
                    t.file_count,
                    self.transfer.batch_done + 1,
                    self.transfer.batch_total
                )
            } else {
                format!(
                    "  File {} of {}",
                    self.transfer.batch_done + 1,
                    self.transfer.batch_total
                )
            };
            let row2 = Line::from(Span::styled(batch_line, dim));
            buf.set_line(inner_area.x, inner_area.y + 2, &row2, inner_area.width);
        }

        true
    }

    /// Render directional arrows on the panel border during a cross-panel drag.
    fn render_drag_arrow(&self, area: Rect, buf: &mut Buffer) {
        let drag = match self.drag {
            Some(ref d) => d,
            None => return,
        };
        let layout = browser_layout(area);
        // Arrows always show: direction is based on drag origin panel.
        let chars: [char; 2] = match drag.origin {
            BrowserFocus::Local => ['>', '>'],
            BrowserFocus::Remote => ['<', '<'],
        };
        let x0 = layout.local_panel.x + layout.local_panel.width - 1;
        let x1 = layout.remote_panel.x;
        let style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let h = layout.local_panel.height;
        let count = 3.min(h as usize);
        let mid = layout.local_panel.y + h / 2;
        let start = mid.saturating_sub(count as u16 / 2);
        for y in start..start + count as u16 {
            for (cx, ch) in [(x0, chars[0]), (x1, chars[1])] {
                buf[(cx, y)].set_char(ch).set_style(style);
            }
        }
    }

    /// Render a ghost label near the cursor during a drag gesture.
    fn render_drag_ghost(&self, buf: &mut Buffer) {
        let drag = match self.drag {
            Some(ref d) => d,
            None => return,
        };
        let label = format!(" {} ", drag.label);
        let x = drag.mouse_col + 2;
        let y = drag.mouse_row;
        let Rect {
            x: bx,
            y: by,
            width: bw,
            height: bh,
        } = *buf.area();
        if y < by || y >= by + bh {
            return;
        }
        for (i, ch) in label.chars().enumerate() {
            let cx = x + i as u16;
            if cx >= bx && cx < bx + bw {
                let cell = &mut buf[(cx, y)];
                cell.set_char(ch);
                cell.fg = Color::Yellow;
                cell.modifier.insert(Modifier::BOLD);
            }
        }
    }

    /// Render the normal status bar (state badge + message + shortcuts).
    fn render_normal_status(
        &self,
        area: Rect,
        buf: &mut Buffer,
        state_label: &str,
        state_color: Color,
        progress_suffix: &str,
        bindings: &BrowserBindings,
    ) {
        let help = format!(" [{}]xfer [{}]rm ", bindings.transfer, bindings.delete);
        let help_len = help.chars().count() as u16;
        let help_x = area.x + area.width.saturating_sub(help_len);

        let duration_suffix = if let Some(d) = self.last_duration {
            format!(" ({})", Self::format_duration(d))
        } else {
            String::new()
        };

        let left_line = Line::from(vec![
            Span::styled(
                format!("[{}]", state_label),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}{}", self.status_msg, progress_suffix),
                Style::default().fg(state_color),
            ),
            Span::styled(duration_suffix, Style::default().fg(Color::DarkGray)),
        ]);
        buf.set_line(area.x, area.y, &left_line, help_x.saturating_sub(area.x));

        buf.set_span(
            help_x,
            area.y,
            &Span::styled(help, Style::default().fg(Color::DarkGray)),
            area.width.saturating_sub(help_x.saturating_sub(area.x)),
        );
    }
}

// ---------------------------------------------------------------------------
// FileBrowserView — the widget
// ---------------------------------------------------------------------------

/// What the status row shows when no delete-confirm bar is active.
pub enum StatusKind<'a> {
    /// State badge + status message + keybinding hints.
    Normal {
        label: &'a str,
        color: Color,
        progress: &'a str,
    },
    /// SCP password prompt (masked input).
    Password { masked_len: usize },
}

/// Dual-panel file browser: local + remote listings, a status row, and the
/// overlays driven by `BrowserCore` state (delete confirm, drop-upload
/// confirm, transfer progress, drag feedback). Renders into the pane's inner
/// area (the title bar is drawn by `PaneTreeView`); per-browser decisions
/// (status badge, password mode) arrive precomputed as config.
pub struct FileBrowserView<'a> {
    pub is_focus: bool,
    pub bindings: &'a BrowserBindings,
    pub status: StatusKind<'a>,
    /// Show the transfer-progress overlay (a transfer is running).
    pub transferring: bool,
}

impl StatefulWidget for FileBrowserView<'_> {
    type State = BrowserCore;

    fn render(self, area: Rect, buf: &mut Buffer, core: &mut BrowserCore) {
        let status_area = core.render_panels(area, buf, self.is_focus);
        if !core.render_confirm_delete(status_area, buf) {
            match self.status {
                StatusKind::Password { masked_len } => {
                    render_password_bar(status_area, buf, masked_len)
                }
                StatusKind::Normal {
                    label,
                    color,
                    progress,
                } => core.render_normal_status(
                    status_area,
                    buf,
                    label,
                    color,
                    progress,
                    self.bindings,
                ),
            }
        }
        core.render_upload_confirm(area, buf);
        core.render_transfer_progress(area, buf, self.transferring);
        core.render_drag_arrow(area, buf);
        core.render_drag_ghost(buf);
    }
}

/// Magenta password-entry bar shown in place of the status line.
fn render_password_bar(area: Rect, buf: &mut Buffer, masked_len: usize) {
    let stars = "*".repeat(masked_len);
    let text = format!("  Password: {stars}\u{2588}");
    let pad = (area.width as usize).saturating_sub(text.chars().count());
    let msg = format!("{}{}", text, " ".repeat(pad));
    let span = Span::styled(
        msg,
        Style::default()
            .fg(Color::White)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    );
    buf.set_span(area.x, area.y, &span, area.width);
}

#[cfg(test)]
mod tests {
    use super::render_password_bar;
    use crate::browser::common::{
        BrowserCore, DeleteKind, DeleteLocation, DeleteTarget, TransferDirection, TransferStatus,
    };
    use crate::keybindings::BrowserBindings;
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};

    fn render_buf(width: u16, height: u16) -> (Rect, Buffer) {
        let area = Rect::new(0, 0, width, height);
        (area, Buffer::empty(area))
    }

    fn buf_line_text(buf: &Buffer, y: u16) -> String {
        let w = buf.area().width;
        (0..w)
            .map(|x| {
                buf.cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect::<String>()
    }

    #[test]
    fn password_bar_masks_input() {
        let (area, mut buf) = render_buf(40, 1);
        render_password_bar(area, &mut buf, 3);
        let text = buf_line_text(&buf, 0);
        assert!(text.contains("Password:"), "should contain label: {}", text);
        assert!(text.contains("***"), "should mask with stars: {}", text);
    }

    #[test]
    fn password_bar_empty_input_has_no_stars() {
        let (area, mut buf) = render_buf(40, 1);
        render_password_bar(area, &mut buf, 0);
        let text = buf_line_text(&buf, 0);
        assert!(text.contains("Password:"), "should contain label: {}", text);
        assert!(
            !text.contains('*'),
            "should have no stars for empty password: {}",
            text
        );
    }

    #[test]
    fn render_confirm_delete_single_file() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Remote,
            kind: DeleteKind::File,
            path: "test.txt".to_string(),
        });
        let (area, mut buf) = render_buf(60, 1);

        let rendered = core.render_confirm_delete(area, &mut buf);

        assert!(rendered);
        let text = buf_line_text(&buf, 0);
        assert!(text.contains("Delete"), "should contain 'Delete': {}", text);
        assert!(
            text.contains("test.txt"),
            "should contain filename: {}",
            text
        );
        assert!(
            text.contains("[y] Yes"),
            "should contain yes option: {}",
            text
        );
        assert!(
            text.contains("[n] No"),
            "should contain no option: {}",
            text
        );
        assert!(
            text.contains("remote"),
            "should indicate remote side: {}",
            text
        );
    }

    #[test]
    fn render_confirm_delete_with_pending() {
        let mut core = BrowserCore::new("host");
        core.delete.confirm = Some(DeleteTarget {
            location: DeleteLocation::Local,
            kind: DeleteKind::Dir,
            path: "mydir".to_string(),
        });
        core.delete.pending.push(DeleteTarget {
            location: DeleteLocation::Local,
            kind: DeleteKind::File,
            path: "other.txt".to_string(),
        });
        let (area, mut buf) = render_buf(70, 1);

        core.render_confirm_delete(area, &mut buf);

        let text = buf_line_text(&buf, 0);
        assert!(text.contains("+1 more"), "should show +N more: {}", text);
        assert!(
            text.contains("local"),
            "should indicate local side: {}",
            text
        );
    }

    #[test]
    fn render_confirm_delete_returns_false_when_none() {
        let core = BrowserCore::new("host");
        let (area, mut buf) = render_buf(60, 1);

        let rendered = core.render_confirm_delete(area, &mut buf);

        assert!(!rendered);
    }

    #[test]
    fn render_normal_status_shows_state_and_message() {
        let mut core = BrowserCore::new("host");
        core.status_msg = "Connected to host".to_string();
        let bindings = BrowserBindings::default();
        let (area, mut buf) = render_buf(80, 1);

        core.render_normal_status(area, &mut buf, "idle", Color::Green, "", &bindings);

        let text = buf_line_text(&buf, 0);
        assert!(
            text.contains("[idle]"),
            "should contain state label: {}",
            text
        );
        assert!(
            text.contains("Connected to host"),
            "should contain status message: {}",
            text
        );
    }

    #[test]
    fn render_normal_status_shows_progress_suffix() {
        let mut core = BrowserCore::new("host");
        core.status_msg = "Downloading...".to_string();
        let bindings = BrowserBindings::default();
        let (area, mut buf) = render_buf(80, 1);

        core.render_normal_status(area, &mut buf, "transfer", Color::Green, " 42%", &bindings);

        let text = buf_line_text(&buf, 0);
        assert!(text.contains("42%"), "should contain progress: {}", text);
    }

    #[test]
    fn render_normal_status_shows_keybinding_hints() {
        let mut core = BrowserCore::new("host");
        core.status_msg = "ok".to_string();
        let bindings = BrowserBindings::default();
        let (area, mut buf) = render_buf(80, 1);

        core.render_normal_status(area, &mut buf, "idle", Color::Green, "", &bindings);

        let text = buf_line_text(&buf, 0);
        assert!(
            text.contains("xfer"),
            "should contain transfer hint: {}",
            text
        );
        assert!(text.contains("rm"), "should contain delete hint: {}", text);
    }

    #[test]
    fn render_normal_status_shows_duration() {
        let mut core = BrowserCore::new("host");
        core.status_msg = "ok".to_string();
        core.last_duration = Some(std::time::Duration::from_secs(5));
        let bindings = BrowserBindings::default();
        let (area, mut buf) = render_buf(80, 1);

        core.render_normal_status(area, &mut buf, "idle", Color::Green, "", &bindings);

        let text = buf_line_text(&buf, 0);
        assert!(text.contains("5s"), "should contain duration: {}", text);
    }

    #[test]
    fn render_transfer_progress_hidden_when_not_transferring() {
        let core = BrowserCore::new("host");
        let (area, mut buf) = render_buf(60, 10);

        let rendered = core.render_transfer_progress(area, &mut buf, false);

        assert!(!rendered);
    }

    #[test]
    fn render_transfer_progress_shows_gauge_when_active() {
        let mut core = BrowserCore::new("host");
        core.transfer.last = Some(TransferStatus {
            filename: "big.bin".to_string(),
            direction: TransferDirection::Download,
            is_dir: false,
            done: false,
            progress: 50,
            file_count: 0,
        });
        core.transfer.batch_done = 0;
        core.transfer.batch_total = 1;
        core.transfer.start = Some(std::time::Instant::now());
        let (area, mut buf) = render_buf(60, 10);

        let rendered = core.render_transfer_progress(area, &mut buf, true);

        assert!(rendered);
        // Check that the progress bar area contains something (not blank)
        let mut found_content = false;
        for y in 0..area.height {
            let text = buf_line_text(&buf, y);
            if text.contains("big.bin") || text.contains("50") || text.contains("Download") {
                found_content = true;
                break;
            }
        }
        assert!(found_content, "should render transfer progress content");
    }

    #[test]
    fn render_transfer_progress_shows_batch_info() {
        let mut core = BrowserCore::new("host");
        core.transfer.last = Some(TransferStatus {
            filename: "file2.txt".to_string(),
            direction: TransferDirection::Upload,
            is_dir: false,
            done: false,
            progress: 75,
            file_count: 0,
        });
        core.transfer.batch_done = 1;
        core.transfer.batch_total = 3;
        core.transfer.start = Some(std::time::Instant::now());
        let (area, mut buf) = render_buf(60, 10);

        core.render_transfer_progress(area, &mut buf, true);

        // Format: "  File 2 of 3"
        let mut found_batch = false;
        for y in 0..area.height {
            let text = buf_line_text(&buf, y);
            if text.contains("File 2 of 3") {
                found_batch = true;
                break;
            }
        }
        assert!(found_batch, "should show batch progress 'File 2 of 3'");
    }
}
