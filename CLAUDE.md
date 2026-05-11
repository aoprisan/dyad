# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`dyad` is an agent-native terminal editor written in Rust. The full design is in
`DESIGN.md` — read it before making non-trivial changes. The headline idea: the
editor is a runtime, and humans and agents are symmetric clients speaking the
same protocol. There is no privileged path for the UI vs. an agent.

The current code is **Phase 1** of the phased build order in `DESIGN.md`
(§Phased build order): buffer + view + textual edit, with no Tree-sitter, no
LSP, no MCP, no transactions yet. Subsequent phases bolt on without
restructuring; the module layout was chosen with that in mind.

## Commands

```bash
cargo build                              # build
cargo run -- <path>                      # open <path> in the editor (creates on save if missing)
cargo clippy --all-targets -- -D warnings  # lint (must stay clean)
```

There are no tests yet. The editor is exercised manually; a smoke checklist
lives in the approved Phase 1 plan at `~/.claude/plans/`.

## Architecture

Data flow per keystroke:

```
KeyEvent -> input::map -> Action -> App::apply -> Buffer/View mutations -> ui::render
```

- `buffer.rs` — `Buffer { rope: ropey::Rope, path, version, dirty }`. Owns the
  text. **Every mutation bumps `version` and sets `dirty`** — `version` is
  Phase 4 (MCP) optimistic-concurrency scaffolding (DESIGN.md §Buffers & views:
  "Every read returns a version. Every write must reference one."). Method
  names (`insert_char`, `delete_range`, `save`) are deliberately close to the
  protocol verbs in DESIGN.md §Edits so Phase 4 wraps them thinly; **do not
  rename them casually**.
- `view.rs` — `View { cursor_line, cursor_col, sticky_col, top_line }`.
  **Borrows `Buffer`; never owns it.** This is the multi-client hook
  (DESIGN.md §Awareness / multi-client): Phase 8 will have many views per
  buffer, and that should be a non-event.
- `app.rs` — `App` owns one `Buffer` + one `View` and runs the event loop.
  `App::apply(Action)` is the **single mutation funnel** — Phase 3 (DESIGN.md
  §Transactions & intent) will wrap it with `tx.begin/commit`, so keep all
  state changes flowing through it.
- `action.rs` — flat `Action` enum, no logic. Phase 4 MCP handlers will
  construct the same enum the keymap does.
- `input.rs` — pure `KeyEvent -> Option<Action>`. The keymap. **Non-modal**:
  letter keys always insert, Alt+h/j/k/l and arrows move, Ctrl-S saves,
  Ctrl-Q quits.
- `ui.rs` — single `render(frame, app)`. Vertical split into content + 1-row
  status; content split into gutter (width = `digits(line_count) + 1`) +
  text. No soft wrap, no horizontal scroll yet (long lines truncate).
- `terminal.rs` — RAII `Guard` around `ratatui::try_init`/`restore`. The
  `try_init` call installs a panic hook that restores the terminal before the
  panic message prints, so a panic mid-render does not strand the user.

## Invariants to preserve

- **Cursor positions are char offsets within a line, not display cells.**
  Wide Unicode chars and tab-width math are deliberately deferred. If you add
  display-cell awareness, do it in `view.rs` and `ui.rs` together; don't leak
  cell math into `buffer.rs`.
- **No privileged UI path.** When adding new editing capability, expose it as
  a method on `Buffer` (named to match the eventual protocol verb) and call it
  from `App::apply`. Do not reach into the rope from `app.rs` or `ui.rs`.
- **`#[allow(dead_code)]` on `Buffer::insert_str` and `Buffer::version` is
  intentional** — those are Phase 4 protocol scaffolding. Don't delete them.
- **`cargo clippy -D warnings` must stay clean.** New `#[allow]`s need a
  one-line justification comment naming the phase or constraint that requires
  them.

## When in doubt

- "What's the right name for this new edit operation?" — Look at DESIGN.md
  §Edits — three tiers. Match the protocol verb.
- "Should this go in Buffer or View?" — Buffer owns the text and persistence.
  View owns the cursor and viewport. If it touches the rope, it's Buffer.
- "Should I add a feature for the next phase while I'm here?" — No. Each
  phase in DESIGN.md is its own scope. Forward-compat scaffolding that's
  already in place (the `version` field, the borrow-not-own discipline) is
  enough.
