//! Embedded terminal component: PTY-backed session state (`session`) and the
//! vt100 screen-grid renderer (`view`).

mod session;
mod view;

pub use session::{EmbeddedTerminal, PtyChannel, split_ssh_args};
#[cfg(test)]
pub use session::{MockPty, MockPtyHandle};
pub use view::{TerminalView, render_screen};
