//! A ratatui [`Backend`] wrapper that gives us control over how ANSI colour
//! escapes are serialised to the terminal.
//!
//! # The problem
//!
//! Terminal emulators only apply bold-brightening to basic ANSI colour
//! escapes (`\x1b[30..37m` / `\x1b[90..97m`), not to the 256-colour form
//! (`\x1b[38;5;Nm`). This is standard behaviour across XTerm, VTE, and
//! Windows Terminal (tested on 1.23.20211.0).
//! See <https://github.com/microsoft/terminal/issues/5384>.
//!
//! When a remote program emits `\x1b[01;34m` (bold blue), `ssh.exe` passes
//! it through unchanged and the terminal brightens it. In sshmux the bytes
//! go through three layers before reaching the terminal, and none of them
//! preserve the original escape form:
//!
//! 1. **vt100** parses both `\x1b[34m` (basic) and `\x1b[38;5;4m` (256-colour)
//!    into the same `Color::Idx(4)` — the original form is lost.
//! 2. **ratatui** has no colour variant that distinguishes "basic ANSI" from
//!    "256-colour indexed" — only `Color::Indexed(u8)`.
//! 3. **crossterm 0.29** serialises every colour in 256-colour form (`38;5;N`),
//!    including named colours like `Color::DarkBlue`. There is no variant or
//!    setting that emits basic ANSI (`34`).
//!    See <https://github.com/crossterm-rs/crossterm/issues/844>.
//!
//! The result: `\x1b[01;34m` goes in, `\x1b[1m` + `\x1b[38;5;4m` comes out,
//! bold-brightening is lost, and the rendered blue doesn't match a direct
//! `ssh.exe` connection. To reproduce:
//!
//! ```text
//! for i in $(seq 0 15); do printf "\x1b[38;5;${i}m %3d \x1b[0m" $i; [ $((i % 8)) -eq 7 ] && echo; done && echo "--- bold ---" && for i in $(seq 0 15); do printf "\x1b[1;38;5;${i}m %3d \x1b[0m" $i; [ $((i % 8)) -eq 7 ] && echo; done
//! printf '\x1b[1;34m BOLD-BASIC \x1b[0m \x1b[1m\x1b[38;5;4m BOLD-256 \x1b[0m\n'
//! ```
//!
//! # The fix
//!
//! This backend re-implements the `draw` loop to bypass crossterm's colour
//! serialisation. `Color::Indexed(0..=15)` — the values produced by
//! [`vc()`](crate::terminal::vc) for SSH session output — are emitted as
//! basic ANSI escapes, preserving bold-brightening behaviour. Named ratatui
//! colours used by sshmux's own UI chrome (`Color::Black`, `Color::DarkGray`,
//! etc.) remain in 256-colour form so the interface looks consistent across
//! terminals. `Color::Indexed(16..=255)` and `Color::Rgb` are unchanged.

use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute as CtAttribute, Print, SetAttribute},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::{Color, Modifier},
};

/// Backend wrapper. Holds an inner [`CrosstermBackend`] for cursor / clear /
/// size operations and to satisfy the `Write` requirement; only `draw` is
/// re-implemented.
pub struct ColorBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> ColorBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

impl<W: Write> Write for ColorBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

/// Basic foreground SGR codes for ANSI indices 0..=15, indexed by colour
/// number. Indices 0..=7 map to 30..=37; indices 8..=15 map to 90..=97.
const FG_BASIC: [&[u8]; 16] = [
    b"30", b"31", b"32", b"33", b"34", b"35", b"36", b"37", b"90", b"91", b"92", b"93", b"94",
    b"95", b"96", b"97",
];

/// Basic background SGR codes for ANSI indices 0..=15. 0..=7 → 40..=47;
/// 8..=15 → 100..=107.
const BG_BASIC: [&[u8]; 16] = [
    b"40", b"41", b"42", b"43", b"44", b"45", b"46", b"47", b"100", b"101", b"102", b"103", b"104",
    b"105", b"106", b"107",
];

/// Write the foreground SGR parameter for `c` (without leading `\x1b[` or
/// trailing `m`).
///
/// `Color::Indexed(0..=15)` is emitted as basic ANSI (`30..37`/`90..97`) so
/// that terminal emulators render them through the user's colour scheme.
/// Named ratatui colours (`Color::Black` etc.) are emitted as 256-colour
/// (`38;5;N`) — identical to how crossterm 0.29 handles them — so that
/// sshmux's own UI chrome stays fixed.
fn write_fg<W: Write>(w: &mut W, c: Color) -> io::Result<()> {
    match c {
        Color::Reset => w.write_all(b"39"),
        Color::Indexed(i @ 0..=15) => w.write_all(FG_BASIC[i as usize]),
        Color::Indexed(i) => write!(w, "38;5;{i}"),
        Color::Rgb(r, g, b) => write!(w, "38;2;{r};{g};{b}"),
        // Named ratatui colours → crossterm-compatible 256-colour form.
        Color::Black => w.write_all(b"38;5;0"),
        Color::Red => w.write_all(b"38;5;1"),
        Color::Green => w.write_all(b"38;5;2"),
        Color::Yellow => w.write_all(b"38;5;3"),
        Color::Blue => w.write_all(b"38;5;4"),
        Color::Magenta => w.write_all(b"38;5;5"),
        Color::Cyan => w.write_all(b"38;5;6"),
        Color::Gray => w.write_all(b"38;5;7"),
        Color::DarkGray => w.write_all(b"38;5;8"),
        Color::LightRed => w.write_all(b"38;5;9"),
        Color::LightGreen => w.write_all(b"38;5;10"),
        Color::LightYellow => w.write_all(b"38;5;11"),
        Color::LightBlue => w.write_all(b"38;5;12"),
        Color::LightMagenta => w.write_all(b"38;5;13"),
        Color::LightCyan => w.write_all(b"38;5;14"),
        Color::White => w.write_all(b"38;5;15"),
    }
}

/// Write the background SGR parameter for `c` (without leading `\x1b[` or
/// trailing `m`).
fn write_bg<W: Write>(w: &mut W, c: Color) -> io::Result<()> {
    match c {
        Color::Reset => w.write_all(b"49"),
        Color::Indexed(i @ 0..=15) => w.write_all(BG_BASIC[i as usize]),
        Color::Indexed(i) => write!(w, "48;5;{i}"),
        Color::Rgb(r, g, b) => write!(w, "48;2;{r};{g};{b}"),
        Color::Black => w.write_all(b"48;5;0"),
        Color::Red => w.write_all(b"48;5;1"),
        Color::Green => w.write_all(b"48;5;2"),
        Color::Yellow => w.write_all(b"48;5;3"),
        Color::Blue => w.write_all(b"48;5;4"),
        Color::Magenta => w.write_all(b"48;5;5"),
        Color::Cyan => w.write_all(b"48;5;6"),
        Color::Gray => w.write_all(b"48;5;7"),
        Color::DarkGray => w.write_all(b"48;5;8"),
        Color::LightRed => w.write_all(b"48;5;9"),
        Color::LightGreen => w.write_all(b"48;5;10"),
        Color::LightYellow => w.write_all(b"48;5;11"),
        Color::LightBlue => w.write_all(b"48;5;12"),
        Color::LightMagenta => w.write_all(b"48;5;13"),
        Color::LightCyan => w.write_all(b"48;5;14"),
        Color::White => w.write_all(b"48;5;15"),
    }
}

/// Emit a single combined SGR sequence setting both foreground and background.
fn write_color_sgr<W: Write>(w: &mut W, fg: Color, bg: Color) -> io::Result<()> {
    w.write_all(b"\x1b[")?;
    write_fg(w, fg)?;
    w.write_all(b";")?;
    write_bg(w, bg)?;
    w.write_all(b"m")
}

/// Diff `from` -> `to` and emit the necessary SetAttribute commands. Mirrors
/// ratatui-crossterm's private `ModifierDiff::queue`.
fn write_modifier_diff<W: Write>(w: &mut W, from: Modifier, to: Modifier) -> io::Result<()> {
    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CtAttribute::NoReverse))?;
    }
    if removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM) {
        // Bold and Dim are both reset by NormalIntensity; reapply whichever
        // remains in `to`.
        queue!(w, SetAttribute(CtAttribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CtAttribute::Dim))?;
        }
        if to.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CtAttribute::Bold))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CtAttribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CtAttribute::NoUnderline))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CtAttribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CtAttribute::NoBlink))?;
    }

    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CtAttribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(CtAttribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CtAttribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CtAttribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) {
        queue!(w, SetAttribute(CtAttribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CtAttribute::CrossedOut))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(w, SetAttribute(CtAttribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CtAttribute::RapidBlink))?;
    }
    Ok(())
}

impl<W: Write> Backend for ColorBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<Position> = None;
        for (x, y, cell) in content {
            if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
                queue!(self.inner, MoveTo(x, y))?;
            }
            last_pos = Some(Position { x, y });
            if cell.modifier != modifier {
                write_modifier_diff(&mut self.inner, modifier, cell.modifier)?;
                modifier = cell.modifier;
            }
            if cell.fg != fg || cell.bg != bg {
                write_color_sgr(&mut self.inner, cell.fg, cell.bg)?;
                fg = cell.fg;
                bg = cell.bg;
            }
            queue!(self.inner, Print(cell.symbol()))?;
        }
        // Reset everything at the end of the frame, matching ratatui-crossterm.
        write_color_sgr(&mut self.inner, Color::Reset, Color::Reset)?;
        queue!(self.inner, SetAttribute(CtAttribute::Reset))?;
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr(fg: Color, bg: Color) -> Vec<u8> {
        let mut buf = Vec::new();
        write_color_sgr(&mut buf, fg, bg).unwrap();
        buf
    }

    // ── Indexed(0..=15) → basic ANSI (session output via vc()) ──

    #[test]
    fn indexed_basic_blue_emits_34() {
        // The whole point: Indexed(4) from vc() must produce \x1b[34m,
        // NOT \x1b[38;5;4m which bypasses the user's colour scheme.
        assert_eq!(sgr(Color::Indexed(4), Color::Reset), b"\x1b[34;49m");
    }

    #[test]
    fn indexed_0_to_7_use_basic_fg() {
        assert_eq!(sgr(Color::Indexed(0), Color::Reset), b"\x1b[30;49m");
        assert_eq!(sgr(Color::Indexed(1), Color::Reset), b"\x1b[31;49m");
        assert_eq!(sgr(Color::Indexed(2), Color::Reset), b"\x1b[32;49m");
        assert_eq!(sgr(Color::Indexed(3), Color::Reset), b"\x1b[33;49m");
        assert_eq!(sgr(Color::Indexed(4), Color::Reset), b"\x1b[34;49m");
        assert_eq!(sgr(Color::Indexed(5), Color::Reset), b"\x1b[35;49m");
        assert_eq!(sgr(Color::Indexed(6), Color::Reset), b"\x1b[36;49m");
        assert_eq!(sgr(Color::Indexed(7), Color::Reset), b"\x1b[37;49m");
    }

    #[test]
    fn indexed_8_to_15_use_bright_fg() {
        assert_eq!(sgr(Color::Indexed(8), Color::Reset), b"\x1b[90;49m");
        assert_eq!(sgr(Color::Indexed(9), Color::Reset), b"\x1b[91;49m");
        assert_eq!(sgr(Color::Indexed(10), Color::Reset), b"\x1b[92;49m");
        assert_eq!(sgr(Color::Indexed(11), Color::Reset), b"\x1b[93;49m");
        assert_eq!(sgr(Color::Indexed(12), Color::Reset), b"\x1b[94;49m");
        assert_eq!(sgr(Color::Indexed(13), Color::Reset), b"\x1b[95;49m");
        assert_eq!(sgr(Color::Indexed(14), Color::Reset), b"\x1b[96;49m");
        assert_eq!(sgr(Color::Indexed(15), Color::Reset), b"\x1b[97;49m");
    }

    #[test]
    fn indexed_0_to_7_use_basic_bg() {
        assert_eq!(sgr(Color::Reset, Color::Indexed(0)), b"\x1b[39;40m");
        assert_eq!(sgr(Color::Reset, Color::Indexed(4)), b"\x1b[39;44m");
        assert_eq!(sgr(Color::Reset, Color::Indexed(7)), b"\x1b[39;47m");
    }

    #[test]
    fn indexed_8_to_15_use_bright_bg() {
        assert_eq!(sgr(Color::Reset, Color::Indexed(8)), b"\x1b[39;100m");
        assert_eq!(sgr(Color::Reset, Color::Indexed(12)), b"\x1b[39;104m");
        assert_eq!(sgr(Color::Reset, Color::Indexed(15)), b"\x1b[39;107m");
    }

    // ── Indexed(16..=255) → 256-colour (xterm palette) ──

    #[test]
    fn indexed_high_uses_256_color_form() {
        assert_eq!(sgr(Color::Indexed(16), Color::Reset), b"\x1b[38;5;16;49m");
        assert_eq!(sgr(Color::Indexed(231), Color::Reset), b"\x1b[38;5;231;49m");
        assert_eq!(sgr(Color::Reset, Color::Indexed(16)), b"\x1b[39;48;5;16m");
    }

    // ── Named ratatui colours → 256-colour (UI chrome, matches crossterm) ──

    #[test]
    fn named_black_uses_256_color() {
        // Named Color::Black is used by sshmux's UI, NOT session output.
        // Must stay 256-colour so UI looks identical to before.
        assert_eq!(sgr(Color::Black, Color::Reset), b"\x1b[38;5;0;49m");
    }

    #[test]
    fn named_blue_uses_256_color() {
        assert_eq!(sgr(Color::Blue, Color::Reset), b"\x1b[38;5;4;49m");
    }

    #[test]
    fn named_darkgray_uses_256_color() {
        assert_eq!(sgr(Color::DarkGray, Color::Reset), b"\x1b[38;5;8;49m");
    }

    #[test]
    fn named_white_uses_256_color() {
        assert_eq!(sgr(Color::White, Color::Reset), b"\x1b[38;5;15;49m");
    }

    #[test]
    fn named_bg_uses_256_color() {
        assert_eq!(sgr(Color::Reset, Color::Black), b"\x1b[39;48;5;0m");
        assert_eq!(sgr(Color::Reset, Color::DarkGray), b"\x1b[39;48;5;8m");
    }

    // ── RGB → truecolour ──

    #[test]
    fn rgb_uses_truecolor_form() {
        assert_eq!(
            sgr(Color::Rgb(10, 20, 30), Color::Reset),
            b"\x1b[38;2;10;20;30;49m"
        );
        assert_eq!(
            sgr(Color::Reset, Color::Rgb(10, 20, 30)),
            b"\x1b[39;48;2;10;20;30m"
        );
    }

    // ── Combined fg + bg ──

    #[test]
    fn indexed_fg_and_bg_combined() {
        assert_eq!(sgr(Color::Indexed(4), Color::Indexed(3)), b"\x1b[34;43m");
    }

    #[test]
    fn mixed_types_combined() {
        assert_eq!(
            sgr(Color::Rgb(1, 2, 3), Color::Indexed(42)),
            b"\x1b[38;2;1;2;3;48;5;42m"
        );
    }

    #[test]
    fn reset_pair() {
        assert_eq!(sgr(Color::Reset, Color::Reset), b"\x1b[39;49m");
    }
}
