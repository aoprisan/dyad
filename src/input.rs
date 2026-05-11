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
        // Option+Left/Right on macOS: jump word boundaries (macOS
        // convention). Plain arrows still step one character.
        KeyCode::Left if alt => Some(Action::MoveWordLeft),
        KeyCode::Right if alt => Some(Action::MoveWordRight),
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
        KeyCode::Esc => Some(Action::Escape),
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
                    // Ctrl-T toggles the file-tree sidebar.
                    't' => Some(Action::ToggleTree),
                    _ => None,
                }
            } else if alt {
                match c.to_ascii_lowercase() {
                    'h' => Some(Action::MoveLeft),
                    'l' => Some(Action::MoveRight),
                    'k' => Some(Action::MoveUp),
                    'j' => Some(Action::MoveDown),
                    // Readline / macOS Terminal convention for word
                    // jumping: Option+Left is delivered as ESC b
                    // (i.e., Alt+b) by Terminal.app rather than as a
                    // modified arrow key, so we bind the letter form
                    // explicitly — Option+Left/Right above still works
                    // in terminals that do forward modified arrows.
                    'b' => Some(Action::MoveWordLeft),
                    'f' => Some(Action::MoveWordRight),
                    _ => None,
                }
            } else {
                Some(Action::Insert(c))
            }
        }
        _ => None,
    }
}
