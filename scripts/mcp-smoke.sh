#!/usr/bin/env bash
# scripts/mcp-smoke.sh — end-to-end smoke for the dyad MCP server.
#
# Drives target/release/dyad through the same JSON-RPC tools an agent
# would call: initialize, tools/list, buffer.read, ast.query,
# edit.replace_node, history.recent. The buffer starts seeded with a
# tiny Rust file so ast.query has something to match.
#
# Exits 0 on success, non-zero (with the first failing assertion
# printed) otherwise. Re-run after any protocol or transport change.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${ROOT}/target/release/dyad"
FIXTURE="$(mktemp -t dyad_smoke_XXXXXX).rs"
trap 'rm -f "$FIXTURE"' EXIT

printf 'fn hello() {}\n' > "$FIXTURE"

if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not built. Run: cargo build --release" >&2
  exit 2
fi

# A small awk-driven runner: feed multiple JSON-RPC lines into the
# binary, capture all output, then assert on individual responses by
# their JSON-RPC `id`.
RESPONSES="$(
  {
    echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
    echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
    echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"buffer.read","arguments":{"buffer_id":1}}}'
    echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ast.query","arguments":{"buffer_id":1,"query":"(function_item name: (identifier) @name)"}}}'
    # buffer.version() right after open is 0 (Buffer::open does not push edits).
    echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"edit.replace_range","arguments":{"buffer_id":1,"version":0,"range":{"start":3,"end":8},"text":"farewell"}}}'
    echo '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"history.recent","arguments":{"limit":10}}}'
    echo '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"buffer.read","arguments":{"buffer_id":1}}}'
  } | "$BIN" --mcp "$FIXTURE"
)"

assert_contains() {
  local id="$1"
  local needle="$2"
  local line
  line=$(printf '%s\n' "$RESPONSES" | awk -v id="$id" '
    {
      idx = index($0, "\"id\":" id ",")
      if (idx > 0 || index($0, "\"id\": " id ",") > 0) { print; exit }
    }
  ')
  if [[ -z "$line" ]]; then
    echo "FAIL: no response with id=$id" >&2
    echo "---raw---" >&2
    printf '%s\n' "$RESPONSES" >&2
    exit 1
  fi
  if ! printf '%s' "$line" | grep -qF -- "$needle"; then
    echo "FAIL: id=$id response missing '$needle'" >&2
    echo "got: $line" >&2
    exit 1
  fi
  echo "ok id=$id: $needle"
}

assert_contains 1 '"serverInfo"'
assert_contains 1 '"name":"dyad"'
assert_contains 2 '"name":"buffer.list"'
assert_contains 2 '"name":"edit.replace_range"'
assert_contains 2 '"name":"symbol.definition"'
assert_contains 2 '"name":"diag.current"'
assert_contains 2 '"name":"edit.rename_symbol"'
assert_contains 2 '"name":"buffer.open"'
assert_contains 2 '"name":"buffer.close"'
assert_contains 2 '"name":"clients.list"'
assert_contains 2 '"name":"git.diff"'
assert_contains 2 '"name":"edit.propose_range"'
assert_contains 2 '"name":"proposals.list"'
assert_contains 2 '"name":"proposals.accept"'
assert_contains 2 '"name":"proposals.reject"'
assert_contains 3 'fn hello() {}'
# id=4's payload is a JSON-stringified array inside an MCP text content
# item, so quotes are backslash-escaped on the wire — match that form.
assert_contains 4 '\"capture\":\"name\"'
assert_contains 5 '"isError":false'
assert_contains 6 'edit.replace_range'
assert_contains 7 'fn farewell() {}'

echo "PASS: mcp-smoke"
