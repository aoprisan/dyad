use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use crate::action::Action;
use crate::buffer::Buffer;
use crate::input;
use crate::syntax::Syntax;
use crate::tx::TxManager;
use crate::ui;
use crate::view::View;

pub struct App {
    pub buffer: Buffer,
    pub view: View,
    pub syntax: Option<Syntax>,
    pub running: bool,
    pub status: String,
    tx_manager: TxManager,
    quit_pending: bool,
}

impl App {
    pub fn new(path: PathBuf) -> Result<Self> {
        let mut buffer = Buffer::open(path)?;
        let mut syntax = Syntax::for_path(buffer.path());
        if let Some(syn) = syntax.as_mut() {
            syn.refresh(&mut buffer);
        }
        Ok(Self {
            buffer,
            view: View::new(),
            syntax,
            running: true,
            status: String::new(),
            tx_manager: TxManager::new(),
            quit_pending: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while self.running {
            terminal.draw(|frame| ui::render(frame, self))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn handle_events(&mut self) -> Result<()> {
        if !event::poll(Duration::from_millis(250))? {
            return Ok(());
        }
        match event::read()? {
            Event::Key(key) => {
                if let Some(action) = input::map(key) {
                    self.apply(action)?;
                }
            }
            Event::Resize(_, _) => {
                // The next draw call will re-layout against the new size.
            }
            _ => {}
        }
        Ok(())
    }

    fn apply(&mut self, action: Action) -> Result<()> {
        // Any non-Quit input clears the pending-quit confirmation.
        if !matches!(action, Action::Quit) {
            self.quit_pending = false;
        }

        // Open an auto-tx for buffer-mutating actions so every edit lands
        // in the flat history with a human-readable intent (DESIGN.md
        // §Transactions & intent). Movement, save, and quit aren't edits
        // and don't get wrapped.
        let tx_id = action_intent(&action)
            .map(|intent| self.tx_manager.begin(intent, None, &self.buffer));
        let pre_version = tx_id.and_then(|id| self.tx_manager.pre_version(id));

        match action {
            Action::Insert(c) => {
                let idx = self.view.char_idx(&self.buffer);
                self.buffer.insert_char(idx, c);
                let mut tmp = [0u8; 4];
                let s: &str = c.encode_utf8(&mut tmp);
                self.view.after_insert(&self.buffer, s);
            }
            Action::DeletePrev => {
                let end = self.view.char_idx(&self.buffer);
                if end > 0 {
                    let start = end - 1;
                    self.buffer.delete_range(start..end);
                    self.view.after_delete_prev(&self.buffer);
                }
            }
            Action::DeleteNext => {
                let start = self.view.char_idx(&self.buffer);
                if start < self.buffer.len_chars() {
                    self.buffer.delete_range(start..start + 1);
                    // Cursor position stays the same (chars shift left).
                }
            }
            Action::MoveLeft => self.view.move_left(&self.buffer),
            Action::MoveRight => self.view.move_right(&self.buffer),
            Action::MoveUp => self.view.move_up(&self.buffer),
            Action::MoveDown => self.view.move_down(&self.buffer),
            Action::MoveHome => self.view.move_home(),
            Action::MoveEnd => self.view.move_end(&self.buffer),
            Action::PageUp | Action::PageDown => {
                // Use the most recent terminal size; ratatui exposes it via the next draw,
                // but for paging we ask crossterm directly.
                let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
                let viewport = ui::text_viewport_rows(ratatui::layout::Rect::new(0, 0, 1, rows));
                if matches!(action, Action::PageUp) {
                    self.view.page_up(&self.buffer, viewport);
                } else {
                    self.view.page_down(&self.buffer, viewport);
                }
            }
            Action::Save => match self.buffer.save() {
                Ok(bytes) => self.status = format!("Saved {} bytes", bytes),
                Err(e) => self.status = format!("Save failed: {}", e),
            },
            Action::Quit => {
                if self.buffer.is_dirty() && !self.quit_pending {
                    self.quit_pending = true;
                    self.status = "Unsaved changes — Ctrl-Q again to quit, Ctrl-S to save".into();
                } else {
                    self.running = false;
                }
            }
        }

        // Close out the auto-tx. If the mutation didn't actually change
        // the rope (e.g., DeletePrev at the start of the buffer), drop
        // it without recording a history entry — pre_version comparison
        // is the test of record because Buffer::touch bumps version on
        // every real mutation.
        if let Some(tx_id) = tx_id {
            if Some(self.buffer.version()) == pre_version {
                self.tx_manager.discard(tx_id)?;
            } else {
                self.tx_manager.commit(tx_id, &self.buffer)?;
            }
        }

        // Scroll-into-view after every action. We re-query the terminal height; the next draw
        // will adjust if it changes.
        let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let viewport = rows.saturating_sub(1); // minus status row
        self.view.scroll_into_view(viewport);

        if let Some(syn) = self.syntax.as_mut() {
            syn.refresh(&mut self.buffer);
        }

        Ok(())
    }
}

fn action_intent(action: &Action) -> Option<String> {
    match action {
        Action::Insert(c) => Some(format!("insert {}", describe_char(*c))),
        Action::DeletePrev => Some("delete backward".into()),
        Action::DeleteNext => Some("delete forward".into()),
        Action::MoveLeft
        | Action::MoveRight
        | Action::MoveUp
        | Action::MoveDown
        | Action::MoveHome
        | Action::MoveEnd
        | Action::PageUp
        | Action::PageDown
        | Action::Save
        | Action::Quit => None,
    }
}

fn describe_char(c: char) -> String {
    match c {
        '\n' => "newline".into(),
        '\t' => "tab".into(),
        ' ' => "space".into(),
        c if c.is_ascii_graphic() => format!("'{c}'"),
        c => format!("U+{:04X}", c as u32),
    }
}
