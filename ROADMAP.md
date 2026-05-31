# Roadmap — remaining work

The single living list of what's left to build. Ordered by leverage.
Cross things out as they ship; keep notes inline. This file absorbed the
old `PLAN.md` (Phases 1–3 are done; the active track is Phase 4 below).

See `DESIGN.md` for the long-form spec these are drawn from. "Spec'd"
means it appears in `DESIGN.md`; "not spec'd" means it would help agents
but isn't in the design doc yet.

## How to add an MCP verb

The recurring checklist, proven by every shipped tool:

1. `language.rs` — descriptor data if language-specific.
2. New backing module *or* a `ProtocolState`/`EditorCore` method
   returning a `#[derive(Serialize)]` struct.
3. `mcp.rs` — `tool_def(...)` in `tools_list_result()` + a match arm in
   `dispatch_tool()` (inline `#[derive(Deserialize)] struct Args`).
4. Tests at all three layers: unit (`core.rs`/`protocol.rs`), transport
   (`mcp.rs`), end-to-end (`tests/mcp_integration.rs`).
5. Docs: flip the line here, note scope in `DESIGN.md`, add the row to
   `README.md`'s tool table.

## Acceptance gates (every change)

- `cargo test` green (unit + both integration suites).
- `cargo clippy --all-targets -- -D warnings` clean; new `#[allow]`
  carries a one-line justification naming the phase/constraint.
- `cargo build --release && scripts/mcp-smoke.sh` passes.
- Docs updated per the checklist above.

---

## Active track — Phase 4: live TUI+MCP coexistence

The daemon split flagged in `CLAUDE.md` and `DESIGN.md` (§Dogfooding —
TUI+MCP coexistence). The north star: the TUI and an agent both drive the
*same* editor at once. Multi-week track, sequenced so each step lands on
the previous one with full test parity before the next builds on it.

1. **Extract a shared `EditorCore`** — *done.* `core.rs` owns `buffers`,
   `tx_manager`, `proposals`, LSP clients, etc. behind `Arc<Mutex<…>>`;
   `ProtocolState` is now a thin view over it.
2. **Make the TUI a view over the same core** — *in progress.*
   - *Done:* the TUI-facing lens on `EditorCore` (`buffer_ref`,
     `buffer_meta`, `render_lines`, `tui_apply_edit`, `tui_save`), with
     unit tests. `App`/`ui.rs` still untouched at this point.
   - *Next:* flip `App` + `ui.rs` onto the lens. Drop `App`'s own
     `buffer`/`syntax`/`tx_manager`/`lsp_clients`; hold
     `core: Arc<Mutex<EditorCore>>` and keep only per-client TUI state
     (`view`, overlays, git status, autosave). Route `App::apply`
     through `tui_apply_edit`/`tui_save`; rebuild the `ui.rs` render path
     to call `render_lines`/`buffer_meta` once per frame (lock dropped
     before drawing — std `Mutex` isn't reentrant). `main.rs` constructs
     one core and shares it with both `App` and `ProtocolState`. **This
     completes the gate** — nothing below can land until App and the
     agent share one core.
3. **Event bus** — a broadcast channel emitting `{client_id, change}` so
   the TUI repaints on agent edits and vice-versa.
4. **Populate `clients.list` for real** + add `clients.cursor`.
   `ProtocolState::clients_list` is hardcoded to a single fake `agent`
   session today; the `ClientInfo.kind: human|agent` slot already exists.
5. **TUI pending-hunks panel** — render `proposals.list` as an
   accept/reject overlay (reuse the git-overlay modal style — `s`/`u`/`c`
   — to dodge the global keymap; single-`Ctrl+letter` only, per terminal
   constraints).
6. **Ghost agent cursor** in `ui.rs`.

---

## Deferred sub-features from shipped phases

- `scope.in_scope` **`locals`** — the params/`let`-bindings tier via a
  Tree-sitter scope walk. `enclosing` + `imports` + `siblings` shipped;
  `locals` is the deep part and was held back to avoid algorithmic risk.
- `context.pack` **v1 rungs** — the LSP-backed candidate tiers
  (referenced type defs via `symbol.definition`, callee signatures via
  `symbol.hover`, enclosing-item docstring). v0 ships an LSP-free greedy
  packer (anchor → imports → sibling signatures, `chars/4` budget,
  always-honest `truncated` flag); the v1 rungs slot in as lower-priority
  tiers behind it.

---

## Shipped

- Phases 1–3 of the agent-native build-out:
  - `test.run(target?)` / `test.last_results` (`DESIGN.md` edit→verify
    loop). Rust → `cargo test`; libtest's human summary +
    `---- <name> stdout ----` failure blocks parsed in `test_runner.rs`;
    result cached. Smoke runs a filtered live `cargo test`.
  - `scope.imports` / `scope.in_scope` ("the unlock"). `scope.imports`
    is an LSP-free tree-sitter query (`@import` per
    `Language::import_query`); `scope.in_scope` layers LSP
    `documentSymbol` for `enclosing` (outer→inner) + `siblings`.
  - `context.pack(buffer, position, token_budget)` — v0 deterministic
    greedy packer (see deferred v1 above).
- `git.diff`, `git.status`, `git.log`, `git.show`, `git.stage`,
  `git.unstage`, `git.commit` — wired through `ProtocolState`/`mcp.rs`;
  smoke covers `git.status` + `git.log`.
- `symbol.references` (LSP `textDocument/references`).
- `symbol.hover` (LSP `textDocument/hover`; also covers the
  `symbol.signature` slot — same endpoint, agent slices the body).
- `buffer.version(id)` — thin wrapper over `Buffer::version`.
- `proposals.count` — wrapper over `ProposalQueue::count()`.

---

## High leverage, medium effort

- `ast.node_at(buffer, position)` — single-node lookup by point; infra is
  in `syntax.rs`.
- Workspace navigation (not spec'd in `DESIGN.md`):
  - `fs.list(path, glob?)`, `fs.exists(path)`
  - `search.text(query, glob?)` — ripgrep-style content search
  - `workspace.root()`, `workspace.languages()`
- `format.file` / `format.range` — call `rustfmt`; useful right after
  structural edits.

## Spec'd but never started

- `diag.subscribe` — push diagnostics instead of poll.
- `history.diff(change_id)` / `history.replay(change_id, target)` /
  `history.tree(buffer_id)` — replay is the unique-to-dyad pitch.
- Conversation pins: `note.pin` / `note.list` / `note.resolve` with
  Tree-sitter re-anchoring (`DESIGN.md` §Conversation pins).

## Tier 2/3 edits (partly missing)

- `edit.wrap_node`, `edit.insert_before_node`, `edit.insert_after_node`
  — cheap Tree-sitter-aware variants.
- `edit.add_import` — Rust-specific, high agent value.
- `edit.extract_function`, `edit.inline` — bigger lifts (likely
  hand-rolled before LSP catches up).

## Larger / structural

- `git.diff` against the unsaved buffer — needs an in-process diff vs.
  disk (currently reads disk).
- Cross-buffer atomic `edit.rename_symbol`; today it's per-buffer
  auto-tx.
- `git.blame` — line-level provenance; needs new backing code in
  `src/git.rs`.
- Branch / checkout / push / pull / fetch — none in `src/git.rs` yet.
  Decide whether to add or stay shell-out-only.

## Quality of life

- Agent breadcrumb / metadata store (key-value scoped per
  `conversation_id`).
- Buffer save state / modtime query.
- `tools/list` filter or namespace grouping — the list is growing.

---

## Suggested next slice (after the Phase 4 gate)

Once the App-on-core flip lands, the next concentrated slice is the
**high-leverage, medium-effort** workspace-navigation verbs (`fs.list`,
`fs.exists`, `search.text`, `workspace.root`, `workspace.languages`).
Each is a thin wrapper, but together they unblock the "agent navigates
the repo without shelling out" loop and make `symbol.workspace_search`
and the edit tools productively usable.
