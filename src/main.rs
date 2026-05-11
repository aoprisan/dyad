use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod action;
mod app;
mod buffer;
mod git;
mod input;
mod install;
mod language;
mod lsp;
mod mcp;
mod proposals;
mod protocol;
mod syntax;
mod terminal;
mod theme;
mod tree;
mod tx;
mod ui;
mod view;

use app::App;
use protocol::ProtocolState;
use terminal::Guard;

#[derive(Parser)]
#[command(name = "dyad", about = "Agent-native terminal editor")]
struct Cli {
    /// File to open. Missing files are opened as an empty buffer; the file is created on save.
    #[arg(required_unless_present = "install")]
    path: Option<PathBuf>,

    /// Run as an MCP server over stdio instead of starting the TUI.
    /// JSON-RPC 2.0, line-delimited; one message per line.
    #[arg(long)]
    mcp: bool,

    /// Symlink the current binary into ~/.local/bin so `dyad` is on $PATH.
    #[arg(long)]
    install: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.install {
        return install::install();
    }
    let path = cli.path.expect("clap enforces path when --install is absent");
    if cli.mcp {
        let state = ProtocolState::open(path)?;
        return mcp::run(state);
    }
    let mut app = App::new(path)?;
    let mut guard = Guard::new()?;
    app.run(guard.terminal())
}
