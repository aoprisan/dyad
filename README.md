# dyad

An **agent-native terminal editor** in Rust. The editor is a runtime; humans
and agents are symmetric clients speaking the same protocol. There is no
privileged path for the UI vs. an agent.

This is not "an editor with AI bolted on." It's editor-as-runtime: the editor
owns buffers, AST, LSP state, and undo history, and exposes them over MCP so
an agent operates with the same primitives a human does.

The headline idea and the full protocol sketch live in [`DESIGN.md`](DESIGN.md).
Working notes for contributors (including Claude Code) are in [`CLAUDE.md`](CLAUDE.md).

## Status

Early. The TUI runs, edits a single buffer, and saves. An MCP server over
stdio exposes buffer operations to agents. Tree-sitter, LSP, and transactions
are wired in incrementally per the phased build order in `DESIGN.md`.

There are no tests yet; the editor is exercised manually.

## Quick start

```bash
cargo run -- path/to/file        # open in the TUI (created on save if missing)
cargo run -- path/to/file --mcp  # run as an MCP server over stdio (JSON-RPC 2.0, line-delimited)
```

Build and lint:

```bash
cargo build
cargo clippy --all-targets -- -D warnings
```

`cargo clippy -D warnings` must stay clean.

## Keybindings (TUI)

Non-modal — letter keys always insert.

| Key                  | Action                              |
| -------------------- | ----------------------------------- |
| Arrows / Alt+h,j,k,l | Move cursor                         |
| Ctrl-S               | Save                                |
| Ctrl-G (or F12, Ctrl-]) | Go to definition (LSP, Rust files) |
| Alt-T                | Find type (workspace symbol search) |
| Ctrl-O               | Jump back (navigation stack)        |
| Ctrl-Q               | Quit                                |

LSP-backed features (diagnostics on the status bar, go-to-definition) light up
automatically when the file is `.rs` and `rust-analyzer` is on `PATH`. They
stay dark otherwise — the editor falls back to plain text editing.

## MCP

With `--mcp`, dyad speaks JSON-RPC 2.0 line-delimited over stdio. A smoke
script lives at [`scripts/mcp-smoke.sh`](scripts/mcp-smoke.sh). The protocol
verbs mirror the names in `DESIGN.md` §Edits and §Buffers & views — `Buffer`
method names track them deliberately so the MCP layer is a thin wrapper.

## Architecture (one screen)

```
KeyEvent -> input::map -> Action -> App::apply -> Buffer/View mutations -> ui::render
```

- `buffer.rs` — owns the rope, path, version, dirty flag. Every mutation
  bumps `version` (optimistic concurrency for MCP writers).
- `view.rs` — cursor + viewport. Borrows `Buffer`; never owns it. Multi-view
  per buffer is a non-event by construction.
- `app.rs` — single mutation funnel (`App::apply(Action)`). Transactions wrap
  this; nothing else mutates state.
- `action.rs` — flat enum, no logic. Keymap and MCP handlers construct the
  same actions.
- `input.rs` / `ui.rs` / `terminal.rs` — keymap, render, and a RAII terminal
  guard that restores on panic.
- `mcp.rs` / `protocol.rs` / `tx.rs` / `syntax.rs` / `lsp.rs` / `git.rs` /
  `proposals.rs` — agent-facing surface and the integrations behind it.

See `DESIGN.md` for the rationale behind each boundary.
