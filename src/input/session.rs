//! Session key passthrough: translate keys into escape sequences and forward
//! them to the focused session pane.

use crossterm::event::KeyCode;
use log::trace;

use crate::app::App;
use crate::pane::Pane;

/// Translate a key into the matching escape sequence and forward it to the
/// focused session pane. Also resets scrollback on any keypress.
pub(super) fn handle_session_key(
    app: &mut App,
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    focused_pane_has_app_cursor: bool,
) {
    if let Some(Pane::Session { terminal, .. }) = app.tab_mut().focused_pane_mut() {
        terminal.reset_scroll();
    }

    // Ctrl+Arrow word-jump
    if ctrl && !alt {
        let seq = match code {
            KeyCode::Left => Some("\x1b[1;5D"),
            KeyCode::Right => Some("\x1b[1;5C"),
            KeyCode::Up => Some("\x1b[1;5A"),
            KeyCode::Down => Some("\x1b[1;5B"),
            _ => None,
        };
        if let Some(s) = seq {
            app.send_str(s);
            return;
        }
    }

    match code {
        KeyCode::Char(c) if ctrl && !alt => {
            // Convert Ctrl+<key> to its control byte. Some terminals report
            // Ctrl+C as Char('\x03') with CONTROL modifier instead of
            // Char('c') with CONTROL — handle both forms. Keys without a
            // control-byte meaning are dropped (never `wrapping_sub`, which
            // produced invalid >0x7F bytes for digits and punctuation).
            let byte = match c {
                c if c.is_ascii_control() => Some(c as u8),
                ' ' | '@' => Some(0x00), // Ctrl+Space / Ctrl+@ → NUL
                'a'..='z' => Some(c as u8 - b'a' + 1),
                'A'..='Z' => Some(c as u8 - b'A' + 1),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' => Some(0x1f),
                '?' => Some(0x7f),
                _ => None,
            };
            trace!(
                "ctrl+char: c={:?} (0x{:02X}) -> byte={:02X?}",
                c, c as u32, byte
            );
            if let Some(b) = byte {
                // All control bytes are < 0x80, so the char cast is a
                // single-byte UTF-8 encoding.
                app.send_char(b as char);
            }
        }
        // Unbound Alt+char → ESC prefix (Meta). AltGr chars arrive as
        // Ctrl+Alt and must fall through to the plain-char arm below.
        KeyCode::Char(c) if alt && !ctrl => {
            app.send_str(&format!("\x1b{c}"));
        }
        KeyCode::Char(c) => app.send_char(c),
        KeyCode::Enter => app.send_str("\r"),
        KeyCode::Backspace => app.send_str("\x7f"),
        KeyCode::Delete => app.send_str("\x1b[3~"),
        KeyCode::Tab => app.send_str("\t"),
        KeyCode::BackTab => app.send_str("\x1b[Z"),
        KeyCode::Left => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOD"
        } else {
            "\x1b[D"
        }),
        KeyCode::Right => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOC"
        } else {
            "\x1b[C"
        }),
        KeyCode::Up => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOA"
        } else {
            "\x1b[A"
        }),
        KeyCode::Down => app.send_str(if focused_pane_has_app_cursor {
            "\x1bOB"
        } else {
            "\x1b[B"
        }),
        KeyCode::Home => app.send_str("\x1b[H"),
        KeyCode::End => app.send_str("\x1b[F"),
        KeyCode::Esc => app.send_str("\x1b"),
        KeyCode::PageUp => app.send_str("\x1b[5~"),
        KeyCode::PageDown => app.send_str("\x1b[6~"),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => "",
            };
            if !seq.is_empty() {
                app.send_str(seq);
            }
        }
        _ => {}
    }
}
