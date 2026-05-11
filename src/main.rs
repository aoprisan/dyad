use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod action;
mod app;
mod buffer;
mod input;
mod syntax;
mod terminal;
mod ui;
mod view;

use app::App;
use terminal::Guard;

#[derive(Parser)]
#[command(name = "dyad", about = "Agent-native terminal editor (Phase 1)")]
struct Cli {
    /// File to open. Missing files are opened as an empty buffer; the file is created on save.
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut app = App::new(cli.path)?;
    let mut guard = Guard::new()?;
    app.run(guard.terminal())
}
