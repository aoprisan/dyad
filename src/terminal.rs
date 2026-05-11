use anyhow::Result;
use ratatui::DefaultTerminal;

pub struct Guard {
    terminal: DefaultTerminal,
}

impl Guard {
    pub fn new() -> Result<Self> {
        // ratatui::try_init enables raw mode, enters the alt screen, and installs a panic hook
        // that restores the terminal before the panic message is printed.
        let terminal = ratatui::try_init()?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
