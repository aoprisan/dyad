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
                    // Page up/down for keyboards without dedicated
                    // PageUp/PageDown keys (e.g. MacBook). Mnemonic
                    // mirrors vim's Ctrl-U/Ctrl-D scroll bindings.
                    'u' => Some(Action::PageUp),
                    'd' => Some(Action::PageDown),
                    // Word jumping under Ctrl too, alongside Alt+b/f,
                    // so the user doesn't have to switch modifiers.
                    // Readline convention: b = back, f = forward.
                    'b' => Some(Action::MoveWordLeft),
                    'f' => Some(Action::MoveWordRight),
                    // Readline convention: a = start of line, e = end.
                    'a' => Some(Action::MoveHome),
                    'e' => Some(Action::MoveEnd),
                    // Ctrl-R ("review"): show the current file's diff
                    // against HEAD in a scrollable overlay.
                    'r' => Some(Action::ToggleGitDiff),
                    // Ctrl-N: prompt for a filename and create it
                    // under the tree's selected directory (or the tree
                    // root when nothing's selected / tree is closed).
                    'n' => Some(Action::NewFile),
                    // Ctrl-W ("write"): toggle autosave — buffer
                    // writes itself ~500ms after the last edit.
                    'w' => Some(Action::ToggleAutosave),
                    // Ctrl-L ("log"): toggle the commit-history view.
                    'l' => Some(Action::ToggleHistory),
                    // Ctrl-K ("kind"): LSP hover — show the type /
                    // signature of the symbol under the cursor.
                    'k' => Some(Action::ShowType),
                    // Ctrl-Y: LSP rename — prompt for the new name
                    // and apply the workspace edits to the current
                    // buffer (cross-file edits are skipped with a
                    // count in the status bar).
                    'y' => Some(Action::Rename),
                    // Ctrl-P ("palette"): show every keybinding in
                    // an overlay. Esc or Ctrl-P again closes it.
                    'p' => Some(Action::ToggleKeysHelp),
                    // Ctrl-X ("eXamine files"): fuzzy-open dialog
                    // rooted at the project root. Substring match
                    // against every non-hidden file under the root;
                    // Up/Down picks, Enter opens, Esc cancels.
                    'x' => Some(Action::OpenFile),
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
                    // Alt-T ("type"): open the workspace type-search
                    // dialog — fuzzy-match struct/enum/trait names
                    // across the workspace via LSP `workspace/symbol`.
                    't' => Some(Action::OpenTypeSearch),
                    _ => None,
                }
            } else {
                Some(Action::Insert(c))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn release_events_are_ignored() {
        let mut ev = key(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert!(map(ev).is_none());
    }

    #[test]
    fn plain_letter_inserts_itself() {
        match map(key(KeyCode::Char('x'))).unwrap() {
            Action::Insert(c) => assert_eq!(c, 'x'),
            other => panic!("expected Insert('x'), got {other:?}"),
        }
    }

    #[test]
    fn enter_inserts_newline() {
        match map(key(KeyCode::Enter)).unwrap() {
            Action::Insert(c) => assert_eq!(c, '\n'),
            other => panic!("expected Insert('\\n'), got {other:?}"),
        }
    }

    #[test]
    fn tab_inserts_tab_char() {
        match map(key(KeyCode::Tab)).unwrap() {
            Action::Insert(c) => assert_eq!(c, '\t'),
            other => panic!("expected Insert('\\t'), got {other:?}"),
        }
    }

    #[test]
    fn plain_arrows_move_one_char() {
        assert!(matches!(map(key(KeyCode::Left)).unwrap(), Action::MoveLeft));
        assert!(matches!(map(key(KeyCode::Right)).unwrap(), Action::MoveRight));
        assert!(matches!(map(key(KeyCode::Up)).unwrap(), Action::MoveUp));
        assert!(matches!(map(key(KeyCode::Down)).unwrap(), Action::MoveDown));
    }

    #[test]
    fn alt_left_right_jumps_words() {
        let ev = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        assert!(matches!(map(ev).unwrap(), Action::MoveWordLeft));
        let ev = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        assert!(matches!(map(ev).unwrap(), Action::MoveWordRight));
    }

    #[test]
    fn home_end_pageup_pagedown_map() {
        assert!(matches!(map(key(KeyCode::Home)).unwrap(), Action::MoveHome));
        assert!(matches!(map(key(KeyCode::End)).unwrap(), Action::MoveEnd));
        assert!(matches!(map(key(KeyCode::PageUp)).unwrap(), Action::PageUp));
        assert!(matches!(
            map(key(KeyCode::PageDown)).unwrap(),
            Action::PageDown
        ));
    }

    #[test]
    fn backspace_and_delete_map_to_delete_prev_next() {
        assert!(matches!(
            map(key(KeyCode::Backspace)).unwrap(),
            Action::DeletePrev
        ));
        assert!(matches!(
            map(key(KeyCode::Delete)).unwrap(),
            Action::DeleteNext
        ));
    }

    #[test]
    fn escape_maps_to_escape_action() {
        assert!(matches!(map(key(KeyCode::Esc)).unwrap(), Action::Escape));
    }

    #[test]
    fn f12_maps_to_go_to_definition() {
        assert!(matches!(
            map(key(KeyCode::F(12))).unwrap(),
            Action::GoToDefinition
        ));
    }

    #[test]
    fn ctrl_letter_bindings_route_to_their_actions() {
        let pairs: &[(char, Action)] = &[
            ('s', Action::Save),
            ('q', Action::Quit),
            ('g', Action::GoToDefinition),
            ('o', Action::GoBack),
            ('t', Action::ToggleTree),
            ('u', Action::PageUp),
            ('d', Action::PageDown),
            ('b', Action::MoveWordLeft),
            ('f', Action::MoveWordRight),
            ('a', Action::MoveHome),
            ('e', Action::MoveEnd),
            ('r', Action::ToggleGitDiff),
            ('n', Action::NewFile),
            ('w', Action::ToggleAutosave),
            ('l', Action::ToggleHistory),
            ('k', Action::ShowType),
            ('y', Action::Rename),
            ('p', Action::ToggleKeysHelp),
            ('x', Action::OpenFile),
        ];
        for (c, expected) in pairs {
            let got = map(ctrl(*c))
                .unwrap_or_else(|| panic!("Ctrl-{c} produced no action"));
            // Match by discriminant by using Debug format equality; the
            // Action enum has no PartialEq so we compare formatted form.
            assert_eq!(
                format!("{got:?}"),
                format!("{expected:?}"),
                "Ctrl-{c} routed wrong"
            );
        }
    }

    #[test]
    fn ctrl_close_bracket_also_goes_to_definition() {
        assert!(matches!(
            map(ctrl(']')).unwrap(),
            Action::GoToDefinition
        ));
    }

    #[test]
    fn alt_hjkl_act_as_directional_keys() {
        assert!(matches!(map(alt('h')).unwrap(), Action::MoveLeft));
        assert!(matches!(map(alt('l')).unwrap(), Action::MoveRight));
        assert!(matches!(map(alt('k')).unwrap(), Action::MoveUp));
        assert!(matches!(map(alt('j')).unwrap(), Action::MoveDown));
    }

    #[test]
    fn alt_b_f_jump_word() {
        assert!(matches!(map(alt('b')).unwrap(), Action::MoveWordLeft));
        assert!(matches!(map(alt('f')).unwrap(), Action::MoveWordRight));
    }

    #[test]
    fn alt_t_opens_type_search() {
        assert!(matches!(map(alt('t')).unwrap(), Action::OpenTypeSearch));
    }

    #[test]
    fn unknown_ctrl_letter_returns_none() {
        // No binding for Ctrl-Z today.
        assert!(map(ctrl('z')).is_none());
    }
}
