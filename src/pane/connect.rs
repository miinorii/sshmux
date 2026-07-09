use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState, Paragraph, StatefulWidget, Widget},
};

use super::render_pane_border;
use crate::keybindings::KeyBindings;
use crate::ssh_config::SshHost;

// ---------------------------------------------------------------------------
// Key editor constants & navigation
// ---------------------------------------------------------------------------

/// Number of bindings per group (for header index calculation).
const GLOBAL_COUNT: usize = 12;
const CONNECT_COUNT: usize = 6;

/// Header indices in the flat display list.
pub const HEADER_GLOBAL: usize = 0;
pub const HEADER_CONNECT: usize = GLOBAL_COUNT + 1; // 13
pub const HEADER_BROWSER: usize = GLOBAL_COUNT + 1 + CONNECT_COUNT + 1; // 20

/// Total rows in the editor list (3 headers + 28 bindings).
pub const EDITOR_ROW_COUNT: usize = 31;

/// Returns true if the given index is a section header row.
pub fn is_editor_header(idx: usize) -> bool {
    idx == HEADER_GLOBAL || idx == HEADER_CONNECT || idx == HEADER_BROWSER
}

/// Map a display index to a binding entry index (0..26), or None for headers.
pub fn editor_binding_index(display_idx: usize) -> Option<usize> {
    if is_editor_header(display_idx) {
        return None;
    }
    let binding_idx = if display_idx < HEADER_CONNECT {
        display_idx - 1 // subtract global header
    } else if display_idx < HEADER_BROWSER {
        display_idx - 2 // subtract global + connect headers
    } else {
        display_idx - 3 // subtract all 3 headers
    };
    Some(binding_idx)
}

/// Move selection to next non-header row (wrapping).
pub fn editor_nav_down(list_state: &mut ListState) {
    let cur = list_state.selected().unwrap_or(0);
    let mut next = cur;
    for _ in 0..EDITOR_ROW_COUNT {
        next += 1;
        if next >= EDITOR_ROW_COUNT {
            next = 1; // wrap to first binding (skip header at 0)
        }
        if !is_editor_header(next) {
            break;
        }
    }
    list_state.select(Some(next));
}

/// Move selection to previous non-header row (wrapping).
pub fn editor_nav_up(list_state: &mut ListState) {
    let cur = list_state.selected().unwrap_or(0);
    let mut prev = cur;
    for _ in 0..EDITOR_ROW_COUNT {
        if prev == 0 {
            prev = EDITOR_ROW_COUNT - 1;
        } else {
            prev -= 1;
        }
        if !is_editor_header(prev) {
            break;
        }
    }
    list_state.select(Some(prev));
}

// ---------------------------------------------------------------------------
// ConnectOverlay — mutually exclusive overlay states
// ---------------------------------------------------------------------------

pub enum ConnectOverlay {
    None,
    BrowserMenu(ListState),
    ConnectInput(InputField),
    KeyEditor(KeyEditorState),
}

// ---------------------------------------------------------------------------
// InputField — single-line text input with a movable cursor
// ---------------------------------------------------------------------------

/// Single-line editable text with a cursor (char index, may sit one past the
/// last char). Rendering uses [`InputField::view`] to horizontally scroll the
/// text so the cursor is always visible.
#[derive(Default)]
pub struct InputField {
    pub text: String,
    cursor: usize,
}

impl InputField {
    pub fn new() -> Self {
        Self::default()
    }

    /// Byte offset of the cursor's char index.
    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// Delete the char before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    /// Delete the char at the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.char_len() {
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_len();
    }

    /// Return the visible window of the text for a field `width` chars wide,
    /// padded to `width`, plus the cursor's column inside that window. The
    /// window scrolls so the cursor is always visible.
    pub fn view(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let scroll = self.cursor.saturating_sub(width - 1);
        let visible: String = self.text.chars().skip(scroll).take(width).collect();
        let padded = format!("{:<width$}", visible, width = width);
        (padded, self.cursor - scroll)
    }
}

pub struct KeyEditorState {
    pub list_state: ListState,
    pub editing: bool,
    pub status: Option<String>,
}

impl Default for KeyEditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyEditorState {
    pub fn new() -> Self {
        let mut ls = ListState::default();
        ls.select(Some(1)); // first binding (index 0 is a header)
        Self {
            list_state: ls,
            editing: false,
            status: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ConnectPane
// ---------------------------------------------------------------------------

pub struct ConnectPane {
    pub list_state: ListState,
    pub overlay: ConnectOverlay,
}

impl Default for ConnectPane {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectPane {
    pub fn new() -> Self {
        let mut ls = ListState::default();
        ls.select_first();
        ConnectPane {
            list_state: ls,
            overlay: ConnectOverlay::None,
        }
    }

    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        is_focus: bool,
        hosts: &[SshHost],
        leaf_count: usize,
        keybindings: &KeyBindings,
    ) {
        let inner = render_pane_border(area, buf, is_focus, leaf_count, "connect");

        let list_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };
        let hint_y = inner.y + inner.height.saturating_sub(1);

        let items: Vec<&str> = hosts.iter().map(|h| h.label.as_str()).collect();
        let list = List::new(items)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        StatefulWidget::render(list, list_area, buf, &mut self.list_state);

        let help_key = format!("  {}", keybindings.connect.help);
        buf.set_line(
            inner.x,
            hint_y,
            &Line::from(vec![
                Span::raw(help_key).style(Style::default().fg(Color::Yellow)),
                Span::raw(" keybindings").style(Style::default().fg(Color::DarkGray)),
            ]),
            inner.width,
        );

        match &mut self.overlay {
            ConnectOverlay::BrowserMenu(menu_state) => {
                let menu_w = 36u16;
                let menu_h = 4u16; // border + 2 items + border
                let cx = inner.x + inner.width.saturating_sub(menu_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(menu_h) / 2;
                let menu_area = Rect {
                    x: cx,
                    y: cy,
                    width: menu_w.min(inner.width),
                    height: menu_h.min(inner.height),
                };
                let menu_items = vec!["SFTP", "SCP (legacy, linux target)"];
                let menu_list = List::new(menu_items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" Browse with "),
                    )
                    .style(Style::default().fg(Color::White))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                StatefulWidget::render(menu_list, menu_area, buf, menu_state);
            }
            ConnectOverlay::KeyEditor(editor) => {
                let entries = keybindings.entries();
                let selected_display = editor.list_state.selected().unwrap_or(0);
                let groups = ["Global", "Connect", "Browser"];
                let header_indices = [HEADER_GLOBAL, HEADER_CONNECT, HEADER_BROWSER];

                let mut items: Vec<Line> = Vec::with_capacity(EDITOR_ROW_COUNT);
                let mut entry_idx = 0usize;
                for row in 0..EDITOR_ROW_COUNT {
                    if let Some(gi) = header_indices.iter().position(|&h| h == row) {
                        let label = format!(" ── {} ", groups[gi]);
                        items.push(Line::from(Span::styled(
                            label,
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )));
                    } else {
                        let e = &entries[entry_idx];
                        entry_idx += 1;
                        let key_str = if editor.editing && row == selected_display {
                            "[press key...]".to_string()
                        } else {
                            e.binding.to_string()
                        };
                        let key_style = if editor.editing && row == selected_display {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Yellow)
                        };
                        items.push(Line::from(vec![
                            Span::styled(format!(" {:14}", key_str), key_style),
                            Span::styled(e.description, Style::default().fg(Color::DarkGray)),
                        ]));
                    }
                }

                let title = if editor.editing {
                    " press a key to bind "
                } else {
                    " keybindings (Enter to edit) "
                };

                let status_line = editor.status.as_deref().unwrap_or("");
                let extra_h = if status_line.is_empty() { 0u16 } else { 1 };

                let ed_w = 44u16.min(inner.width.saturating_sub(2));
                let ed_h = (EDITOR_ROW_COUNT as u16 + 2 + extra_h).min(inner.height);
                let cx = inner.x + inner.width.saturating_sub(ed_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(ed_h) / 2;
                let ed_area = Rect {
                    x: cx,
                    y: cy,
                    width: ed_w,
                    height: ed_h,
                };

                let ed_list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(title),
                    )
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");
                StatefulWidget::render(ed_list, ed_area, buf, &mut editor.list_state);

                if !status_line.is_empty() {
                    let status_y = ed_area.y + ed_area.height.saturating_sub(2);
                    buf.set_line(
                        ed_area.x + 2,
                        status_y,
                        &Line::from(Span::styled(status_line, Style::default().fg(Color::Green))),
                        ed_area.width.saturating_sub(4),
                    );
                }
            }
            ConnectOverlay::ConnectInput(input) => {
                let input_w = 50u16.min(inner.width.saturating_sub(2));
                let input_h = 4u16;
                let cx = inner.x + inner.width.saturating_sub(input_w) / 2;
                let cy = inner.y + inner.height.saturating_sub(input_h) / 2;
                let input_area = Rect {
                    x: cx,
                    y: cy,
                    width: input_w,
                    height: input_h,
                };
                let text_w = input_w.saturating_sub(2) as usize;
                let (display, cursor_col) = input.view(text_w);
                let paragraph = Paragraph::new(vec![
                    Line::from(Span::raw(display).style(Style::default().fg(Color::White))),
                    Line::from(
                        Span::raw("e.g. -o StrictHostKeyChecking=no user@host")
                            .style(Style::default().fg(Color::DarkGray)),
                    ),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(" ssh "),
                );
                paragraph.render(input_area, buf);
                // Block cursor: reverse the cell at the cursor column.
                let cur_x = input_area.x + 1 + cursor_col as u16;
                let cur_y = input_area.y + 1;
                if cur_x < input_area.x + input_w.saturating_sub(1) {
                    let style = buf[(cur_x, cur_y)].style().add_modifier(Modifier::REVERSED);
                    buf[(cur_x, cur_y)].set_style(style);
                }
            }
            ConnectOverlay::None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The editor constants above are hand-maintained copies of the group
    /// sizes in `KeyBindings::entries()`. If they drift, rendering the key
    /// editor panics with an out-of-bounds `entries[idx]` — fail here instead.
    #[test]
    fn editor_constants_match_binding_entries() {
        let entries = KeyBindings::default().entries();
        let globals = entries.iter().filter(|e| e.group == "global").count();
        let connects = entries.iter().filter(|e| e.group == "connect").count();
        let browsers = entries.iter().filter(|e| e.group == "browser").count();
        assert_eq!(GLOBAL_COUNT, globals, "GLOBAL_COUNT out of sync");
        assert_eq!(CONNECT_COUNT, connects, "CONNECT_COUNT out of sync");
        assert_eq!(
            EDITOR_ROW_COUNT,
            globals + connects + browsers + 3,
            "EDITOR_ROW_COUNT out of sync (3 headers + all bindings)"
        );
    }

    // ---- InputField ----

    fn field_with(text: &str) -> InputField {
        let mut f = InputField::new();
        for c in text.chars() {
            f.insert(c);
        }
        f
    }

    #[test]
    fn input_field_insert_and_backspace_at_end() {
        let mut f = field_with("ab");
        f.backspace();
        assert_eq!(f.text, "a");
    }

    #[test]
    fn input_field_cursor_movement_and_mid_insert() {
        let mut f = field_with("host");
        f.move_left();
        f.move_left();
        f.insert('X');
        assert_eq!(f.text, "hoXst");
        f.move_home();
        f.insert('>');
        assert_eq!(f.text, ">hoXst");
        f.move_end();
        f.insert('!');
        assert_eq!(f.text, ">hoXst!");
    }

    #[test]
    fn input_field_delete_at_cursor() {
        let mut f = field_with("abc");
        f.move_home();
        f.delete();
        assert_eq!(f.text, "bc");
        f.delete();
        f.delete();
        f.delete(); // extra delete on empty is a no-op
        assert_eq!(f.text, "");
    }

    #[test]
    fn input_field_backspace_mid_text() {
        let mut f = field_with("abcd");
        f.move_left(); // cursor between c and d
        f.backspace(); // removes c
        assert_eq!(f.text, "abd");
        f.insert('C');
        assert_eq!(f.text, "abCd");
    }

    #[test]
    fn input_field_left_saturates_right_clamps() {
        let mut f = field_with("ab");
        f.move_home();
        f.move_left(); // no-op at 0
        f.insert('<');
        assert_eq!(f.text, "<ab");
        f.move_end();
        f.move_right(); // no-op at end
        f.insert('>');
        assert_eq!(f.text, "<ab>");
    }

    #[test]
    fn input_field_utf8_editing() {
        let mut f = field_with("héllo");
        f.move_left();
        f.move_left();
        f.backspace(); // removes the second l
        assert_eq!(f.text, "hélo");
    }

    #[test]
    fn input_field_view_fits_without_scroll() {
        let f = field_with("abc");
        let (view, col) = f.view(10);
        assert_eq!(view, "abc       ");
        assert_eq!(col, 3, "cursor sits after the text");
    }

    #[test]
    fn input_field_view_scrolls_to_keep_cursor_visible() {
        // 10 chars in a 5-wide field, cursor at the end: window shows the
        // tail and the cursor stays on the last column.
        let f = field_with("0123456789");
        let (view, col) = f.view(5);
        assert_eq!(view, "6789 ");
        assert_eq!(col, 4);
    }

    #[test]
    fn input_field_view_follows_cursor_back_left() {
        let mut f = field_with("0123456789");
        f.move_home();
        let (view, col) = f.view(5);
        assert_eq!(view, "01234");
        assert_eq!(col, 0, "window snaps back to the start");
    }

    #[test]
    fn input_field_view_zero_width() {
        let f = field_with("abc");
        assert_eq!(f.view(0), (String::new(), 0));
    }

    #[test]
    fn editor_nav_down_skips_headers() {
        let mut ls = ListState::default();
        ls.select(Some(HEADER_CONNECT - 1)); // last global binding
        editor_nav_down(&mut ls);
        let sel = ls.selected().unwrap();
        assert!(!is_editor_header(sel));
        assert_eq!(sel, HEADER_CONNECT + 1); // first connect binding
    }

    #[test]
    fn editor_nav_up_skips_headers() {
        let mut ls = ListState::default();
        ls.select(Some(HEADER_CONNECT + 1)); // first connect binding
        editor_nav_up(&mut ls);
        let sel = ls.selected().unwrap();
        assert!(!is_editor_header(sel));
        assert_eq!(sel, HEADER_CONNECT - 1); // last global binding
    }

    #[test]
    fn editor_nav_down_wraps_to_first_binding() {
        let mut ls = ListState::default();
        ls.select(Some(EDITOR_ROW_COUNT - 1)); // last row
        editor_nav_down(&mut ls);
        let sel = ls.selected().unwrap();
        assert!(!is_editor_header(sel));
        assert_eq!(sel, 1); // first binding (index 0 is header)
    }

    #[test]
    fn editor_nav_up_wraps_to_last_binding() {
        let mut ls = ListState::default();
        ls.select(Some(1)); // first binding
        editor_nav_up(&mut ls);
        let sel = ls.selected().unwrap();
        assert!(!is_editor_header(sel));
        assert_eq!(sel, EDITOR_ROW_COUNT - 1);
    }

    #[test]
    fn editor_nav_never_lands_on_header() {
        for start in 0..EDITOR_ROW_COUNT {
            let mut ls = ListState::default();
            ls.select(Some(start));
            editor_nav_down(&mut ls);
            assert!(
                !is_editor_header(ls.selected().unwrap()),
                "nav_down from {} landed on header {}",
                start,
                ls.selected().unwrap()
            );

            ls.select(Some(start));
            editor_nav_up(&mut ls);
            assert!(
                !is_editor_header(ls.selected().unwrap()),
                "nav_up from {} landed on header {}",
                start,
                ls.selected().unwrap()
            );
        }
    }
}
