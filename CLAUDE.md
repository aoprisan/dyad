# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`dyad` is an agent-native terminal editor written in Rust. The full design is in
`DESIGN.md` — read it before making non-trivial changes. The headline idea: the
editor is a runtime, and humans and agents are symmetric clients speaking the
same protocol. There is no privileged path for the UI vs. an agent.

The codebase has progressed through most of the phased build order in
`DESIGN.md` §Phased build order — buffer/view, Tree-sitter, transactions+intent,
MCP stdio server, `rust-analyzer` LSP, and proposals (Phase 10) are all wired
in. Multi-client awareness (Phase 8) is partial: multiple buffers per
`ProtocolState`, but no concurrent TUI+MCP session split yet.

Each source file's module doc-comment names the phase it belongs to and what's
deferred — read those before extending a module. `ROADMAP.md` tracks the
MCP/protocol features still missing for an agent client to be fully symmetric
with the TUI.

## Commands

```bash
cargo build                                 # build
cargo run -- <path>                         # open <path> in the TUI (created on save if missing)
cargo run -- <path> --mcp                   # run as an MCP server over stdio (JSON-RPC 2.0, line-delimited)
cargo run -- --install                      # symlink the built binary into ~/.local/bin
cargo clippy --all-targets -- -D warnings   # lint (must stay clean)

cargo test                                  # unit tests in src/ + integration tests in tests/
cargo build --release && scripts/mcp-smoke.sh   # extra end-to-end MCP smoke check
```

`cargo test` is the primary regression gate. `tests/mcp_integration.rs` and
`tests/buffer_io_integration.rs` spawn `dyad --mcp` as a subprocess and drive
it via JSON-RPC — that's the canonical way to test the agent-facing surface.
`scripts/mcp-smoke.sh` is a thinner shell-based smoke that predates the
integration tests and still works. The TUI is exercised manually.

## Architecture

Data flow per keystroke (TUI):

```
KeyEvent -> input::map -> Action -> App::apply -> TxManager wrap -> Buffer/View mutations -> ui::render
```

Data flow per agent call (MCP):

```
stdin JSON-RPC line -> mcp::handle_line -> ProtocolState method -> TxManager wrap -> Buffer mutation -> JSON response
```

Both paths funnel through `TxManager` and the same `Buffer` methods — that's
the "symmetric clients" invariant from `DESIGN.md`.

### Modules

- `buffer.rs` — `Buffer { rope: ropey::Rope, path, version, dirty, pending_edits }`.
  Owns the text. **Every mutation bumps `version` and sets `dirty`**; `version`
  is MCP optimistic-concurrency (every read returns one; every write must
  reference one). Method names track the protocol verbs in `DESIGN.md` §Edits
  — `insert_char`, `delete_range`, `replace_range`, `save` — so the MCP layer
  wraps them thinly; **do not rename them casually**. `pending_edits` feeds
  Tree-sitter's `Tree::edit` for incremental reparse. `BufferSnapshot` is the
  rollback primitive for transactions.
- `view.rs` — `View { cursor_line, cursor_col, sticky_col, top_line }`.
  **Borrows `Buffer`; never owns it.** Multiple views per buffer is the
  Phase 8 hook (`DESIGN.md` §Awareness / multi-client).
- `action.rs` — flat `Action` enum, no logic. Keymap and MCP handlers
  construct the same actions.
- `input.rs` — pure `KeyEvent -> Option<Action>`. **Non-modal**: letter keys
  always insert. The full keymap is documented inline; see also README.md.
- `app.rs` — the TUI runtime. `App` owns the focused `Buffer`, `View`,
  `Syntax`, `LspClient`, `TxManager`, file tree, prompts, and overlay state
  (git diff, history, fuzzy open, keys help). `App::apply(Action)` is the
  **single TUI mutation funnel** — every keystroke that touches state goes
  through it, wrapped in an auto-transaction with a synthetic intent string.
- `protocol.rs` — `ProtocolState`: the agent-facing surface. Methods are
  one-per-`DESIGN.md`-verb (`buffer_open`, `buffer_read`, `ast_query`,
  `edit_replace_range`, `edit_replace_node`, `edit_rename_symbol`, `tx_begin`,
  `tx_commit`, `history_recent`, `git_diff`, `edit_propose_range`, …). The
  TUI does **not** route through `ProtocolState` today — `App` calls
  `Buffer`/`TxManager` directly. Keep the two surfaces semantically aligned.
- `mcp.rs` — JSON-RPC 2.0 stdio transport. One tool per `ProtocolState`
  method. Implements `initialize`, `notifications/initialized`, `tools/list`,
  `tools/call`. Split into `handle_line` + I/O loop so tests drive the
  dispatcher in-process.
- `tx.rs` — `TxManager`: transactions + flat history. `begin` snapshots the
  buffer; `commit` records a `Change`; `rollback` restores from the snapshot.
  Edits without an explicit `tx.begin` get an auto-tx with a synthetic intent.
- `syntax.rs` — Tree-sitter parser + cached tree + Rust highlights query.
  `refresh` consumes the buffer's `pending_edits`, reparses incrementally,
  and produces per-line highlight spans for the renderer. Same tree backs
  `ast.query` and `edit.replace_node`.
- `language.rs` — `Language` enum + per-language descriptors (binary name,
  install hint, workspace markers, capability flags). The single source of
  truth threaded through `syntax.rs`, `lsp.rs`, and `protocol.rs`. Adding a
  third language is two steps: extend the enum, fill out the descriptor
  methods. Currently covers `Rust` (`rust-analyzer`) and `Scala` (`metals`,
  including `.sc` and `.sbt`).
- `lsp.rs` — generic LSP client driven by `Language`. Spawned lazily on first
  open of a recognized extension via `LspClient::spawn(language, …)`; shared
  across buffers in the same workspace. Reader thread updates the diagnostics
  cache; writer thread (main) sends requests. **Fail-graceful**: if the
  server binary isn't on `PATH` or initialize times out, `spawn` returns
  `Err` and everything else keeps working without LSP. Metals' first import
  is slow (>30s); the indexing-status hooks in `language.rs` drive the
  status-line indicator.
- `git.rs` — shells out to `git` for status, diff, log, stage, commit.
  Per-line `LineStatus` against `HEAD` is what the gutter renders.
- `proposals.rs` — Phase 10 queue. `edit.propose_range` enqueues; another
  client accepts/rejects. Accept runs through the same tx machinery so the
  proposal's intent string lands in flat history. Stale-version accept errors
  and re-queues under a new id.
- `tree.rs` — left-sidebar file tree. Lazy expansion, flat `entries` vector
  for fast viewport rendering.
- `theme.rs` — Solarized palette.
- `ui.rs` — single `render(frame, app)`. Layout: optional left tree sidebar,
  optional gutter (line numbers + git status), main text area, 1-row status,
  plus modal overlays (git diff, history, fuzzy open, keys help, prompt).
  No soft wrap, no horizontal scroll yet (long lines truncate).
- `terminal.rs` — RAII `Guard` around `ratatui::try_init`/`restore`. The
  `try_init` call installs a panic hook that restores the terminal before the
  panic message prints.
- `install.rs` — `dyad --install`: symlinks the current binary into
  `~/.local/bin`. Refuses to overwrite anything that isn't already a symlink,
  so it won't clobber a user-owned file.

## Invariants to preserve

- **Cursor positions are char offsets within a line, not display cells.**
  Wide Unicode chars and tab-width math are deliberately deferred. If you add
  display-cell awareness, do it in `view.rs` and `ui.rs` together; don't leak
  cell math into `buffer.rs`.
- **No privileged UI path.** New editing capability is a method on `Buffer`
  (named to match the protocol verb), wrapped in a transaction, exposed
  through both `App::apply` (TUI) and `ProtocolState` (MCP). Do not reach
  into the rope from `app.rs` or `ui.rs`.
- **Buffer/View ownership.** `Buffer` owns the rope and persistence; `View`
  owns the cursor and viewport. If it touches the rope, it's `Buffer`.
- **Every buffer mutation goes through a transaction.** `App::apply`
  auto-wraps each keystroke; `ProtocolState` auto-wraps each edit call when
  no explicit tx is open. New mutation paths must do the same so flat history
  stays complete.
- **LSP coordinates are line + UTF-16 code units** (LSP spec). Exact for
  BMP-only source; fine for current Rust use. Keep this confined to `lsp.rs`
  and the `protocol.rs` LSP-adjacent methods — don't leak UTF-16 indexing
  elsewhere.
- **`cargo clippy -D warnings` must stay clean.** New `#[allow]`s need a
  one-line justification comment naming the phase or constraint that requires
  them.
- **`#[allow(dead_code)]` markers are deliberate scaffolding** — they call
  out fields/methods consumed by a later phase (e.g. `tx::Change` fields used
  by `history.recent`). Read the justification comment before deleting.

## Cross-cutting deferrals (don't try to fix these incidentally)

These are known gaps documented in `DESIGN.md` §Dogfooding. Fixing them is a
real piece of work; don't paper over them while doing something else:

- True cross-buffer transaction atomicity (`edit.rename_symbol` runs a
  per-buffer auto-tx today).
- TUI + MCP coexistence — `clients.list` only reports the current MCP
  session; the TUI doesn't show agent cursors.
- `git.diff` reads disk, not the unsaved buffer.
- `view.*`, `symbol.references`, `symbol.signature`, `edit.extract_function`,
  `edit.add_import`, `edit.inline`, `note.pin`.

## TUI keybinding conventions

Prefer `Ctrl+<letter>` for new bindings. F-keys and `Ctrl+<symbol>` (e.g.
`Ctrl-]`) are unreliable in common macOS terminal setups, which is why
`Ctrl-G` is the primary go-to-definition binding even though `F12` is the
IDE convention.

When picking a letter, prefer a mnemonic that won't collide with readline
conventions already bound in `input.rs` (Ctrl-A/E for home/end, Ctrl-B/F for
word-jump, Ctrl-U/D for page, Ctrl-O for jumplist-back).

## When in doubt

- **What's the right name for this new edit operation?** Look at `DESIGN.md`
  §Edits — three tiers. Match the protocol verb.
- **Should this go in `Buffer` or `View`?** Buffer owns the text and
  persistence. View owns the cursor and viewport. If it touches the rope,
  it's `Buffer`.
- **Should this go in `App` or `ProtocolState`?** TUI-only state (overlays,
  prompts, tree visibility, autosave timer) is `App`. Anything an agent
  could legitimately call is `ProtocolState`, and `mcp.rs` exposes it.
- **Should I add a feature for a later phase while I'm here?** No. Each
  phase in `DESIGN.md` is its own scope. The forward-compat scaffolding
  that's already in place (`version` field, borrow-not-own discipline,
  protocol-shaped `Buffer` method names) is enough.
