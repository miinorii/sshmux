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
    pub prev_pane: KeyBinding,
    pub next_pane: KeyBinding,
    pub close: KeyBinding,
    pub new_tab: KeyBinding,
    pub split_horizontal: KeyBinding,
    pub split_vertical: KeyBinding,
}

impl Default for GlobalBindings {
    fn default() -> Self {
        Self {
            quit: KeyBinding::new(KeyCode::Char('q'), false, true, false),
            prev_tab: KeyBinding::new(KeyCode::Left, false, true, false),
            next_tab: KeyBinding::new(KeyCode::Right, false, true, false),
            prev_pane: KeyBinding::new(KeyCode::Up, false, true, false),
            next_pane: KeyBinding::new(KeyCode::Down, false, true, false),
            close: KeyBinding::new(KeyCode::Char('w'), false, true, false),
            new_tab: KeyBinding::new(KeyCode::Char('t'), false, true, false),
            split_horizontal: KeyBinding::new(KeyCode::Char('-'), false, true, false),
            split_vertical: KeyBinding::new(KeyCode::Char('+'), false, true, false),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConnectBindings {
    pub select_prev: KeyBinding,
    pub select_prev_alt: KeyBinding,
    pub select_next: KeyBinding,
    pub select_next_alt: KeyBinding,
    pub connect: KeyBinding,
    pub browser_menu: KeyBinding,
    pub manual_connect: KeyBinding,
    pub help: KeyBinding,
}

impl Default for ConnectBindings {
    fn default() -> Self {
        Self {
            select_prev: KeyBinding::new(KeyCode::Up, false, false, false),
            select_prev_alt: KeyBinding::new(KeyCode::Char('k'), false, false, false),
            select_next: KeyBinding::new(KeyCode::Down, false, false, false),
            select_next_alt: KeyBinding::new(KeyCode::Char('j'), false, false, false),
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
    pub enter_alt: KeyBinding,
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
            enter_alt: KeyBinding::new(KeyCode::Char(' '), false, false, false),
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
    prev_pane: Option<String>,
    next_pane: Option<String>,
    close: Option<String>,
    new_tab: Option<String>,
    split_horizontal: Option<String>,
    split_vertical: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawConnect {
    select_prev: Option<String>,
    select_prev_alt: Option<String>,
    select_next: Option<String>,
    select_next_alt: Option<String>,
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
    enter_alt: Option<String>,
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
        "config: global: quit={}, new_tab={}, close={}, split_h={}, split_v={}, prev_tab={}, next_tab={}, prev_pane={}, next_pane={}",
        g.quit,
        g.new_tab,
        g.close,
        g.split_horizontal,
        g.split_vertical,
        g.prev_tab,
        g.next_tab,
        g.prev_pane,
        g.next_pane
    );
    info!(
        "config: connect: prev={}, prev_alt={}, next={}, next_alt={}, connect={}, browser={}, manual={}, help={}",
        c.select_prev,
        c.select_prev_alt,
        c.select_next,
        c.select_next_alt,
        c.connect,
        c.browser_menu,
        c.manual_connect,
        c.help
    );
    info!(
        "config: browser: focus={}, up={}, down={}, left={}, right={}, enter={}, enter_alt={}, go_up={}, transfer={}, delete={}",
        b.toggle_focus,
        b.navigate_up,
        b.navigate_down,
        b.scroll_left,
        b.scroll_right,
        b.enter,
        b.enter_alt,
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
                prev_pane: parse_or_default("global.prev_pane", &rg.prev_pane, &dg.prev_pane),
                next_pane: parse_or_default("global.next_pane", &rg.next_pane, &dg.next_pane),
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
            },
            connect: ConnectBindings {
                select_prev: parse_or_default(
                    "connect.select_prev",
                    &rc.select_prev,
                    &dc.select_prev,
                ),
                select_prev_alt: parse_or_default(
                    "connect.select_prev_alt",
                    &rc.select_prev_alt,
                    &dc.select_prev_alt,
                ),
                select_next: parse_or_default(
                    "connect.select_next",
                    &rc.select_next,
                    &dc.select_next,
                ),
                select_next_alt: parse_or_default(
                    "connect.select_next_alt",
                    &rc.select_next_alt,
                    &dc.select_next_alt,
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
                enter_alt: parse_or_default("browser.enter_alt", &rb.enter_alt, &db.enter_alt),
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

#[cfg(test)]
impl KeyBinding {
    /// For serialization back to config-file format (no Unicode arrows).
    fn to_config_string(&self) -> String {
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
