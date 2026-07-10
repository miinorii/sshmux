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
// Binding table — the single source of truth
// ---------------------------------------------------------------------------

/// One row per binding: field name, key-editor description, default combo
/// `(code, ctrl, alt, shift)`. From this table the macro generates the group
/// structs and their `Default`s, the `Raw*` config structs, `KeyBindings`,
/// `RawConfig`, `merge`, `entries`, `set_binding`, and [`GROUP_SIZES`] (used
/// by the key-editor layout in `pane::connect`). Row order is the key
/// editor's display order. Adding a binding is adding one row.
macro_rules! define_bindings {
    (
        $(
            $group:ident / $GroupStruct:ident / $RawStruct:ident {
                $( $field:ident : $desc:literal =>
                    ($code:expr, $ctrl:literal, $alt:literal, $shift:literal) ),* $(,)?
            }
        )*
    ) => {
        $(
            #[derive(Clone, Debug)]
            pub struct $GroupStruct {
                $( pub $field: KeyBinding, )*
            }

            impl Default for $GroupStruct {
                fn default() -> Self {
                    Self {
                        $( $field: KeyBinding::new($code, $ctrl, $alt, $shift), )*
                    }
                }
            }

            #[derive(Deserialize, Default)]
            struct $RawStruct {
                $( $field: Option<String>, )*
            }
        )*

        #[derive(Clone, Debug, Default)]
        pub struct KeyBindings {
            $( pub $group: $GroupStruct, )*
        }

        /// Raw config-file shape: every group and field optional, so partial
        /// configs merge over the defaults.
        #[derive(Deserialize, Default)]
        struct RawConfig {
            $( $group: Option<$RawStruct>, )*
        }

        /// Number of bindings per group, in declaration order.
        pub const GROUP_SIZES: &[usize] = &[
            $( [ $( stringify!($field) ),* ].len() ),*
        ];

        impl KeyBindings {
            fn merge(raw: RawConfig, defaults: &Self) -> Self {
                Self {
                    $(
                        $group: {
                            let r = raw.$group.unwrap_or_default();
                            $GroupStruct {
                                $(
                                    $field: parse_or_default(
                                        concat!(stringify!($group), ".", stringify!($field)),
                                        &r.$field,
                                        &defaults.$group.$field,
                                    ),
                                )*
                            }
                        },
                    )*
                }
            }

            /// All bindings in table order — the key editor's display order.
            pub fn entries(&self) -> Vec<BindingEntry<'_>> {
                vec![
                    $($(
                        BindingEntry {
                            group: stringify!($group),
                            field: stringify!($field),
                            description: $desc,
                            binding: &self.$group.$field,
                        },
                    )*)*
                ]
            }

            /// Update a single binding by group and field name.
            pub fn set_binding(&mut self, group: &str, field: &str, kb: KeyBinding) {
                match (group, field) {
                    $($(
                        (stringify!($group), stringify!($field)) => self.$group.$field = kb,
                    )*)*
                    _ => warn!("config: unknown binding {group}.{field}"),
                }
            }
        }
    };
}

define_bindings! {
    global / GlobalBindings / RawGlobal {
        quit: "quit" => (KeyCode::Char('q'), false, true, false),
        new_tab: "new tab" => (KeyCode::Char('t'), false, true, false),
        close: "close pane / tab" => (KeyCode::Char('w'), false, true, false),
        split_horizontal: "split top/bottom" => (KeyCode::Char('-'), false, true, false),
        split_vertical: "split left/right" => (KeyCode::Char('+'), false, true, false),
        focus_left: "focus pane left" => (KeyCode::Left, false, true, false),
        focus_right: "focus pane right" => (KeyCode::Right, false, true, false),
        focus_up: "focus pane above" => (KeyCode::Up, false, true, false),
        focus_down: "focus pane below" => (KeyCode::Down, false, true, false),
        zoom: "zoom focused pane" => (KeyCode::Char('z'), false, true, false),
        prev_tab: "previous tab" => (KeyCode::Char('j'), false, true, false),
        next_tab: "next tab" => (KeyCode::Char('k'), false, true, false),
    }
    connect / ConnectBindings / RawConnect {
        select_prev: "previous host" => (KeyCode::Up, false, false, false),
        select_next: "next host" => (KeyCode::Down, false, false, false),
        connect: "connect" => (KeyCode::Enter, false, false, false),
        browser_menu: "file browser" => (KeyCode::Char('b'), false, false, false),
        manual_connect: "manual connect" => (KeyCode::Char('c'), false, false, false),
        help: "keybindings" => (KeyCode::Char('h'), false, false, false),
    }
    browser / BrowserBindings / RawBrowser {
        toggle_focus: "toggle focus" => (KeyCode::Tab, false, false, false),
        navigate_up: "navigate up" => (KeyCode::Up, false, false, false),
        navigate_down: "navigate down" => (KeyCode::Down, false, false, false),
        scroll_left: "scroll left" => (KeyCode::Left, false, false, false),
        scroll_right: "scroll right" => (KeyCode::Right, false, false, false),
        enter: "enter / open" => (KeyCode::Enter, false, false, false),
        enter_or_transfer: "enter dir / transfer file" => (KeyCode::Char(' '), false, false, false),
        go_up: "go up" => (KeyCode::Backspace, false, false, false),
        transfer: "transfer" => (KeyCode::Char('t'), false, false, false),
        delete: "delete" => (KeyCode::Delete, false, false, false),
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Log one line per group with every binding, derived from the table.
fn log_bindings(kb: &KeyBindings) {
    let mut line = String::new();
    let mut group = "";
    for e in kb.entries() {
        if e.group != group {
            if !line.is_empty() {
                info!("{line}");
            }
            line = format!("config: {}:", e.group);
            group = e.group;
        }
        line.push_str(&format!(" {}={}", e.field, e.binding));
    }
    if !line.is_empty() {
        info!("{line}");
    }
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

/// One row of the binding table, as shown in the key editor. `binding`
/// borrows from the `KeyBindings` it was enumerated from.
pub struct BindingEntry<'a> {
    pub group: &'static str,
    pub field: &'static str,
    pub description: &'static str,
    pub binding: &'a KeyBinding,
}

impl KeyBindings {
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

    /// The binding metadata is enumerated by hand in several places (struct
    /// fields, `Raw*` config structs, `merge`, `entries`, `set_binding`).
    /// These two roundtrips fail if any of them misses a binding, so drift
    /// cannot land silently.
    #[test]
    fn set_binding_covers_every_entry() {
        let probe = KeyBinding::parse("Ctrl+F9").unwrap();
        let mut kb = KeyBindings::default();
        for e in KeyBindings::default().entries() {
            kb.set_binding(e.group, e.field, probe.clone());
        }
        for e in kb.entries() {
            assert_eq!(
                e.binding, &probe,
                "set_binding has no arm for {}.{}",
                e.group, e.field
            );
        }
    }

    #[test]
    fn merge_covers_every_entry() {
        // Build a config overriding every field; a binding missing from a
        // Raw* struct is silently ignored by serde and stays at its default.
        let defaults = KeyBindings::default();
        let entries = defaults.entries();
        let mut toml_src = String::new();
        let mut group = "";
        for e in &entries {
            if e.group != group {
                toml_src.push_str(&format!("[{}]\n", e.group));
                group = e.group;
            }
            toml_src.push_str(&format!("{} = \"Ctrl+F9\"\n", e.field));
        }
        let raw: RawConfig = toml::from_str(&toml_src).unwrap();
        let merged = KeyBindings::merge(raw, &KeyBindings::default());
        let probe = KeyBinding::parse("Ctrl+F9").unwrap();
        for e in merged.entries() {
            assert_eq!(
                e.binding, &probe,
                "merge/RawConfig misses {}.{}",
                e.group, e.field
            );
        }
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
