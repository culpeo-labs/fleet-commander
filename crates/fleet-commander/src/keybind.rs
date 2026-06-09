//! Parsing and matching of keybindings.
//!
//! Bindings are written as strings in the config: a bare key (`"h"`, `"Tab"`,
//! `"Enter"`) optionally prefixed by one or more modifier chunks (`"C-"` for
//! Ctrl, `"S-"` for Shift, `"M-"` for Alt/Meta).
//!
//! A single uppercase ASCII letter automatically implies the Shift modifier
//! so that `"H"` matches the same event crossterm emits for Shift+H.

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Binding {
    #[cfg(test)]
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    /// True when the binding describes exactly this key event.
    ///
    /// Modifier-aware (`Ctrl-q` will not match plain `q`) and case-sensitive
    /// for `Char` (`q` will not match `Q`).
    pub fn matches(&self, event: &KeyEvent) -> bool {
        self.code == event.code && self.mods == event.modifiers
    }
}

impl FromStr for Binding {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        let mut mods = KeyModifiers::NONE;
        let mut rest = input;
        loop {
            if let Some(r) = rest.strip_prefix("C-") {
                mods |= KeyModifiers::CONTROL;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("S-") {
                mods |= KeyModifiers::SHIFT;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("M-") {
                mods |= KeyModifiers::ALT;
                rest = r;
            } else {
                break;
            }
        }

        let code = match rest {
            "Tab" => KeyCode::Tab,
            "BackTab" => KeyCode::BackTab,
            "Enter" => KeyCode::Enter,
            "Esc" => KeyCode::Esc,
            "Backspace" => KeyCode::Backspace,
            "Delete" => KeyCode::Delete,
            "Space" => KeyCode::Char(' '),
            "Left" => KeyCode::Left,
            "Right" => KeyCode::Right,
            "Up" => KeyCode::Up,
            "Down" => KeyCode::Down,
            "Home" => KeyCode::Home,
            "End" => KeyCode::End,
            "PageUp" => KeyCode::PageUp,
            "PageDown" => KeyCode::PageDown,
            other if other.chars().count() == 1 => {
                let ch = other.chars().next().unwrap();
                if ch.is_ascii_uppercase() {
                    mods |= KeyModifiers::SHIFT;
                }
                KeyCode::Char(ch)
            }
            other => return Err(anyhow!("unknown key in binding: {other:?}")),
        };

        Ok(Binding { code, mods })
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Binding::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_plain_char() {
        let b: Binding = "h".parse().unwrap();
        assert_eq!(b, Binding::new(KeyCode::Char('h'), KeyModifiers::NONE));
    }

    #[test]
    fn parses_uppercase_char_as_shifted() {
        let b: Binding = "H".parse().unwrap();
        assert_eq!(b, Binding::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
    }

    #[test]
    fn parses_control_modifier() {
        let b: Binding = "C-q".parse().unwrap();
        assert_eq!(b, Binding::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    }

    #[test]
    fn parses_special_keys() {
        assert_eq!(
            "Tab".parse::<Binding>().unwrap(),
            Binding::new(KeyCode::Tab, KeyModifiers::NONE)
        );
        assert_eq!(
            "S-Tab".parse::<Binding>().unwrap(),
            Binding::new(KeyCode::Tab, KeyModifiers::SHIFT)
        );
        assert_eq!(
            "Esc".parse::<Binding>().unwrap(),
            Binding::new(KeyCode::Esc, KeyModifiers::NONE)
        );
    }

    #[test]
    fn rejects_unknown_key() {
        assert!("Frobnicate".parse::<Binding>().is_err());
    }

    #[test]
    fn matches_exact_event() {
        let b: Binding = "h".parse().unwrap();
        assert!(b.matches(&key(KeyCode::Char('h'), KeyModifiers::NONE)));
    }

    #[test]
    fn does_not_match_when_modifier_differs() {
        let b: Binding = "h".parse().unwrap();
        assert!(!b.matches(&key(KeyCode::Char('h'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn does_not_case_fold_char() {
        let b: Binding = "h".parse().unwrap();
        assert!(!b.matches(&key(KeyCode::Char('H'), KeyModifiers::SHIFT)));
        assert!(!b.matches(&key(KeyCode::Char('H'), KeyModifiers::NONE)));
    }

    #[test]
    fn ctrl_q_does_not_match_bare_q() {
        let q: Binding = "q".parse().unwrap();
        let ctrl_q: Binding = "C-q".parse().unwrap();
        let event = key(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(!q.matches(&event));
        assert!(ctrl_q.matches(&event));
    }
}
