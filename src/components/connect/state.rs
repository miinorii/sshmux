use ratatui::widgets::ListState;

use crate::keybindings::GROUP_SIZES;

// ---------------------------------------------------------------------------
// Key editor constants & navigation
// ---------------------------------------------------------------------------

/// Number of bindings per group, derived from the binding table
/// (`define_bindings!` in keybindings.rs).
const GLOBAL_COUNT: usize = GROUP_SIZES[0];
const CONNECT_COUNT: usize = GROUP_SIZES[1];
const BROWSER_COUNT: usize = GROUP_SIZES[2];

/// Header indices in the flat display list.
pub const HEADER_GLOBAL: usize = 0;
pub const HEADER_CONNECT: usize = GLOBAL_COUNT + 1;
pub const HEADER_BROWSER: usize = GLOBAL_COUNT + 1 + CONNECT_COUNT + 1;

/// Total rows in the editor list (3 headers + all bindings).
pub const EDITOR_ROW_COUNT: usize = GLOBAL_COUNT + CONNECT_COUNT + BROWSER_COUNT + 3;

/// Returns true if the given index is a section header row.
pub fn is_editor_header(idx: usize) -> bool {
    idx == HEADER_GLOBAL || idx == HEADER_CONNECT || idx == HEADER_BROWSER
}

/// Map a display index to a binding entry index, or None for headers.
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeyBindings;

    /// The editor constants above are hand-maintained copies of the group
    /// sizes in `KeyBindings::entries()`. If they drift, rendering the key
    /// editor panics with an out-of-bounds `entries[idx]` — fail here instead.
    #[test]
    fn editor_constants_match_binding_entries() {
        let kb = KeyBindings::default();
        let entries = kb.entries();
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
