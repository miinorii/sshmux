# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build --release        # Release binary at target/release/sshmux
cargo build                  # Debug build
cargo test                   # Run all tests (~125 tests, inline in source files)
cargo test pane::tests       # Run tests for a specific module
cargo test test_name         # Run a single test by name
cargo clippy --release       # Lint — keep at zero warnings
```

Debug logging: `sshmux --debug` creates a timestamped log file in the current directory.

## Architecture

SSH session multiplexer TUI. Uses system `ssh`, `sftp`, and `scp` binaries — no Rust SSH library.

### Core loop

`main.rs` runs a 5ms poll loop: crossterm events → App dispatch → ratatui render. The App holds a `Vec<Tab>`, each Tab holds a tree of `Pane` nodes.

### Pane tree

`Pane` is a recursive enum — leaf variants (`Connect`, `Session`, `FileBrowser`, `SshBrowser`) or branch (`Split { kind, children }`). Splitting inserts a new Split node wrapping the target leaf and a new pane. Focus tracks a leaf index via depth-first traversal.

### PTY layer

`EmbeddedTerminal` (terminal.rs) wraps `portable_pty`. A background reader thread processes output through `vt100::Parser` (screen grid + 1000-line scrollback), accumulates `raw_output` (only when `capture_raw` is true — browsers only, not interactive sessions), and replies to DSR probes. The main thread writes via `send_str()`/`send_char()`.

Terminal state (mouse mode, application cursor, cursor visibility, alternate screen) is queried directly from `vt100::Screen` via methods on `EmbeddedTerminal` (`mouse_active()`, `app_cursor()`, `alternate_screen()`). No manual escape sequence scanning.

Three PTY constructors: `ssh()` (interactive session, no raw capture), `sftp()` (hidden, for SFTP browser, captures raw), `ssh_shell()` (hidden, for SCP browser, captures raw). A fourth, `ssh_raw()`, accepts arbitrary SSH arguments for manual connections.

### Scrollback

Interactive sessions have 1000-line scrollback via `vt100::Parser`. Mouse scroll (when the remote app doesn't capture mouse) adjusts `scroll_offset` and calls `screen.set_scrollback()`. In alternate screen mode (vim, htop), scroll instead sends arrow key sequences. Any keypress resets scroll to live view. Cursor is hidden during scrollback.

### Browser state machines

Both browsers (`FileBrowser` in browser/sftp.rs, `SshBrowser` in browser/ssh.rs) use prompt-stability detection: raw PTY buffer byte count unchanged for N ticks + expected prompt string present. They share parsing utilities from `browser/parse.rs` (ANSI stripping, `ls -la` parsing, transfer progress scraping).

**SFTP**: Detects `sftp>` prompt. Commands (`cd`, `get`, `put`, `rm`) run inside the SFTP session.

**SCP**: Sets `PS1='SSHMUX> '` after SSH auth, then detects `SSHMUX> ` prompt. Browsing uses shell commands (`ls`, `rm`, `pwd`). Transfers spawn separate `scp` processes in temporary PTYs.

### Key patterns

- `dirty: Arc<AtomicBool>` and `exited: Arc<AtomicBool>` for cross-thread state
- `raw_output: Arc<Mutex<Vec<u8>>>` — browsers scrape PTY output by reading and draining this buffer
- Connect pane has three mutually exclusive overlays: `browser_menu`, `connect_input`, `show_help`
- Browser focus toggle (`Tab` key) switches between local and remote panels
- `pane_inner()` computes render area by subtracting tab bar and shortcut bar

## Constraints

- No external SSH libraries (ssh2, russh). Must use system binaries only.
- Must work on both Windows (ConPTY) and Linux.
- ConPTY on Windows has known quirks: spurious SIGWINCH on mouse mode changes causes double-prompt artifacts in some remote shells. No clean fix found yet.

## Logging conventions

Use `log` crate. Levels: `info` for lifecycle events (connect, transfer, delete), `warn` for recoverable issues (password rejected, delete failed), `error` for failures (PTY errors, spawn failures), `debug` only for internal diagnostics (state machine details, resize events).
