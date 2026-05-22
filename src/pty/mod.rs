//! Cross-platform PTY abstraction.
//!
//! On Unix, this is a thin wrapper around `portable-pty`.
//! On Windows, this is a custom ConPTY backend that enables the modern
//! compatibility flags (`PSEUDOCONSOLE_RESIZE_QUIRK`,
//! `PSEUDOCONSOLE_WIN32_INPUT_MODE`, and `PSEUDOCONSOLE_PASSTHROUGH_MODE` on
//! Win11 22621+) that portable-pty does not opt into.

pub use portable_pty::CommandBuilder;

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::{ExitStatus, PtyChild, PtyMaster, PtyPair, PtySlave, openpty};

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{ExitStatus, PtyChild, PtyMaster, PtyPair, PtySlave, openpty};
