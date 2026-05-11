use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::action::Action;

pub fn map(ev: KeyEvent) -> Option<Action> {
    // Crossterm 0.29 emits Press, Repeat and Release events; treat Repeat like Press, ignore Release.
    if matches!(ev.kind, KeyEventKind::Release) {
        return None;
    }

    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);

    match ev.code {
        KeyCode::Left => Some(Action::MoveLeft),
        KeyCode::Right => Some(Action::MoveRight),
        KeyCode::Up => Some(Action::MoveUp),
        KeyCode::Down => Some(Action::MoveDown),
        KeyCode::Home => Some(Action::MoveHome),
        KeyCode::End => Some(Action::MoveEnd),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Backspace => Some(Action::DeletePrev),
        KeyCode::Delete => Some(Action::DeleteNext),
        KeyCode::Enter => Some(Action::Insert('\n')),
        KeyCode::Tab if !ctrl && !alt => Some(Action::Insert('\t')),
        // F12: IDE convention for go-to-definition. More reliable across
        // terminals than Ctrl-] (kept below for terminals that route it).
        KeyCode::F(12) => Some(Action::GoToDefinition),
        KeyCode::Char(c) => {
            if ctrl {
                match c.to_ascii_lowercase() {
                    's' => Some(Action::Save),
                    'q' => Some(Action::Quit),
                    // Ctrl-G ("go") is the primary go-to-definition key —
                    // single-letter Ctrl bindings route cleanly on macOS
                    // terminals where F12 needs `fn` and Ctrl-] often
                    // loses its modifier.
                    'g' => Some(Action::GoToDefinition),
                    ']' => Some(Action::GoToDefinition),
                    // Ctrl-O ("older") — vim convention for the back side
                    // of the navigation stack.
                    'o' => Some(Action::GoBack),
                    _ => None,
                }
            } else if alt {
                match c.to_ascii_lowercase() {
                    'h' => Some(Action::MoveLeft),
                    'l' => Some(Action::MoveRight),
                    'k' => Some(Action::MoveUp),
                    'j' => Some(Action::MoveDown),
                    _ => None,
                }
            } else {
                Some(Action::Insert(c))
            }
        }
        _ => None,
    }
}
