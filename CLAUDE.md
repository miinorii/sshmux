# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build --release        # Release binary at target/release/sshmux
cargo build                  # Debug build
cargo test                   # Run all tests (~200 tests, inline in source files)
cargo test pane::tests       # Run tests for a specific module
cargo test test_name         # Run a single test by name
cargo clippy --release       # Lint — keep at zero warnings
cargo fmt                    # Format — run before every commit
```

Debug logging: `sshmux --log=LEVEL` (trace/debug/info/warn/error).

## Architecture

SSH session multiplexer TUI. Uses system `ssh`, `sftp`, and `scp` binaries — no Rust SSH library.

### Core loop

`main.rs` runs a 5ms poll loop: crossterm events → `input.rs` dispatch → ratatui render. The App holds a `Vec<Tab>`, each Tab holds a tree of `Pane` nodes. All key and mouse handling lives in `input.rs`.

### Pane tree

`Pane` is a recursive enum — leaf variants (`Connect`, `Session`, `FileBrowser`, `SshBrowser`) or branch (`Split { kind, children }`). Splitting inserts a new Split node wrapping the target leaf and a new pane. Focus tracks a leaf index via depth-first traversal.

### PTY layer

`EmbeddedTerminal` (terminal.rs) wraps `portable_pty`. A background reader thread processes output through `vt100::Parser` (screen grid + 1000-line scrollback), accumulates `raw_output` (only when `capture_raw` is true — browsers only, not interactive sessions), and replies to DSR probes. The main thread writes via `send_str()`/`send_char()`.

Terminal state (mouse mode, application cursor, cursor visibility, alternate screen) is queried directly from `vt100::Screen` via methods on `EmbeddedTerminal` (`mouse_active()`, `app_cursor()`, `alternate_screen()`). No manual escape sequence scanning.

Three PTY constructors: `ssh()` (interactive session, no raw capture), `sftp()` (hidden, for SFTP browser, captures raw), `ssh_shell()` (hidden, for SCP browser, captures raw). A fourth, `ssh_raw()`, accepts arbitrary SSH arguments for manual connections.

### Scrollback

Interactive sessions have 1000-line scrollback via `vt100::Parser`. Mouse scroll (when the remote app doesn't capture mouse) adjusts `scroll_offset` and calls `screen.set_scrollback()`. In alternate screen mode (vim, htop), scroll instead sends arrow key sequences. Any keypress resets scroll to live view. Cursor is hidden during scrollback.

### Browser state machines

Both browsers (`FileBrowser` in browser/sftp.rs, `SshBrowser` in browser/ssh.rs) hold a `BrowserCore` field (browser/common.rs) that provides shared state, dual-panel rendering, local navigation, click/drag handling, and the common key dispatch via `handle_browser_key()`. Browser-specific logic (SFTP commands, SCP process spawning, password prompts) stays on the outer struct.

Both use prompt-stability detection: raw PTY buffer byte count unchanged for N ticks + expected prompt string present. They share parsing utilities from `browser/parse.rs` (ANSI stripping, `ls -la` parsing, transfer progress scraping).

**SFTP**: Detects `sftp>` prompt. Commands (`cd`, `get`, `put`, `rm`) run inside the SFTP session.

**SCP**: Sets `PS1='SSHMUX> '` after SSH auth, then detects `SSHMUX> ` prompt. Browsing uses shell commands (`ls`, `rm`, `pwd`). Transfers spawn separate `scp` processes in temporary PTYs.

### Key patterns

- `dirty: Arc<AtomicBool>` and `exited: Arc<AtomicBool>` for cross-thread state
- `raw_output: Arc<Mutex<Vec<u8>>>` — browsers scrape PTY output by reading and draining this buffer
- Connect pane has three mutually exclusive overlays: `browser_menu`, `connect_input`, `show_help`
- Right-click context menu lives on `App` (not per-pane): `context_menu: Option<ContextMenu>`. Opens on right-click Down, tracks hover via Drag, executes on Up, dismissed by any keypress or resize. Right-click is intercepted before pane dispatch so it is never forwarded to remote apps.
- Browser focus toggle (`Tab` key) switches between local and remote panels
- `pane_inner()` computes render area by subtracting tab bar and shortcut bar

## Constraints

- No external SSH libraries (ssh2, russh). Must use system binaries only.
- Must work on both Windows (ConPTY) and Linux.
- ConPTY on Windows has known quirks: spurious SIGWINCH on mouse mode changes causes double-prompt artifacts in some remote shells. No clean fix found yet.

## Code review expectations

When asked for a code sanity check or review, go beyond lint and formatting. Review logic: are there race conditions, off-by-one errors, unreachable states, dead paths, redundant work, or things that silently fail? Look for duplicated code that should be shared, inconsistent patterns between similar modules, and places where the control flow is unnecessarily convoluted. Suggest concrete improvements, not vague advice.

## Logging conventions

Use `log` crate. Levels: `info` for lifecycle events (connect, transfer, delete), `warn` for recoverable issues (password rejected, delete failed), `error` for failures (PTY errors, spawn failures), `debug` only for internal diagnostics (state machine details, resize events).
