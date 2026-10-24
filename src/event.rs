//! Keyboard mapping: crossterm key events to semantic UI actions.
//!
//! Keeping this separate from both the widget code and the state machine means
//! P3 can add actions without touching rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::UiAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Connect,
    /// Close the active link and open it again ("f").
    ForceReconnect,
    Disconnect,
    Refresh,
    BluetoothSettings,
    Quit,
    None,
}

/// Maps a key press to an [`Action`].
///
/// Plain characters only take effect without Ctrl/Alt so terminal shortcuts
/// (Ctrl+C, Ctrl+R, ...) never trigger list navigation by accident.
pub fn map_key(key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        return Action::Quit;
    }
    if ctrl || alt {
        return Action::None;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Enter => Action::Connect,
        KeyCode::Char('f') | KeyCode::Char('F') => Action::ForceReconnect,
        KeyCode::Char('d') | KeyCode::Char('D') => Action::Disconnect,
        KeyCode::Char('r') | KeyCode::Char('R') => Action::Refresh,
        KeyCode::Char('b') | KeyCode::Char('B') => Action::BluetoothSettings,
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Action::Quit,
        _ => Action::None,
    }
}

impl UiAction {
    /// Translates a click into the keyboard action it stands for, so mouse and
    /// keyboard share one behaviour path in `main::handle_action`.
    ///
    /// `SelectDevice` is resolved by the caller (it mutates the selection before
    /// any connect happens), hence `Action::None` here.
    pub fn to_action(self) -> Action {
        match self {
            UiAction::Connect => Action::Connect,
            UiAction::Reconnect => Action::ForceReconnect,
            UiAction::Disconnect => Action::Disconnect,
            UiAction::Refresh => Action::Refresh,
            UiAction::BluetoothSettings => Action::BluetoothSettings,
            UiAction::Quit => Action::Quit,
            UiAction::SelectDevice(_) => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn arrow_and_vim_keys_move() {
        assert_eq!(map_key(key(KeyCode::Up)), Action::Up);
        assert_eq!(map_key(key(KeyCode::Char('k'))), Action::Up);
        assert_eq!(map_key(key(KeyCode::Down)), Action::Down);
        assert_eq!(map_key(key(KeyCode::Char('j'))), Action::Down);
    }

    #[test]
    fn force_reconnect_key() {
        assert_eq!(map_key(key(KeyCode::Char('f'))), Action::ForceReconnect);
        assert_eq!(map_key(key(KeyCode::Char('F'))), Action::ForceReconnect);
        // Modified keys must not trigger a reconnect.
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Action::None
        );
    }

    #[test]
    fn quit_keys() {
        assert_eq!(map_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(map_key(key(KeyCode::Esc)), Action::Quit);
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn ctrl_and_alt_are_not_navigation() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT)),
            Action::None
        );
    }

    #[test]
    fn mouse_buttons_reuse_the_keyboard_actions() {
        assert_eq!(UiAction::Connect.to_action(), map_key(key(KeyCode::Enter)));
        assert_eq!(
            UiAction::Reconnect.to_action(),
            map_key(key(KeyCode::Char('f')))
        );
        assert_eq!(
            UiAction::Disconnect.to_action(),
            map_key(key(KeyCode::Char('d')))
        );
        assert_eq!(UiAction::Refresh.to_action(), map_key(key(KeyCode::Char('r'))));
        assert_eq!(
            UiAction::BluetoothSettings.to_action(),
            map_key(key(KeyCode::Char('b')))
        );
        assert_eq!(UiAction::Quit.to_action(), map_key(key(KeyCode::Char('q'))));
        // Row clicks only move the selection; the caller connects explicitly.
        assert_eq!(UiAction::SelectDevice(3).to_action(), Action::None);
    }
}
