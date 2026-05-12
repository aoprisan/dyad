# Roadmap — agent-helping features

Living list of MCP/protocol features still missing for an agent client
(e.g. Claude Code) to be a fully symmetric peer to the TUI, ordered by
leverage. Cross things out as they ship; keep notes inline.

See `DESIGN.md` §Protocol sketch for the long-form spec these are
drawn from. "Spec'd" below means it appears in `DESIGN.md`; "not
spec'd" means it would help agents but isn't in the design doc yet.

## Done

- `git.diff` (initial Phase 9)
- `git.status`, `git.log`, `git.show`, `git.stage`, `git.unstage`,
  `git.commit` — wired through `ProtocolState` and `mcp.rs`; smoke
  covers `git.status` + `git.log` round-trips.
- `symbol.references` — LSP `textDocument/references`, paired with
  the existing `symbol.definition`.
- `symbol.hover` — LSP `textDocument/hover` (also covers the
  `symbol.signature` slot; same endpoint, agent slices the body).
- `buffer.version(id)` — thin wrapper exposing `Buffer::version`.
- `proposals.count` — wrapper over `ProposalQueue::count()`.

## High leverage, medium effort

- `scope.in_scope` / `scope.imports` — `DESIGN.md` calls this "the
  unlock" (line 68). Combine LSP `documentSymbol` + Tree-sitter scope
  walking.
- `ast.node_at(buffer, position)` — single-node lookup by point;
  infra is in `syntax.rs`.
- Workspace navigation (not spec'd in `DESIGN.md`):
  - `fs.list(path, glob?)`, `fs.exists(path)`
  - `search.text(query, glob?)` — ripgrep-style content search
  - `workspace.root()`, `workspace.languages()`
- `format.file` / `format.range` — call `rustfmt`; useful right
  after structural edits.

## Spec'd but never started

- `test.run(target?)` / `test.last_results` (`DESIGN.md` 121-123) —
  completes the agent validation loop.
- `diag.subscribe` — push diagnostics instead of poll.
- `history.diff(change_id)` / `history.replay(change_id, target)` /
  `history.tree(buffer_id)` — replay is the unique-to-dyad pitch.
- Conversation pins: `note.pin` / `note.list` / `note.resolve` with
  Tree-sitter re-anchoring (`DESIGN.md` 138-145).

## Tier 2/3 edits (partly missing)

- `edit.wrap_node`, `edit.insert_before_node`, `edit.insert_after_node`
  — cheap Tree-sitter-aware variants.
- `edit.add_import` — Rust-specific, high agent value.
- `edit.extract_function`, `edit.inline` — bigger lifts (likely
  hand-rolled before LSP catches up).

## Larger / structural

- `git.diff` against the unsaved buffer (`DESIGN.md` 218) — needs an
  in-process diff vs. disk.
- Cross-buffer atomic `edit.rename_symbol`; today it's per-buffer
  auto-tx (see `DESIGN.md` 207-210).
- Multi-client awareness (Phase 8): real `clients.list` populated
  with concurrent TUI + MCP sessions, `clients.cursor`,
  `clients.subscribe_edits`. Needs a TUI/MCP daemon split.
- `git.blame` — line-level provenance; needs new backing code in
  `src/git.rs`.
- Branch / checkout / push / pull / fetch — none in `src/git.rs`
  yet. Decide whether to add or stay shell-out-only.

## Quality of life

- Agent breadcrumb / metadata store (key-value scoped per
  `conversation_id`).
- Buffer save state / modtime query.
- `tools/list` filter or namespace grouping — the list is growing.

## Suggested next slice

With the low-effort LSP/proposal wrappers landed, the next concentrated
slice is **High-leverage, medium-effort** workspace navigation: a small
set of read-only filesystem + search verbs (`fs.list`, `fs.exists`,
`search.text`, `workspace.root`, `workspace.languages`) that an agent
needs before it can productively use `symbol.workspace_search` and the
edit tools. Each one is a thin wrapper, but together they unblock the
"agent navigates the repo without shelling out" loop.
