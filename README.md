# sshmux

![sshmux demo](demo.png)

SSH session multiplexer that runs inside your local terminal. Tabs, split panes, and a two-panel SFTP file browser, all driven by the system `ssh` and `sftp` binaries — no additional SSH library dependency.

> This project started as a personal vibecoded tool to manage an ever-growing list of SSH connections at work. It solved the problem well enough that it felt worth sharing. It is not a polished product — use it as a starting point, adapt it freely, and contribute back if you find it useful.

---

## SSH config

Hosts are read from `~/.ssh/config` at startup. Any non-wildcard `Host` entry is listed in the connect pane. The `ssh` and `sftp` binaries inherit the full system environment including SSH agent, `~/.ssh/config` options, and jump hosts.

## Keybindings

### Global (work in any pane)

| Key | Action |
|---|---|
| `Alt+T` | New tab |
| `Alt+W` | Close focused pane (closes tab if last pane) |
| `Alt+-` | Split pane vertically (top / bottom) |
| `Alt++` | Split pane horizontally (left / right) |
| `Alt+B` | Open SFTP file browser for selected host |
| `Alt+↑` / `Alt+↓` | Cycle focus between panes |
| `Alt+←` / `Alt+→` | Switch tabs |
| `Ctrl+C` | Quit |

### Connect pane

| Key | Action |
|---|---|
| `↑` / `k` | Select previous host |
| `↓` / `j` | Select next host |
| `Enter` | Open SSH session |
| `Alt+B` | Open SFTP browser |

### Session pane (SSH)

Standard terminal input. Notable mappings:

| Key | Sent |
|---|---|
| `Ctrl+<letter>` | C0 control code |
| `Ctrl+Arrow` | `ESC[1;5D/C/A/B` (word navigation) |
| `Backspace` | `0x7f` |
| `F1`–`F12` | xterm sequences |

Mouse events forwarded as SGR sequences when the remote app enables mouse reporting.

### File browser pane

| Key | Action |
|---|---|
| `Tab` | Toggle local / remote panel focus |
| `↑` / `↓` | Navigate entries |
| `Space` / `Enter` | Enter directory; download if file (remote) |
| `Backspace` | Go up one directory |
| `F5` | Download selected remote file to local directory |
| `F6` | Upload selected local file to remote directory |
| `Delete` | Delete focused file (confirmation required) |
| `y` | Confirm deletion |
| `n` / `Esc` | Cancel deletion |

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   sshmux process                        │
│                                                         │
│  main loop (5 ms poll)                                  │
│  ┌──────────────┐    ┌──────────────────────────────┐   │
│  │ crossterm    │    │ App                          │   │
│  │ event input  │───>│  Vec<Tab>                    │   │
│  └──────────────┘    │    Tab                       │   │
│                      │      Pane (tree)             │   │
│  ┌──────────────┐    │        Connect               │   │
│  │ ratatui      │<───│        Session               │   │
│  │ draw buffer  │    │        FileBrowser           │   │
│  └──────────────┘    │        Split{H|V, children}  │   │
│                      └──────────────────────────────┘   │
│                                                         │
│  Per Session / FileBrowser:                             │
│                                                         │
│  main thread             reader thread                  │
│  ┌─────────────┐         ┌─────────────────────────┐    │
│  │ send_str()  │         │ reader.read()           │    │
│  │     │       │         │   │                     │    │
│  │     v       │         │   ├─> vt100::Parser     │    │
│  │  writer     │         │   │     (screen grid)   │    │
│  │  (Mutex)    │         │   ├─> raw_output Vec<u8>│    │
│  └──────┬──────┘         │   │     SFTP scraping   │    │
│         │                │   ├─> dirty AtomicBool  │    │
│         │                │   ├─> mouse_active      │    │
│         │                │   └─> cursor_visible    │    │
│         │                └─────────────┬───────────┘    │
│         │                              │                │
│         v                              v                │
│  ┌──────────────────────────────────────────────────┐   │
│  │          portable_pty  (PTY master)              │   │
│  │          PTY slave fd                            │   │
│  └───────────────────────────┬──────────────────────┘   │
│                              │  spawn                   │
└──────────────────────────────┼──────────────────────────┘
                               │
               ┌───────────────┴────────────────┐
               │                                │
         ┌─────v──────┐                  ┌──────v─────┐
         │  ssh host  │                  │ sftp host  │
         │  (Session) │                  │(FileBrowser│
         │            │                  │ hidden PTY)│
         └────────────┘                  └────────────┘
```

### PTY data flow (Session pane)

```
keystroke
    │
    v
crossterm Event::Key
    │
    v
send_str() / send_char()
    │  write bytes
    v
PTY master writer ───────────────────────────────────┐
                                                     │ PTY slave stdin
                                               ┌─────v─────┐
                                               │  ssh(1)   │
                                               │  process  │
                                               └─────┬─────┘
                                                     │ PTY slave stdout
PTY master reader <──────────────────────────────────┘
    │
    ├─> vt100::Parser::process(bytes)
    │        └─> screen grid updated
    │
    ├─> raw_output.extend(bytes)      (SFTP only)
    │
    ├─> dirty.store(true)            ──> triggers ratatui redraw
    │
    ├─> scan ESC[?...h/l             ──> mouse_active / cursor_visible
    │
    └─> reply to DSR (ESC[6n)        ──> neovim/htop cursor probe
```

### SFTP state machine (FileBrowser)

```
Connecting
    │  prompt stable × 3 ticks
    v
WaitingPwd ── send "pwd\r\n"
    │  prompt stable
    v
WaitingLs ── send "ls -la\r\n"
    │  prompt stable, parse_ls()
    v
Idle <──────────────────────────────────────────────┐
    │                                               │
    ├── cd dir ──> WaitingCd ──> WaitingPwd ──> WaitingLs
    │
    ├── get/put ──> Transferring ──> WaitingLs ─────┘
    │
    └── rm ──> WaitingDelete ──> WaitingLs ─────────┘
```

"Stable" means the raw PTY buffer byte count has not changed for 3 consecutive ticks (~15 ms) and the last non-empty line contains `sftp>`. This prevents acting on a prompt that appears mid-output before all data has been flushed.

---

## Build

```
cargo build --release
```

Binary: `target/release/sshmux`

Optional debug logging to `debug.log`:

```
sshmux --debug
```