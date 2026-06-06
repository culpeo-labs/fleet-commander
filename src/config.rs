//! Application configuration loaded from TOML.
//!
//! Missing fields in the user's config fall back to the built-in defaults so
//! that partial configs don't wipe out the rest of the bindings. Parse errors
//! are surfaced to the caller rather than silently swallowed.

use anyhow::{Context, Result};
use crossterm::event::KeyEvent;
use serde::Deserialize;
use std::path::Path;

use crate::keybind::Binding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    Up,
    Down,
    Left,
    Right,
    Activate,
    Back,
    TogglePane,
    DismissPane,
    Insert,
    Command,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bindings: Bindings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bindings {
    #[serde(default = "default_quit")]
    pub quit: Vec<Binding>,
    #[serde(default = "default_up")]
    pub up: Vec<Binding>,
    #[serde(default = "default_down")]
    pub down: Vec<Binding>,
    #[serde(default = "default_left")]
    pub left: Vec<Binding>,
    #[serde(default = "default_right")]
    pub right: Vec<Binding>,
    #[serde(default = "default_activate")]
    pub activate: Vec<Binding>,
    #[serde(default = "default_back")]
    pub back: Vec<Binding>,
    #[serde(default = "default_toggle_pane")]
    pub toggle_pane: Vec<Binding>,
    #[serde(default = "default_dismiss_pane")]
    pub dismiss_pane: Vec<Binding>,
    #[serde(default = "default_insert")]
    pub insert: Vec<Binding>,
    #[serde(default = "default_command")]
    pub command: Vec<Binding>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            quit: default_quit(),
            up: default_up(),
            down: default_down(),
            left: default_left(),
            right: default_right(),
            activate: default_activate(),
            back: default_back(),
            toggle_pane: default_toggle_pane(),
            dismiss_pane: default_dismiss_pane(),
            insert: default_insert(),
            command: default_command(),
        }
    }
}

impl Bindings {
    /// Returns the first action whose configured bindings match this key event.
    pub fn action_for(&self, event: &KeyEvent) -> Option<Action> {
        for (action, bindings) in self.entries() {
            if bindings.iter().any(|b| b.matches(event)) {
                return Some(action);
            }
        }
        None
    }

    fn entries(&self) -> impl Iterator<Item = (Action, &[Binding])> {
        [
            (Action::Quit, self.quit.as_slice()),
            (Action::Up, self.up.as_slice()),
            (Action::Down, self.down.as_slice()),
            (Action::Left, self.left.as_slice()),
            (Action::Right, self.right.as_slice()),
            (Action::Activate, self.activate.as_slice()),
            (Action::Back, self.back.as_slice()),
            (Action::TogglePane, self.toggle_pane.as_slice()),
            (Action::DismissPane, self.dismiss_pane.as_slice()),
            (Action::Insert, self.insert.as_slice()),
            (Action::Command, self.command.as_slice()),
        ]
        .into_iter()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn from_str(input: &str) -> Result<Self> {
        toml::from_str(input).map_err(Into::into)
    }
}

fn parse_each(specs: &[&str]) -> Vec<Binding> {
    specs
        .iter()
        .map(|s| s.parse().expect("default binding parses"))
        .collect()
}

fn default_quit() -> Vec<Binding> {
    parse_each(&["q", "C-c"])
}
fn default_up() -> Vec<Binding> {
    parse_each(&["k", "Up"])
}
fn default_down() -> Vec<Binding> {
    parse_each(&["j", "Down"])
}
fn default_left() -> Vec<Binding> {
    parse_each(&["h", "Left"])
}
fn default_right() -> Vec<Binding> {
    parse_each(&["l", "Right"])
}
fn default_activate() -> Vec<Binding> {
    parse_each(&["Enter"])
}
fn default_back() -> Vec<Binding> {
    parse_each(&["Esc"])
}
fn default_toggle_pane() -> Vec<Binding> {
    parse_each(&["Tab"])
}
fn default_dismiss_pane() -> Vec<Binding> {
    parse_each(&["d"])
}
fn default_insert() -> Vec<Binding> {
    parse_each(&["i"])
}
fn default_command() -> Vec<Binding> {
    parse_each(&[":"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::from_str("").unwrap();
        assert!(
            cfg.bindings
                .quit
                .iter()
                .any(|b| b.code == KeyCode::Char('q'))
        );
    }

    #[test]
    fn partial_config_fills_unspecified_fields_from_defaults() {
        let cfg = Config::from_str(
            r#"
            [bindings]
            quit = ["Q"]
        "#,
        )
        .unwrap();
        // Override applied.
        assert_eq!(cfg.bindings.quit.len(), 1);
        assert_eq!(cfg.bindings.quit[0].code, KeyCode::Char('Q'));
        // Untouched fields still have their defaults.
        assert!(
            cfg.bindings
                .down
                .iter()
                .any(|b| b.code == KeyCode::Char('j'))
        );
        assert!(
            cfg.bindings
                .activate
                .iter()
                .any(|b| b.code == KeyCode::Enter)
        );
    }

    #[test]
    fn bad_binding_string_surfaces_error() {
        let err = Config::from_str(
            r#"
            [bindings]
            quit = ["Nope"]
        "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Nope") || msg.to_lowercase().contains("unknown key"));
    }

    #[test]
    fn malformed_toml_surfaces_error() {
        assert!(Config::from_str("[bindings\nquit = ").is_err());
    }

    #[test]
    fn action_for_returns_matching_action() {
        let bindings = Bindings::default();
        let event = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(bindings.action_for(&event), Some(Action::Down));
    }

    #[test]
    fn action_for_returns_none_when_no_binding_matches() {
        let bindings = Bindings::default();
        let event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(bindings.action_for(&event), None);
    }

    #[test]
    fn action_for_respects_modifiers() {
        let bindings = Bindings::default();
        // Plain `q` triggers Quit.
        let plain = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(bindings.action_for(&plain), Some(Action::Quit));

        // Ctrl-c is also bound to Quit; Ctrl-q is *not* (modifier-aware).
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(bindings.action_for(&ctrl_c), Some(Action::Quit));
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(bindings.action_for(&ctrl_q), None);
    }
}
