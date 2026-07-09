# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build --release        # Release binary at target/release/sshmux
cargo build                  # Debug build
cargo test --lib             # Run all tests (~490 tests, inline in source files)
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
# or: tests/run-integration.sh (starts container, runs tests, stops container)
```

Integration tests cover SSH sessions, SFTP browser (navigate, download, upload, delete), and SCP browser (navigate, download, upload) against a real SSH daemon.

Debug logging: `sshmux --log=LEVEL` (trace/debug/info/warn/error). Creates `sshmux-LEVEL-YYYYMMDD_HHMMSS.log` in the current directory.

## Architecture

SSH session multiplexer TUI. Uses system `ssh`, `sftp`, and `scp` binaries — no Rust SSH library.

### Core loop

`main.rs` runs a 5ms poll loop (`event::poll` with a 5ms timeout — blocking, not a busy sleep): crossterm events → `input.rs` dispatch → ratatui render. The App holds a `Vec<Tab>`, each Tab holds a tree of `Pane` nodes. All key and mouse handling lives in `input.rs`. A `TerminalGuard` (RAII drop + panic hook) restores the user's terminal on any exit path, including panics.

### Pane tree

`Pane` is a recursive enum — leaf variants (`Connect`, `Session`, `FileBrowser`, `SshBrowser`) or branch (`Split { kind, children }`). Splitting inserts a new Split node wrapping the target leaf and a new pane. Focus tracks a leaf index via depth-first traversal.

### PTY layer

`EmbeddedTerminal` (terminal.rs) sits on top of `crate::pty`, a small cross-platform abstraction. On Unix, `pty/unix.rs` is a thin wrapper around `portable_pty`. On Windows, `pty/win.rs` is a custom ConPTY backend that uses `windows-sys` directly so we can opt into the modern compatibility flags — `PSEUDOCONSOLE_INHERIT_CURSOR | RESIZE_QUIRK | WIN32_INPUT_MODE` always, plus `PASSTHROUGH_MODE` on Win11 build ≥ 22621. Passthrough makes ConPTY forward escape sequences instead of re-interpreting them, which removes a class of artifacts. Set `SSHMUX_NO_CONPTY_PASSTHROUGH=1` to disable.

A background reader thread processes output through `vt100::Parser` (screen grid + 1000-line scrollback), accumulates `raw_output` (only when `capture_raw` is true — browsers only, not interactive sessions), and replies to DSR probes. The main thread writes via `send_str()`/`send_char()`. The `PtyChannel` trait abstracts PTY I/O; `MockPty` implements it for unit tests.

Terminal state (mouse mode, application cursor, cursor visibility, alternate screen) is queried directly from `vt100::Screen` via methods on `EmbeddedTerminal` (`mouse_active()`, `app_cursor()`, `alternate_screen()`). No manual escape sequence scanning.

Resize is straightforward: `master.resize(rows, cols)` followed by `parser.screen_mut().set_size(rows, cols)`. With ConPTY's `RESIZE_QUIRK` already enabled by the backend, the child shell's own SIGWINCH redraw covers reflow — no local snapshot/replay needed.

Three PTY constructors: `ssh()` (interactive session, no raw capture), `sftp()` (hidden, for SFTP browser, captures raw), `ssh_shell()` (hidden, for SCP browser, captures raw). A fourth, `ssh_raw()`, accepts arbitrary SSH arguments for manual connections.

### Scrollback

Interactive sessions have 1000-line scrollback via `vt100::Parser`. Mouse scroll (when the remote app doesn't capture mouse) adjusts `scroll_offset` and calls `screen.set_scrollback()`. In alternate screen mode (vim, htop), scroll instead sends arrow key sequences. Any keypress resets scroll to live view. Cursor is hidden during scrollback.

### Browser state machines

Both browsers (`FileBrowser` in browser/sftp.rs, `SshBrowser` in browser/ssh.rs) implement the `Browser` trait (browser/common.rs) and hold a `BrowserCore` field that provides shared state, dual-panel rendering, local navigation, mouse handling, and the common key dispatch via `handle_browser_key()`. Browser-specific logic (SFTP commands, SCP process spawning, password prompts) stays on the outer struct. `Pane::as_browser_mut()` returns `&mut dyn Browser` to avoid duplicated match arms.

Both use prompt-stability detection: raw PTY buffer byte count unchanged for N ticks + expected prompt string present. They share parsing utilities from `browser/parse.rs` (ANSI stripping, `ls -la` parsing, transfer progress scraping).

**SFTP**: Detects `sftp>` prompt. Commands (`cd`, `get`, `put`, `rm`) run inside the SFTP session. Transfer completion checks output for error text before reporting success.

**SCP**: States `Connecting → SettingPrompt → WaitingPwd → WaitingLs → Idle`. Sets `PS1='SSHMUX> '` after SSH auth, then detects `SSHMUX> ` prompt. Browsing uses shell commands (`ls`, `rm`, `pwd`). Transfers spawn separate `scp` processes in temporary PTYs and check the scp exit code on completion. A password sub-state machine (`waiting_password`, `saved_password`, `password_prompts_seen`) handles SSH and SCP password prompts, auto-replays the saved password, and restarts a dropped transfer after a password retry. The prompt-count comparison relies on the raw buffer NOT being drained during `Connecting` — never reset `password_prompts_seen` without also draining. `Connecting`/`SettingPrompt` share the 30s command timeout (refreshed by connect-phase keystrokes and password submissions) so undetectable shell prompts fail with a hint instead of hanging.

Transfer queues (`transfer.pending`) always carry an explicit `pending_direction`, recorded when the queue is filled — chaining never infers direction from past transfers. The in-flight transfer is tracked in `transfer.current` for restart-after-password-retry.

### Keybindings

`keybindings.rs` defines `KeyBinding` (single key combo with code/ctrl/alt/shift) and three binding groups: `GlobalBindings` (12), `ConnectBindings` (6), `BrowserBindings` (10) — 28 total, wrapped in `KeyBindings`. The browser's `enter_or_transfer` (Space) is context-aware: it enters directories and transfers files (upload from the local panel, download from the remote one). Bindings load from `~/.config/sshmux/config.toml` at startup; `--reset-kb` deletes the config file to restore defaults. When adding a binding, update the group struct, its `Default`, the `Raw*` struct, `merge`, `entries()`, `set_binding()`, and the editor constants in `pane/connect.rs` — a consistency test (`editor_constants_match_binding_entries`) fails if the constants drift.

The Connect pane's `KeyEditor` overlay (replacing the old Help overlay) lets users remap bindings interactively. When in capture mode (`editing: true`), `input.rs` sets `editor_capturing` to bypass global shortcuts and Alt suppression so any key combo can be captured. `KeyBindings::save()` writes changes to disk immediately.

`KeyBinding::matches()` requires exact modifier match. `matches_ignore_shift()` exists for bindings where Shift extends behavior (e.g., Shift+Up for multiselect in browsers).

### Key patterns

- `dirty: Arc<AtomicBool>` and `exited: Arc<AtomicBool>` for cross-thread state
- `raw_output: Arc<Mutex<Vec<u8>>>` — browsers scrape PTY output by reading and draining this buffer
- Connect pane has four mutually exclusive overlays: `None`, `BrowserMenu`, `ConnectInput`, `KeyEditor`
- Right-click context menu lives on `App` (not per-pane): `context_menu: Option<ContextMenu>`. Opens on right-click Down, tracks hover via Drag, executes on Up, dismissed by any keypress or resize. Right-click is intercepted before pane dispatch so it is never forwarded to remote apps.
- Browser focus toggle (`Tab` key) switches between local and remote panels
- `pane_inner()` computes render area by subtracting tab bar and shortcut bar
- Exited panes (session or browser) share one exit overlay: `render_exit_overlay()` (pane/mod.rs) draws it, `handle_exit_overlay_key()` (input.rs) handles Reconnect/Close. The key handler MUST run before `handle_browser_key_dispatch` — the browser path consumes every key on browser panes and would make the overlay unreachable.
- `App::close_focused_or_tab()` is the single close path (global shortcut, context menu, exit overlays)

## Constraints

- No external SSH libraries (ssh2, russh). Must use system binaries only.
- Must work on both Windows (custom ConPTY backend in `pty/win.rs`) and Linux (`portable_pty` via `pty/unix.rs`).

## Code quality rules

- **Zero code duplication.** When two or more types share the same logic, extract it into a trait, a shared method, or a helper function. Never write parallel match arms or if/else branches that do the same thing. Example: `FileBrowser` and `SshBrowser` both implement the `Browser` trait; `Pane::as_browser_mut()` returns `&mut dyn Browser` so callers never duplicate per-type logic.
- **Think big picture first.** Before writing new code, ask: does a shared type, trait, or helper already exist for this? Can the logic live on a common struct or behind an existing abstraction? Exhaust these options before introducing new code paths.
- **No bool-flag dispatch.** Never pass a bool to select between types. Use traits, enums, or polymorphism instead.
- **Always import at the top.** Never use inline `crate::module::Item` paths inside function bodies or match arms. Always add items to the `use` block at the top of the file.

## Code review expectations

When asked for a code sanity check or review, go beyond lint and formatting. Review logic: are there race conditions, off-by-one errors, unreachable states, dead paths, redundant work, or things that silently fail? Look for duplicated code that should be shared, inconsistent patterns between similar modules, and places where the control flow is unnecessarily convoluted. Suggest concrete improvements, not vague advice.

## Logging conventions

Use `log` crate. Levels: `info` for lifecycle events (connect, transfer, delete), `warn` for recoverable issues (password rejected, delete failed), `error` for failures (PTY errors, spawn failures), `debug` only for internal diagnostics (state machine details, resize events).
