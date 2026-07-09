use std::fmt;

use crossterm::event::KeyCode;
use log::{info, warn};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// KeyBinding — a single key combination
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyBinding {
    pub const fn new(code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> Self {
        Self {
            code,
            ctrl,
            alt,
            shift,
        }
    }

    /// Does this binding match the given key event?
    pub fn matches(&self, code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> bool {
        if self.ctrl != ctrl || self.alt != alt || self.shift != shift {
            return false;
        }
        match (&self.code, &code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(b),
            (a, b) => a == b,
        }
    }

    /// Like `matches`, but ignores the shift modifier. Used for navigation
    /// bindings where Shift is a secondary modifier meaning "extend selection".
    pub fn matches_ignore_shift(&self, code: KeyCode, ctrl: bool, alt: bool) -> bool {
        if self.ctrl != ctrl || self.alt != alt {
            return false;
        }
        match (&self.code, &code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(b),
            (a, b) => a == b,
        }
    }

    /// Parse a human-readable key string like `"Alt+Q"`, `"Ctrl+Shift+F1"`, `"Enter"`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty key string".into());
        }

        // Split by '+'. Handle the edge case where the key itself is '+':
        // "Alt++" splits as ["Alt", "", ""] — the trailing empty means key is "+".
        let parts: Vec<&str> = s.split('+').collect();
        let (modifier_parts, key_name) = if parts.len() >= 2 && parts.last() == Some(&"") {
            // String ends with '+', so key is '+'
            (&parts[..parts.len() - 2], "+")
        } else {
            (&parts[..parts.len() - 1], *parts.last().unwrap())
        };

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        for m in modifier_parts {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                other => return Err(format!("unknown modifier '{other}'")),
            }
        }

        let code = parse_key_name(key_name)?;

        // Shift+Tab is reported as BackTab by crossterm
        if shift && code == KeyCode::Tab {
            return Ok(Self {
                code: KeyCode::BackTab,
                ctrl,
                alt,
                shift: false,
            });
        }

        Ok(Self {
            code,
            ctrl,
            alt,
            shift,
        })
    }
}

fn parse_key_name(name: &str) -> Result<KeyCode, String> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "enter" | "return" => Ok(KeyCode::Enter),
        "tab" => Ok(KeyCode::Tab),
        "backtab" => Ok(KeyCode::BackTab),
        "backspace" => Ok(KeyCode::Backspace),
        "delete" | "del" => Ok(KeyCode::Delete),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "space" => Ok(KeyCode::Char(' ')),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        _ if lower.starts_with('f') && lower.len() >= 2 => {
            if let Ok(n) = lower[1..].parse::<u8>()
                && (1..=12).contains(&n)
            {
                return Ok(KeyCode::F(n));
            }
            // Not a valid F-key, treat as single char if applicable
            parse_single_char(name)
        }
        _ => parse_single_char(name),
    }
}

fn parse_single_char(name: &str) -> Result<KeyCode, String> {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(KeyCode::Char(c)),
        _ => Err(format!("unknown key '{name}'")),
    }
}

fn key_code_display(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift+Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

/// Display using Unicode arrows for arrow keys (matches the help overlay style).
fn key_code_display_pretty(code: &KeyCode) -> String {
    match code {
        KeyCode::Left => "\u{2190}".into(),
        KeyCode::Right => "\u{2192}".into(),
        KeyCode::Up => "\u{2191}".into(),
        KeyCode::Down => "\u{2193}".into(),
        other => key_code_display(other),
    }
}

impl fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use Unicode arrows for the pretty display
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        parts.push(key_code_display_pretty(&self.code));
        write!(f, "{}", parts.join("+"))
    }
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        KeyBinding::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Binding groups
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GlobalBindings {
    pub quit: KeyBinding,
    pub prev_tab: KeyBinding,
    pub next_tab: KeyBinding,
    pub close: KeyBinding,
    pub new_tab: KeyBinding,
    pub split_horizontal: KeyBinding,
    pub split_vertical: KeyBinding,
    pub focus_left: KeyBinding,
    pub focus_right: KeyBinding,
    pub focus_up: KeyBinding,
    pub focus_down: KeyBinding,
    pub zoom: KeyBinding,
}

impl Default for GlobalBindings {
    fn default() -> Self {
        Self {
            quit: KeyBinding::new(KeyCode::Char('q'), false, true, false),
            prev_tab: KeyBinding::new(KeyCode::Char('j'), false, true, false),
            next_tab: KeyBinding::new(KeyCode::Char('k'), false, true, false),
            close: KeyBinding::new(KeyCode::Char('w'), false, true, false),
            new_tab: KeyBinding::new(KeyCode::Char('t'), false, true, false),
            split_horizontal: KeyBinding::new(KeyCode::Char('-'), false, true, false),
            split_vertical: KeyBinding::new(KeyCode::Char('+'), false, true, false),
            focus_left: KeyBinding::new(KeyCode::Left, false, true, false),
            focus_right: KeyBinding::new(KeyCode::Right, false, true, false),
            focus_up: KeyBinding::new(KeyCode::Up, false, true, false),
            focus_down: KeyBinding::new(KeyCode::Down, false, true, false),
            zoom: KeyBinding::new(KeyCode::Char('z'), false, true, false),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConnectBindings {
    pub select_prev: KeyBinding,
    pub select_next: KeyBinding,
    pub connect: KeyBinding,
    pub browser_menu: KeyBinding,
    pub manual_connect: KeyBinding,
    pub help: KeyBinding,
}

impl Default for ConnectBindings {
    fn default() -> Self {
        Self {
            select_prev: KeyBinding::new(KeyCode::Up, false, false, false),
            select_next: KeyBinding::new(KeyCode::Down, false, false, false),
            connect: KeyBinding::new(KeyCode::Enter, false, false, false),
            browser_menu: KeyBinding::new(KeyCode::Char('b'), false, false, false),
            manual_connect: KeyBinding::new(KeyCode::Char('c'), false, false, false),
            help: KeyBinding::new(KeyCode::Char('h'), false, false, false),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BrowserBindings {
    pub toggle_focus: KeyBinding,
    pub navigate_up: KeyBinding,
    pub navigate_down: KeyBinding,
    pub scroll_left: KeyBinding,
    pub scroll_right: KeyBinding,
    pub enter: KeyBinding,
    pub go_up: KeyBinding,
    pub transfer: KeyBinding,
    pub delete: KeyBinding,
}

impl Default for BrowserBindings {
    fn default() -> Self {
        Self {
            toggle_focus: KeyBinding::new(KeyCode::Tab, false, false, false),
            navigate_up: KeyBinding::new(KeyCode::Up, false, false, false),
            navigate_down: KeyBinding::new(KeyCode::Down, false, false, false),
            scroll_left: KeyBinding::new(KeyCode::Left, false, false, false),
            scroll_right: KeyBinding::new(KeyCode::Right, false, false, false),
            enter: KeyBinding::new(KeyCode::Enter, false, false, false),
            go_up: KeyBinding::new(KeyCode::Backspace, false, false, false),
            transfer: KeyBinding::new(KeyCode::Char('t'), false, false, false),
            delete: KeyBinding::new(KeyCode::Delete, false, false, false),
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level KeyBindings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct KeyBindings {
    pub global: GlobalBindings,
    pub connect: ConnectBindings,
    pub browser: BrowserBindings,
}

// ---------------------------------------------------------------------------
// Raw deserialization structs (all fields Option for partial configs)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawConfig {
    global: Option<RawGlobal>,
    connect: Option<RawConnect>,
    browser: Option<RawBrowser>,
}

#[derive(Deserialize, Default)]
struct RawGlobal {
    quit: Option<String>,
    prev_tab: Option<String>,
    next_tab: Option<String>,
    close: Option<String>,
    new_tab: Option<String>,
    split_horizontal: Option<String>,
    split_vertical: Option<String>,
    focus_left: Option<String>,
    focus_right: Option<String>,
    focus_up: Option<String>,
    focus_down: Option<String>,
    zoom: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawConnect {
    select_prev: Option<String>,
    select_next: Option<String>,
    connect: Option<String>,
    browser_menu: Option<String>,
    manual_connect: Option<String>,
    help: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawBrowser {
    toggle_focus: Option<String>,
    navigate_up: Option<String>,
    navigate_down: Option<String>,
    scroll_left: Option<String>,
    scroll_right: Option<String>,
    enter: Option<String>,
    go_up: Option<String>,
    transfer: Option<String>,
    delete: Option<String>,
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

fn log_bindings(kb: &KeyBindings) {
    let g = &kb.global;
    let c = &kb.connect;
    let b = &kb.browser;
    info!(
        "config: global: quit={}, new_tab={}, close={}, split_h={}, split_v={}, prev_tab={}, next_tab={}, focus_left={}, focus_right={}, focus_up={}, focus_down={}, zoom={}",
        g.quit,
        g.new_tab,
        g.close,
        g.split_horizontal,
        g.split_vertical,
        g.prev_tab,
        g.next_tab,
        g.focus_left,
        g.focus_right,
        g.focus_up,
        g.focus_down,
        g.zoom,
    );
    info!(
        "config: connect: prev={}, next={}, connect={}, browser={}, manual={}, help={}",
        c.select_prev, c.select_next, c.connect, c.browser_menu, c.manual_connect, c.help
    );
    info!(
        "config: browser: focus={}, up={}, down={}, left={}, right={}, enter={}, go_up={}, transfer={}, delete={}",
        b.toggle_focus,
        b.navigate_up,
        b.navigate_down,
        b.scroll_left,
        b.scroll_right,
        b.enter,
        b.go_up,
        b.transfer,
        b.delete
    );
}

/// Try to parse a key string, logging a warning on failure and returning the default.
fn parse_or_default(field: &str, raw: &Option<String>, default: &KeyBinding) -> KeyBinding {
    match raw {
        Some(s) => match KeyBinding::parse(s) {
            Ok(kb) => kb,
            Err(e) => {
                warn!("config: invalid key for '{field}': {e} (using default)");
                default.clone()
            }
        },
        None => default.clone(),
    }
}

impl KeyBindings {
    /// Load keybindings from the config file, falling back to defaults.
    pub fn load() -> Self {
        let defaults = Self::default();

        let Some(config_dir) = dirs::config_dir() else {
            return defaults;
        };
        let path = config_dir.join("sshmux").join("config.toml");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => {
                info!("config: loaded keybindings from {}", path.display());
                c
            }
            Err(_) => {
                info!(
                    "config: no config file at {}, using defaults",
                    path.display()
                );
                return defaults;
            }
        };
        let raw: RawConfig = match toml::from_str(&content) {
            Ok(r) => r,
            Err(e) => {
                warn!("config: failed to parse {}: {e}", path.display());
                return defaults;
            }
        };

        let result = Self::merge(raw, &defaults);
        log_bindings(&result);
        result
    }

    fn merge(raw: RawConfig, defaults: &Self) -> Self {
        let dg = &defaults.global;
        let dc = &defaults.connect;
        let db = &defaults.browser;

        let rg = raw.global.unwrap_or_default();
        let rc = raw.connect.unwrap_or_default();
        let rb = raw.browser.unwrap_or_default();

        Self {
            global: GlobalBindings {
                quit: parse_or_default("global.quit", &rg.quit, &dg.quit),
                prev_tab: parse_or_default("global.prev_tab", &rg.prev_tab, &dg.prev_tab),
                next_tab: parse_or_default("global.next_tab", &rg.next_tab, &dg.next_tab),
                close: parse_or_default("global.close", &rg.close, &dg.close),
                new_tab: parse_or_default("global.new_tab", &rg.new_tab, &dg.new_tab),
                split_horizontal: parse_or_default(
                    "global.split_horizontal",
                    &rg.split_horizontal,
                    &dg.split_horizontal,
                ),
                split_vertical: parse_or_default(
                    "global.split_vertical",
                    &rg.split_vertical,
                    &dg.split_vertical,
                ),
                focus_left: parse_or_default("global.focus_left", &rg.focus_left, &dg.focus_left),
                focus_right: parse_or_default(
                    "global.focus_right",
                    &rg.focus_right,
                    &dg.focus_right,
                ),
                focus_up: parse_or_default("global.focus_up", &rg.focus_up, &dg.focus_up),
                focus_down: parse_or_default("global.focus_down", &rg.focus_down, &dg.focus_down),
                zoom: parse_or_default("global.zoom", &rg.zoom, &dg.zoom),
            },
            connect: ConnectBindings {
                select_prev: parse_or_default(
                    "connect.select_prev",
                    &rc.select_prev,
                    &dc.select_prev,
                ),
                select_next: parse_or_default(
                    "connect.select_next",
                    &rc.select_next,
                    &dc.select_next,
                ),
                connect: parse_or_default("connect.connect", &rc.connect, &dc.connect),
                browser_menu: parse_or_default(
                    "connect.browser_menu",
                    &rc.browser_menu,
                    &dc.browser_menu,
                ),
                manual_connect: parse_or_default(
                    "connect.manual_connect",
                    &rc.manual_connect,
                    &dc.manual_connect,
                ),
                help: parse_or_default("connect.help", &rc.help, &dc.help),
            },
            browser: BrowserBindings {
                toggle_focus: parse_or_default(
                    "browser.toggle_focus",
                    &rb.toggle_focus,
                    &db.toggle_focus,
                ),
                navigate_up: parse_or_default(
                    "browser.navigate_up",
                    &rb.navigate_up,
                    &db.navigate_up,
                ),
                navigate_down: parse_or_default(
                    "browser.navigate_down",
                    &rb.navigate_down,
                    &db.navigate_down,
                ),
                scroll_left: parse_or_default(
                    "browser.scroll_left",
                    &rb.scroll_left,
                    &db.scroll_left,
                ),
                scroll_right: parse_or_default(
                    "browser.scroll_right",
                    &rb.scroll_right,
                    &db.scroll_right,
                ),
                enter: parse_or_default("browser.enter", &rb.enter, &db.enter),
                go_up: parse_or_default("browser.go_up", &rb.go_up, &db.go_up),
                transfer: parse_or_default("browser.transfer", &rb.transfer, &db.transfer),
                delete: parse_or_default("browser.delete", &rb.delete, &db.delete),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

impl KeyBinding {
    /// For serialization back to config-file format (no Unicode arrows).
    pub fn to_config_string(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        parts.push(key_code_display(&self.code));
        parts.join("+")
    }
}

// ---------------------------------------------------------------------------
// Binding enumeration (for the keybinding editor overlay)
// ---------------------------------------------------------------------------

pub struct BindingEntry {
    pub group: &'static str,
    pub field: &'static str,
    pub description: &'static str,
    pub binding: KeyBinding,
}

impl KeyBindings {
    /// Returns all 27 bindings in display order, grouped by section.
    /// The key-editor constants in `pane::connect` are derived from these
    /// group sizes — a consistency test there guards against drift.
    pub fn entries(&self) -> Vec<BindingEntry> {
        let g = &self.global;
        let c = &self.connect;
        let b = &self.browser;
        vec![
            // Global
            BindingEntry {
                group: "global",
                field: "quit",
                description: "quit",
                binding: g.quit.clone(),
            },
            BindingEntry {
                group: "global",
                field: "new_tab",
                description: "new tab",
                binding: g.new_tab.clone(),
            },
            BindingEntry {
                group: "global",
                field: "close",
                description: "close pane / tab",
                binding: g.close.clone(),
            },
            BindingEntry {
                group: "global",
                field: "split_horizontal",
                description: "split top/bottom",
                binding: g.split_horizontal.clone(),
            },
            BindingEntry {
                group: "global",
                field: "split_vertical",
                description: "split left/right",
                binding: g.split_vertical.clone(),
            },
            BindingEntry {
                group: "global",
                field: "focus_left",
                description: "focus pane left",
                binding: g.focus_left.clone(),
            },
            BindingEntry {
                group: "global",
                field: "focus_right",
                description: "focus pane right",
                binding: g.focus_right.clone(),
            },
            BindingEntry {
                group: "global",
                field: "focus_up",
                description: "focus pane above",
                binding: g.focus_up.clone(),
            },
            BindingEntry {
                group: "global",
                field: "focus_down",
                description: "focus pane below",
                binding: g.focus_down.clone(),
            },
            BindingEntry {
                group: "global",
                field: "zoom",
                description: "zoom focused pane",
                binding: g.zoom.clone(),
            },
            BindingEntry {
                group: "global",
                field: "prev_tab",
                description: "previous tab",
                binding: g.prev_tab.clone(),
            },
            BindingEntry {
                group: "global",
                field: "next_tab",
                description: "next tab",
                binding: g.next_tab.clone(),
            },
            // Connect
            BindingEntry {
                group: "connect",
                field: "select_prev",
                description: "previous host",
                binding: c.select_prev.clone(),
            },
            BindingEntry {
                group: "connect",
                field: "select_next",
                description: "next host",
                binding: c.select_next.clone(),
            },
            BindingEntry {
                group: "connect",
                field: "connect",
                description: "connect",
                binding: c.connect.clone(),
            },
            BindingEntry {
                group: "connect",
                field: "browser_menu",
                description: "file browser",
                binding: c.browser_menu.clone(),
            },
            BindingEntry {
                group: "connect",
                field: "manual_connect",
                description: "manual connect",
                binding: c.manual_connect.clone(),
            },
            BindingEntry {
                group: "connect",
                field: "help",
                description: "keybindings",
                binding: c.help.clone(),
            },
            // Browser
            BindingEntry {
                group: "browser",
                field: "toggle_focus",
                description: "toggle focus",
                binding: b.toggle_focus.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "navigate_up",
                description: "navigate up",
                binding: b.navigate_up.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "navigate_down",
                description: "navigate down",
                binding: b.navigate_down.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "scroll_left",
                description: "scroll left",
                binding: b.scroll_left.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "scroll_right",
                description: "scroll right",
                binding: b.scroll_right.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "enter",
                description: "enter / open",
                binding: b.enter.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "go_up",
                description: "go up",
                binding: b.go_up.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "transfer",
                description: "transfer",
                binding: b.transfer.clone(),
            },
            BindingEntry {
                group: "browser",
                field: "delete",
                description: "delete",
                binding: b.delete.clone(),
            },
        ]
    }

    /// Update a single binding by group and field name.
    pub fn set_binding(&mut self, group: &str, field: &str, kb: KeyBinding) {
        match (group, field) {
            ("global", "quit") => self.global.quit = kb,
            ("global", "new_tab") => self.global.new_tab = kb,
            ("global", "close") => self.global.close = kb,
            ("global", "split_horizontal") => self.global.split_horizontal = kb,
            ("global", "split_vertical") => self.global.split_vertical = kb,
            ("global", "prev_tab") => self.global.prev_tab = kb,
            ("global", "next_tab") => self.global.next_tab = kb,
            ("global", "focus_left") => self.global.focus_left = kb,
            ("global", "focus_right") => self.global.focus_right = kb,
            ("global", "focus_up") => self.global.focus_up = kb,
            ("global", "focus_down") => self.global.focus_down = kb,
            ("global", "zoom") => self.global.zoom = kb,
            ("connect", "select_prev") => self.connect.select_prev = kb,
            ("connect", "select_next") => self.connect.select_next = kb,
            ("connect", "connect") => self.connect.connect = kb,
            ("connect", "browser_menu") => self.connect.browser_menu = kb,
            ("connect", "manual_connect") => self.connect.manual_connect = kb,
            ("connect", "help") => self.connect.help = kb,
            ("browser", "toggle_focus") => self.browser.toggle_focus = kb,
            ("browser", "navigate_up") => self.browser.navigate_up = kb,
            ("browser", "navigate_down") => self.browser.navigate_down = kb,
            ("browser", "scroll_left") => self.browser.scroll_left = kb,
            ("browser", "scroll_right") => self.browser.scroll_right = kb,
            ("browser", "enter") => self.browser.enter = kb,
            ("browser", "go_up") => self.browser.go_up = kb,
            ("browser", "transfer") => self.browser.transfer = kb,
            ("browser", "delete") => self.browser.delete = kb,
            _ => warn!("config: unknown binding {group}.{field}"),
        }
    }

    /// Save all keybindings to the config file.
    pub fn save(&self) -> Result<(), String> {
        let Some(config_dir) = dirs::config_dir() else {
            return Err("could not determine config directory".into());
        };
        let dir = config_dir.join("sshmux");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("could not create {}: {e}", dir.display()));
        }

        let entries = self.entries();
        let mut toml = String::new();
        let mut current_group = "";
        for entry in &entries {
            if entry.group != current_group {
                if !toml.is_empty() {
                    toml.push('\n');
                }
                toml.push_str(&format!("[{}]\n", entry.group));
                current_group = entry.group;
            }
            toml.push_str(&format!(
                "{} = \"{}\"\n",
                entry.field,
                entry.binding.to_config_string()
            ));
        }

        let path = dir.join("config.toml");
        std::fs::write(&path, &toml)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        info!("config: saved keybindings to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_char() {
        let kb = KeyBinding::parse("Q").unwrap();
        assert_eq!(kb.code, KeyCode::Char('Q'));
        assert!(!kb.ctrl && !kb.alt && !kb.shift);
    }

    #[test]
    fn parse_alt_char() {
        let kb = KeyBinding::parse("Alt+Q").unwrap();
        assert_eq!(kb.code, KeyCode::Char('Q'));
        assert!(kb.alt);
        assert!(!kb.ctrl && !kb.shift);
    }

    #[test]
    fn parse_ctrl_alt() {
        let kb = KeyBinding::parse("Ctrl+Alt+Delete").unwrap();
        assert_eq!(kb.code, KeyCode::Delete);
        assert!(kb.ctrl && kb.alt);
        assert!(!kb.shift);
    }

    #[test]
    fn parse_special_keys() {
        assert_eq!(KeyBinding::parse("Enter").unwrap().code, KeyCode::Enter);
        assert_eq!(KeyBinding::parse("Tab").unwrap().code, KeyCode::Tab);
        assert_eq!(
            KeyBinding::parse("Backspace").unwrap().code,
            KeyCode::Backspace
        );
        assert_eq!(KeyBinding::parse("Delete").unwrap().code, KeyCode::Delete);
        assert_eq!(KeyBinding::parse("Esc").unwrap().code, KeyCode::Esc);
        assert_eq!(KeyBinding::parse("Space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(KeyBinding::parse("Left").unwrap().code, KeyCode::Left);
        assert_eq!(KeyBinding::parse("Up").unwrap().code, KeyCode::Up);
        assert_eq!(KeyBinding::parse("Home").unwrap().code, KeyCode::Home);
        assert_eq!(KeyBinding::parse("PageUp").unwrap().code, KeyCode::PageUp);
    }

    #[test]
    fn parse_f_keys() {
        assert_eq!(KeyBinding::parse("F1").unwrap().code, KeyCode::F(1));
        assert_eq!(KeyBinding::parse("F12").unwrap().code, KeyCode::F(12));
        assert_eq!(KeyBinding::parse("f5").unwrap().code, KeyCode::F(5));
    }

    #[test]
    fn parse_shift_tab_becomes_backtab() {
        let kb = KeyBinding::parse("Shift+Tab").unwrap();
        assert_eq!(kb.code, KeyCode::BackTab);
        assert!(!kb.shift); // shift absorbed into BackTab
    }

    #[test]
    fn parse_plus_key() {
        let kb = KeyBinding::parse("Alt++").unwrap();
        assert_eq!(kb.code, KeyCode::Char('+'));
        assert!(kb.alt);
    }

    #[test]
    fn parse_bare_plus() {
        let kb = KeyBinding::parse("+").unwrap();
        assert_eq!(kb.code, KeyCode::Char('+'));
        assert!(!kb.alt && !kb.ctrl && !kb.shift);
    }

    #[test]
    fn parse_minus_key() {
        let kb = KeyBinding::parse("Alt+-").unwrap();
        assert_eq!(kb.code, KeyCode::Char('-'));
        assert!(kb.alt);
    }

    #[test]
    fn parse_case_insensitive_modifiers() {
        let kb = KeyBinding::parse("ctrl+alt+q").unwrap();
        assert!(kb.ctrl && kb.alt);
        assert_eq!(kb.code, KeyCode::Char('q'));
    }

    #[test]
    fn parse_empty_error() {
        assert!(KeyBinding::parse("").is_err());
    }

    #[test]
    fn parse_unknown_modifier_error() {
        assert!(KeyBinding::parse("Meta+Q").is_err());
    }

    #[test]
    fn parse_unknown_key_error() {
        assert!(KeyBinding::parse("Alt+FooBar").is_err());
    }

    #[test]
    fn matches_case_insensitive() {
        let kb = KeyBinding::parse("B").unwrap();
        assert!(kb.matches(KeyCode::Char('b'), false, false, false));
        assert!(kb.matches(KeyCode::Char('B'), false, false, false));
    }

    #[test]
    fn matches_modifiers() {
        let kb = KeyBinding::parse("Alt+Q").unwrap();
        assert!(kb.matches(KeyCode::Char('q'), false, true, false));
        assert!(!kb.matches(KeyCode::Char('q'), false, false, false)); // no alt
        assert!(!kb.matches(KeyCode::Char('q'), true, true, false)); // extra ctrl
    }

    #[test]
    fn display_roundtrip() {
        for s in &["Alt+Q", "Ctrl+A", "Enter", "F1", "Shift+Tab", "Space"] {
            let kb = KeyBinding::parse(s).unwrap();
            let config_str = kb.to_config_string();
            let kb2 = KeyBinding::parse(&config_str).unwrap();
            assert_eq!(kb, kb2, "roundtrip failed for '{s}'");
        }
    }

    #[test]
    fn display_alt_plus() {
        let kb = KeyBinding::parse("Alt++").unwrap();
        assert_eq!(kb.to_config_string(), "Alt++");
    }

    #[test]
    fn merge_partial_config() {
        let toml_str = r#"
[global]
quit = "Alt+X"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let defaults = KeyBindings::default();
        let merged = KeyBindings::merge(raw, &defaults);

        // Overridden
        assert_eq!(merged.global.quit.code, KeyCode::Char('X'));
        assert!(merged.global.quit.alt);
        // Default preserved
        assert_eq!(merged.global.new_tab, defaults.global.new_tab);
        assert_eq!(merged.connect.help, defaults.connect.help);
    }

    #[test]
    fn merge_empty_config() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let defaults = KeyBindings::default();
        let merged = KeyBindings::merge(raw, &defaults);
        assert_eq!(merged.global.quit, defaults.global.quit);
    }

    #[test]
    fn merge_invalid_key_uses_default() {
        let toml_str = r#"
[global]
quit = "Alt+InvalidKeyName"
"#;
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let defaults = KeyBindings::default();
        let merged = KeyBindings::merge(raw, &defaults);
        // Should fall back to default
        assert_eq!(merged.global.quit, defaults.global.quit);
    }
}
