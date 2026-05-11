# Agent-Native Terminal Editor — Design Notes

## The wedge

Most existing editors were designed for a human at the keyboard; AI agents
have been bolted on. There's room for an editor designed from the ground up
so that **an agent is a competent first-class user, not a guest**.

The vintage Emacs philosophy maps surprisingly well: small programmable core,
"everything is a buffer," protocol-as-public-API. We update that for 2026:
LSP, Tree-sitter, and agent clients are first-class assumptions, not afterthoughts.

This is not "AI-powered editor" (every editor has that now). It's
**editor-as-runtime**: the editor owns buffers, AST, LSP state, undo history;
humans and agents are symmetric clients speaking the same protocol.

## Tech choices

- **Language:** Rust
- **TUI:** Ratatui
- **Syntax/structural:** Tree-sitter
- **Semantic:** LSP client (start with one server: `rust-analyzer`)
- **Agent protocol:** MCP server exposing the operations below
- **Storage:** rope buffer (`ropey` crate is the obvious pick)
- **Git:** shell out to `git` initially; consider `git2` later

## Prior art worth reading

- Helix (Rust, modal, built-in LSP, no plugin system)
- Zee (Rust, Tree-sitter)
- Kibi / the kilo tutorial (minimal editor in ~1000 lines)
- Emacs server protocol (philosophical reference)

## Protocol sketch

MCP-flavored. Each operation is a tool the agent can call. The editor's own
UI uses the same operations internally — there is no privileged path.

### Buffers & views

```
buffer.open(path)                       -> buffer_id
buffer.read(buffer_id, range?)          -> {text, version}
buffer.list()                           -> [{id, path, dirty, version}]
view.focus(buffer_id, position)
view.visible_range(buffer_id)           -> range
```

Every read returns a version. Every write must reference one. Optimistic
concurrency, not file locks.

### Semantic queries

```
symbol.definition(buffer_id, position)  -> location
symbol.references(buffer_id, position)  -> [location]
symbol.signature(buffer_id, position)   -> {type, doc, params}
symbol.callers(symbol_id)               -> [location]
symbol.callees(symbol_id)               -> [location]

ast.node_at(buffer_id, position)        -> node
ast.query(buffer_id, ts_query)          -> [match]

scope.in_scope(buffer_id, position)     -> [symbol]
scope.imports(buffer_id)                -> [{module, symbols}]
```

`scope.in_scope` is the unlock: instead of "read three files to figure out
what's available," one call returns the symbol table at a cursor position.

### Edits — three tiers

Agent picks the right level. Tier 3 is preferred; tier 1 is the escape hatch.

```
# Tier 1: textual
edit.replace_range(buffer_id, version, range, text)        -> new_version

# Tier 2: structural (Tree-sitter aware)
edit.replace_node(buffer_id, version, node_id, text)       -> new_version
edit.wrap_node(buffer_id, version, node_id, before, after)
edit.insert_before_node / insert_after_node

# Tier 3: semantic (LSP / refactor-aware)
edit.rename_symbol(symbol_id, new_name)                    -> [affected_files]
edit.extract_function(buffer_id, range, name)
edit.add_import(buffer_id, module, symbol?)
edit.inline(symbol_id)
```

Tier 3 operations are atomic across files.

### Transactions & intent

```
tx.begin(intent: str, conversation_id?: str)  -> tx_id
tx.commit(tx_id)                              -> change_id
tx.rollback(tx_id)
```

Every edit happens inside a transaction with a stated intent string. The
intent is metadata on the change. This gives provenance and replayability
for free.

### History

```
history.recent(limit)             -> [{change_id, intent, author, timestamp, files}]
history.diff(change_id)           -> patch
history.replay(change_id, target) -> apply same intent elsewhere
history.tree(buffer_id)           -> change_graph (branching undo)
```

Branching undo matters when multiple agents edit in parallel.

### Diagnostics & feedback

```
diag.current(buffer_id?)   -> [{severity, range, message, source}]
diag.subscribe(callback)   # streaming
test.run(target?)          -> {passed, failed, output}
test.last_results()        -> structured results
```

Agent does not parse `cargo test` output. The editor parses once, exposes structure.

### Awareness / multi-client

```
clients.list()                  -> [{id, kind: human|agent, focus, recent_edits}]
clients.cursor(client_id)       -> {buffer, position}
clients.subscribe_edits(client_id)
```

Human and agent see each other's cursors and edits live. Critical for review.

### Conversation pins

```
note.pin(buffer_id, range, conversation_id, content)
note.list(buffer_id?)             -> [pinned_notes]
note.resolve(note_id)
```

Threads attached to code ranges. Re-anchored via Tree-sitter, not line
numbers — survives reformatting.

## Phased build order

1. Buffer + view + textual edit (boring but mandatory)
2. Tree-sitter integration; `ast.query` and `edit.replace_node`
3. Transaction wrapper with intent metadata → flat history
4. MCP server exposing the above
5. Connect Claude Code or another agent as a client — dogfood it
6. LSP for one language (Rust)
7. Tier 3 semantic edits via LSP rename/refactor
8. Multi-client awareness
9. Git gutter + diff view
10. Hunk-by-hunk review UI for incoming agent edits

Steps 1–4 are a few weekends. Step 5 is when it becomes genuinely novel.

## Dogfooding (Phase 5)

The Phase 4 MCP server is line-delimited JSON-RPC 2.0 over stdio. To
have a Claude Code session drive `dyad`, register it as a project-local
MCP server:

```jsonc
// .mcp.json (project root) or ~/.claude/mcp_settings.json (global)
{
  "mcpServers": {
    "dyad": {
      "command": "/absolute/path/to/dyad/target/release/dyad",
      "args": ["--mcp", "/absolute/path/to/the/file/to/edit.rs"]
    }
  }
}
```

After `cargo build --release` and a Claude Code reload, the tools
`buffer.list`, `buffer.read`, `ast.query`, `edit.replace_range`,
`edit.replace_node`, `tx.begin`, `tx.commit`, `tx.rollback`, and
`history.recent` show up in `/mcp` and become callable.

Raw stdio is also usable directly — see `scripts/mcp-smoke.sh` for an
end-to-end scenario (initialize → list → read → ast.query → edit →
history → read-back) that the build runs as a regression. Re-run it
after any protocol or transport change:

```
cargo build --release && scripts/mcp-smoke.sh
```

Known scope limits in this iteration:
- One buffer per `--mcp` invocation; `buffer.open` is implicit from the
  CLI path.
- `symbol.definition`, `diag.current`, and `edit.rename_symbol`
  require `rust-analyzer` on `PATH`. Install with `rustup component
  add rust-analyzer` (or `brew install rust-analyzer`). Without it
  the LSP tools return an error but every other tool still works.
- `edit.rename_symbol` only applies edits to the buffer currently
  open by this `--mcp` invocation. Cross-file rename targets come
  back in `skipped_files`; re-run dyad against each file (or wait for
  Phase 8 multi-buffer) to apply them. LSP positions are line +
  UTF-16 code units — exact for BMP-only source, off-by-one per
  non-BMP code point.
- No `view.*`, `symbol.references`, `symbol.signature`,
  `edit.extract_function`, `edit.add_import`, `edit.inline`, or
  `note.pin` yet.
- Edits without an explicit `tx.begin` auto-open + auto-commit a
  one-shot transaction with a synthetic intent string. Multi-step
  refactors should call `tx.begin("rename X for clarity")` first.

## Open questions

- Modal or non-modal? Helix-style or Emacs-style keybinds?
- Plugin model — Lua? WASM? None (Helix-style)?
- How is the agent's edit stream displayed to the human in real time?
  Ghost cursor? Diff overlay? Pending-hunks panel?
- Where do pinned conversations live on disk? Sidecar files? Git notes?
- Should `history.replay` actually re-invoke the original agent, or just
  reapply the recorded operations? (Probably the latter — replays should
  be deterministic.)
- Token-budget-aware context packaging: should the editor pre-compute
  "minimum viable context" for an agent based on cursor + recent edits,
  or leave that to the agent?

## Non-goals (for now)

- Replacing Helix or Neovim
- Plugin ecosystem
- Multi-language LSP support in v0
- GUI / non-terminal frontend
- Windows support in v0 (Linux + macOS first)

## North star

A human and an agent sitting at the same editor, both editing the same
buffer, both seeing each other's cursors, both speaking the same protocol.
The agent suggests a rename; the human sees it as a pending hunk; accepts
with one keystroke. The intent string ("rename for clarity per PR
feedback") is captured. Six months later, `history.recent` shows it,
`history.replay` reapplies it on another branch.
