# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build --release        # Release binary at target/release/sshmux
cargo build                  # Debug build
cargo test --lib             # Run all tests (~373 tests, inline in source files)
cargo test pane::tests       # Run tests for a specific module
cargo test test_name         # Run a single test by name
cargo clippy --release       # Lint — keep at zero warnings
cargo fmt                    # Format — run before every commit
```

### Integration tests (Docker)

Integration tests are `#[ignore]`d so `cargo test` skips them. They require a Docker SSH server:

```bash
cd tests/docker && docker compose up -d --build --wait   # start container
cargo test -- --ignored                                    # run integration tests
cd tests/docker && docker compose down                     # stop container
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

Both browsers (`FileBrowser` in browser/sftp.rs, `SshBrowser` in browser/ssh.rs) implement the `Browser` trait (browser/common.rs) and hold a `BrowserCore` field that provides shared state, dual-panel rendering, local navigation, mouse handling, and the common key dispatch via `handle_browser_key()`. Browser-specific logic (SFTP commands, SCP process spawning, password prompts) stays on the outer struct. `Pane::as_browser_mut()` returns `&mut dyn Browser` to avoid duplicated match arms.

Both use prompt-stability detection: raw PTY buffer byte count unchanged for N ticks + expected prompt string present. They share parsing utilities from `browser/parse.rs` (ANSI stripping, `ls -la` parsing, transfer progress scraping).

**SFTP**: Detects `sftp>` prompt. Commands (`cd`, `get`, `put`, `rm`) run inside the SFTP session.

**SCP**: Sets `PS1='SSHMUX> '` after SSH auth, then detects `SSHMUX> ` prompt. Browsing uses shell commands (`ls`, `rm`, `pwd`). Transfers spawn separate `scp` processes in temporary PTYs.

### Keybindings

`keybindings.rs` defines `KeyBinding` (single key combo with code/ctrl/alt/shift) and three binding groups: `GlobalBindings` (9), `ConnectBindings` (6), `BrowserBindings` (9) — 24 total, wrapped in `KeyBindings`. Bindings load from `~/.config/sshmux/config.toml` at startup; `--reset-kb` deletes the config file to restore defaults.

The Connect pane's `KeyEditor` overlay (replacing the old Help overlay) lets users remap bindings interactively. When in capture mode (`editing: true`), `input.rs` sets `editor_capturing` to bypass global shortcuts and Alt suppression so any key combo can be captured. `KeyBindings::save()` writes changes to disk immediately.

`KeyBinding::matches()` requires exact modifier match. `matches_ignore_shift()` exists for bindings where Shift extends behavior (e.g., Shift+Up for multiselect in browsers).

### Key patterns

- `dirty: Arc<AtomicBool>` and `exited: Arc<AtomicBool>` for cross-thread state
- `raw_output: Arc<Mutex<Vec<u8>>>` — browsers scrape PTY output by reading and draining this buffer
- Connect pane has four mutually exclusive overlays: `None`, `BrowserMenu`, `ConnectInput`, `KeyEditor`
- Right-click context menu lives on `App` (not per-pane): `context_menu: Option<ContextMenu>`. Opens on right-click Down, tracks hover via Drag, executes on Up, dismissed by any keypress or resize. Right-click is intercepted before pane dispatch so it is never forwarded to remote apps.
- Browser focus toggle (`Tab` key) switches between local and remote panels
- `pane_inner()` computes render area by subtracting tab bar and shortcut bar

## Constraints

- No external SSH libraries (ssh2, russh). Must use system binaries only.
- Must work on both Windows (ConPTY) and Linux.
- ConPTY on Windows has known quirks: spurious SIGWINCH on mouse mode changes causes double-prompt artifacts in some remote shells. No clean fix found yet.

## Code quality rules

- **Zero code duplication.** When two or more types share the same logic, extract it into a trait, a shared method, or a helper function. Never write parallel match arms or if/else branches that do the same thing. Example: `FileBrowser` and `SshBrowser` both implement the `Browser` trait; `Pane::as_browser_mut()` returns `&mut dyn Browser` so callers never duplicate per-type logic.
- **Think big picture first.** Before writing new code, ask: does a shared type, trait, or helper already exist for this? Can the logic live on a common struct or behind an existing abstraction? Exhaust these options before introducing new code paths.
- **No bool-flag dispatch.** Never pass a bool to select between types. Use traits, enums, or polymorphism instead.

## Code review expectations

When asked for a code sanity check or review, go beyond lint and formatting. Review logic: are there race conditions, off-by-one errors, unreachable states, dead paths, redundant work, or things that silently fail? Look for duplicated code that should be shared, inconsistent patterns between similar modules, and places where the control flow is unnecessarily convoluted. Suggest concrete improvements, not vague advice.

## Logging conventions

Use `log` crate. Levels: `info` for lifecycle events (connect, transfer, delete), `warn` for recoverable issues (password rejected, delete failed), `error` for failures (PTY errors, spawn failures), `debug` only for internal diagnostics (state machine details, resize events).
