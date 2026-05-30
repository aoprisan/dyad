# Implementation Plan — agent-native feature build-out

Sequenced by ROI and dependency. Each phase is independently shippable, passes
`cargo test` + `cargo clippy -D warnings`, and follows the project invariant: a
new agent capability is a `ProtocolState` method exposed through one `mcp.rs`
dispatch arm + one `tool_def`, with unit tests in `protocol.rs`, transport tests
in `mcp.rs`, and an end-to-end test in `tests/mcp_integration.rs`.

The recurring "add an MCP verb" checklist (proven by every existing tool):

1. `language.rs` — descriptor data if language-specific.
2. New backing module *or* `protocol.rs` method returning a `#[derive(Serialize)]` struct.
3. `mcp.rs` — `tool_def(...)` in `tools_list_result()` + match arm in `dispatch_tool()`
   (inline `#[derive(Deserialize)] struct Args`).
4. Tests at all three layers.
5. Docs: flip the line in `ROADMAP.md`, note scope in `DESIGN.md`, add the row to
   `README.md`'s tool table.

---

## Phase 1 — `test.run` / `test.last_results` (closes the edit→verify loop)

**Why first:** highest ROI for autonomous agents, spec'd in `DESIGN.md:121-123`,
and structurally a clone of the existing `git.rs` shell-out pattern. No LSP, no
new concurrency.

### Step 1.1 — language descriptor
`language.rs`: add `test_command(self) -> Option<&'static [&'static str]>` (Rust →
`["cargo", "test"]`; Scala/Elm → `None` initially so unsupported languages bail
cleanly). Add a `test_runner_kind()` enum (`CargoTest`) selecting the output
parser. Cover with a descriptor unit test alongside the existing ones.

### Step 1.2 — new module `src/test_runner.rs`
Mirror `git.rs`:
- `run(workspace_root: &Path, cmd: &[&str], target: Option<&str>) -> Result<TestResults>`
  using `Command::new(cmd[0]).current_dir(workspace_root)…` (cargo has no `-C`).
- Parse libtest's stable summary lines (`test result: ok. N passed; M failed; …`)
  plus the `failures:` block (avoid nightly `--format=json`).
- Shape:
  ```rust
  #[derive(Serialize)] pub struct TestResults {
      pub passed: usize, pub failed: usize, pub ignored: usize,
      pub failures: Vec<TestFailure>,   // {name, message}
      pub exit_ok: bool,
      pub raw_tail: String,             // last ~4KB, capped
  }
  ```
- Cap captured output (spirit of `TASKS_MAX_FILE_BYTES`) so a noisy build can't
  balloon the MCP response.

### Step 1.3 — protocol surface
`protocol.rs`, new `// ---------- Tests ----------` section after git:
- `pub fn test_run(&mut self, buffer_id, target: Option<&str>) -> Result<TestResults>`
  — resolve workspace root via `repo_root_for_buffer` / `lsp::workspace_root_for`,
  pick the command from `language.test_command()`, call `test_runner::run`, and
  **cache** the result in a new `last_test_results: Option<TestResults>` field.
- `pub fn test_last_results(&self) -> Option<TestResults>` — return the cache.
- Add the field to the struct + its initializer.

### Step 1.4 — transport + tests + docs
- `mcp.rs`: `tool_def("test.run", …)` (`buffer_id`, optional `target`) and
  `tool_def("test.last_results", …)`; two dispatch arms.
- Tests: parser unit test against a captured libtest string (hermetic, fast);
  `protocol.rs` test that `test_last_results` is `None` before / `Some` after
  (live run `#[ignore]`-gated like the LSP tests); `mcp.rs` tools/list assertions.
- Flip `ROADMAP.md:40`, add README rows, note in `DESIGN.md` §Dogfooding.

**Risk:** running `cargo test` from within `cargo test`. Mitigation: the *parser*
is the unit-tested core; the live-run test is `#[ignore]`-gated and exercised by
`scripts/mcp-smoke.sh`. **Effort: ~1 session.**

---

## Phase 2 — `scope.in_scope` / `scope.imports` ("the unlock")

**Why second:** `DESIGN.md:68` calls it the headline agent feature. Builds on
existing LSP + Tree-sitter infra; no new subprocess management.

### Step 2.1 — LSP method
`lsp.rs`: add `pub fn document_symbols(&self, uri) -> Result<Vec<DocumentSymbol>>`
issuing `textDocument/documentSymbol` via the private `request()` helper (same
shape as `workspace_symbol`). Handle both response variants (hierarchical
`DocumentSymbol[]` and flat `SymbolInformation[]`), normalizing to
`{name, kind, range, children}`. Advertise the `documentSymbol` capability in
`initialize`.

### Step 2.2 — scope assembly in `protocol.rs`
New `// ---------- Scope ----------` section:
- `pub fn scope_imports(&self, buffer_id) -> Result<Vec<ImportEntry>>` —
  Tree-sitter query against the cached tree (the `ast_query` path proves tree
  access). Per-language capture (`use_declaration` / `import`). LSP-free.
- `pub fn scope_in_scope(&self, buffer_id, line, character) -> Result<ScopeReport>`
  — combine enclosing symbols (`document_symbols` whose range encloses the point),
  imports, and sibling top-level symbols:
  ```rust
  #[derive(Serialize)] pub struct ScopeReport {
      pub enclosing: Vec<SymbolRef>,   // outer→inner: mod > impl > fn
      pub locals: Vec<SymbolRef>,      // params/lets (TS scope walk)
      pub imports: Vec<ImportEntry>,
      pub siblings: Vec<SymbolRef>,    // other top-level items
  }
  ```
- Ship `enclosing` + `imports` + `siblings` first; add `locals` (Tree-sitter
  scope walk) as a follow-up if it balloons.

### Step 2.3 — transport + tests + docs
- `mcp.rs`: two `tool_def`s + two dispatch arms.
- Tests: `protocol.rs` unit tests over a fixture Rust file (imports are
  LSP-free → easy assertions); `document_symbols` test `#[ignore]`-gated on
  rust-analyzer; `mcp.rs` tools/list assertions; one `tests/mcp_integration.rs`
  round-trip for `scope.imports`.
- Flip `ROADMAP.md:26-28`.

**Risk:** `locals` scope-walking is the deep part. Mitigation: phase it — imports
+ enclosing + siblings deliver most value with zero new algorithmic risk.
**Effort: ~1–1.5 sessions.**

---

## Phase 3 — `context.pack(buffer, position, token_budget)` (standout net-new)

**Why third:** depends on Phase 2 (uses scope + symbols) and is the feature
nothing else does well. Higher design risk, so it follows the two contained wins.

### Steps
1. `protocol.rs` `context_pack(buffer_id, line, character, token_budget)`:
   - Anchor = enclosing function (from `scope_in_scope`).
   - Expand outward by priority until budget hit: enclosing fn body → referenced
     type defs (`symbol_definition`) → callee signatures (`symbol_hover`) →
     imports → enclosing-item docstring.
   - Token estimate: cheap `chars/4` heuristic (real tokenizer out of scope);
     expose the estimate so the agent can recalibrate.
2. Return `{ anchor, slices: [{path, range, reason, text}], estimated_tokens, truncated }`.
   **Always flag `truncated`** — never silently drop (matches the `TASKS_MAX_HITS`
   honesty rule).
3. Tests: deterministic packing test on a fixture with a known budget; assert
   anchor-first ordering and `truncated` flips on a tiny budget.

**Risk:** scope creep on "relevance." Mitigation: ship a deterministic
priority-ordered greedy packer (no scoring model), document it as v0 in
`DESIGN.md`, iterate later. **Effort: ~2 sessions.**

**Shipped (v0):** anchor (enclosing fn) → imports → sibling signatures, all
Tree-sitter / LSP-free, greedily packed to a `chars/4` budget with an
always-reported `truncated` flag. Deviation from the sketch: the anchor is
resolved with Tree-sitter rather than `scope_in_scope`, so `context.pack`
needs no language server. The LSP-backed rungs (referenced type defs, callee
signatures via `symbol.hover`, enclosing-item docstring) are the deferred v1 —
they slot in as lower-priority candidate tiers behind the existing greedy pack.

---

## Phase 4 — Live TUI+MCP coexistence (the north star) — *larger track*

The daemon split flagged in `CLAUDE.md`, `DESIGN.md:211-214`, `ROADMAP.md:62-64`.
Multi-week track, not a session. High-level decomposition:

1. **Extract a shared `EditorCore`** owning `buffers`, `tx_manager`, `proposals`
   behind `Arc<Mutex<…>>`, with `App` and `ProtocolState` becoming *views* over it
   (today `App` owns `Buffer` directly and `ProtocolState` owns its own — unifying
   these is the real work; gate for everything else).
2. **Event bus**: broadcast channel emitting `{client_id, change}` so the TUI
   repaints on agent edits and vice versa.
3. **Populate `clients.list` for real** + add `clients.cursor` (the `ClientInfo`
   struct already has the `kind: human|agent` slot).
4. **TUI pending-hunks panel**: render `proposals.list` as an overlay with
   accept/reject keys (reuse the git-overlay modal style — `s`/`u`/`c` — to dodge
   the global keymap; single-`Ctrl+letter` only, per terminal constraints).
5. **Ghost agent cursor** in `ui.rs`.

Sequence: (1) core extraction → (2) event bus → (3) clients API → (4) proposals
panel → (5) ghost cursor. (1) lands with full test parity before anything builds
on it.

---

## Cross-cutting acceptance gates (every phase)

- `cargo test` green (unit + both integration suites).
- `cargo clippy --all-targets -- -D warnings` clean; new `#[allow]` carries a
  one-line justification.
- `cargo build --release && scripts/mcp-smoke.sh` passes (extend smoke for
  `test.run` and `scope.imports`).
- Docs updated: `ROADMAP.md` line flipped, `README.md` tool-table row, `DESIGN.md`
  §Dogfooding scope note.
- Fold in the stale-doc fix found in review: `README.md:34` says `--install`
  *symlinks* — it now *copies* (per commit `6364bc4`).

---

**Recommended start:** Phase 1 (`test.run`) — most self-contained, clearest
existing template (`git.rs`), and immediately upgrades any agent loop.
