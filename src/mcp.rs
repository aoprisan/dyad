//! Phase 4 — MCP stdio transport.
//!
//! JSON-RPC 2.0, line-delimited, over stdin/stdout. Implements just
//! enough of the MCP spec to register and call tools: `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`. Each tool
//! routes to a `ProtocolState` method; results come back as a single
//! `text` content item carrying the JSON payload.
//!
//! The dispatcher is split from the I/O loop so tests can drive it in
//! process without spinning up a subprocess (`handle_line`).

use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{ByteRange, CharRange, ProtocolState};
use crate::tx::TxId;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "dyad";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Read-process-write loop on stdio. One JSON-RPC message per line.
pub fn run(mut state: ProtocolState) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&mut state, &line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Process a single JSON-RPC line and return the serialized response,
/// or `None` for notifications.
pub fn handle_line(state: &mut ProtocolState, line: &str) -> Option<String> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = req.get("id").is_none();
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let result = match method.as_str() {
        "initialize" => Ok(initialize_result()),
        "notifications/initialized" | "initialized" => return None,
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => handle_tools_call(state, params),
        // Common MCP discovery methods we don't host yet:
        "ping" => Ok(json!({})),
        "resources/list" | "prompts/list" => Ok(json!({"resources": [], "prompts": []})),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    if is_notification {
        return None;
    }
    Some(match result {
        Ok(v) => success_response(id, v),
        Err((code, message)) => error_response(id, code, &message),
    })
}

// ---------- MCP responses ----------

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            tool_def("buffer.list", "List all open buffers.", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("buffer.open", "Open a file as an additional buffer; returns the new buffer_id.", json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string"},
                },
            })),
            tool_def("buffer.close", "Close a buffer by id. If it's the focused buffer, focus shifts to the lowest remaining id (or None when no buffers remain).", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("clients.list", "List active clients with id, kind (agent | human), and currently focused buffer_id.", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("git.diff", "Return the raw `git diff HEAD --no-color -- <path>` for the buffer's file. Errors when the file isn't tracked or git isn't usable.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("git.status", "List `git status --porcelain=v1` entries for the repo containing the buffer's file. Each entry: {path, staged, unstaged}; staged/unstaged are porcelain chars (space = unchanged, M/A/D/?/etc).", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("git.log", "Most recent `limit` commits in the buffer's repo. Each entry: {sha, short_sha, author, date, subject}.", json!({
                "type": "object",
                "required": ["buffer_id", "limit"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "limit":     {"type": "integer", "minimum": 1},
                },
            })),
            tool_def("git.show", "Full `git show --no-color <sha>` output for a commit in the buffer's repo. SHA accepted in any form git recognises (full, short, ref).", json!({
                "type": "object",
                "required": ["buffer_id", "sha"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "sha":       {"type": "string"},
                },
            })),
            tool_def("git.stage", "Stage a file via `git add`. When `path` is omitted, stages the buffer's own file. When set, takes the string as a path relative to the repo root.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "path":      {"type": "string"},
                },
            })),
            tool_def("git.unstage", "Unstage a file via `git restore --staged`. Same path semantics as git.stage.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "path":      {"type": "string"},
                },
            })),
            tool_def("git.commit", "Commit currently-staged changes with `message`. Returns git's stdout. Pre-commit hook failures and 'nothing to commit' surface as the error text.", json!({
                "type": "object",
                "required": ["buffer_id", "message"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "message":   {"type": "string"},
                },
            })),
            tool_def("edit.propose_range", "Queue an `edit.replace_range` for review instead of applying it. Returns a proposal_id. The reviewer (eventually a human at the TUI) accepts or rejects via the `proposals.*` tools.", json!({
                "type": "object",
                "required": ["buffer_id", "version", "range", "text", "intent"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "version":   {"type": "integer"},
                    "range": {
                        "type": "object",
                        "required": ["start", "end"],
                        "properties": {
                            "start": {"type": "integer", "minimum": 0},
                            "end":   {"type": "integer", "minimum": 0},
                        },
                    },
                    "text":   {"type": "string"},
                    "intent": {"type": "string"},
                },
            })),
            tool_def("proposals.list", "List queued proposals: [{id, buffer_id, intent, kind: {kind, ...}}].", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("proposals.accept", "Apply a queued proposal through the tx machinery (using the proposal's intent). Errors if the buffer version moved; the proposal is re-queued so the agent can retry.", json!({
                "type": "object",
                "required": ["proposal_id"],
                "properties": {
                    "proposal_id": {"type": "integer"},
                },
            })),
            tool_def("proposals.reject", "Discard a queued proposal without applying.", json!({
                "type": "object",
                "required": ["proposal_id"],
                "properties": {
                    "proposal_id": {"type": "integer"},
                },
            })),
            tool_def("proposals.accept_all", "Accept every queued proposal in id order, each through the same tx machinery as proposals.accept. Returns {accepted, errors: [{proposal_id, message}]}; individual failures (typically version mismatches) re-queue the offending proposal under a fresh id without stopping the batch.", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("proposals.reject_all", "Discard every queued proposal. Returns {rejected} — the count dropped.", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("buffer.read", "Read all or part of a buffer's text. Returns {text, version}.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "range": {
                        "type": "object",
                        "required": ["start", "end"],
                        "properties": {
                            "start": {"type": "integer", "minimum": 0},
                            "end":   {"type": "integer", "minimum": 0},
                        },
                    },
                },
            })),
            tool_def("ast.query", "Run a tree-sitter query against a buffer's parse tree. Returns [{capture, kind, byte_start, byte_end}].", json!({
                "type": "object",
                "required": ["buffer_id", "query"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "query": {"type": "string"},
                },
            })),
            tool_def("edit.replace_range", "Replace a char range with new text. version must match buffer's current version. Returns new_version.", json!({
                "type": "object",
                "required": ["buffer_id", "version", "range", "text"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "version":   {"type": "integer"},
                    "range": {
                        "type": "object",
                        "required": ["start", "end"],
                        "properties": {
                            "start": {"type": "integer", "minimum": 0},
                            "end":   {"type": "integer", "minimum": 0},
                        },
                    },
                    "text": {"type": "string"},
                },
            })),
            tool_def("edit.replace_node", "Replace the bytes of a tree-sitter node (typically from ast.query) with new text. Returns new_version.", json!({
                "type": "object",
                "required": ["buffer_id", "version", "byte_range", "text"],
                "properties": {
                    "buffer_id":  {"type": "integer"},
                    "version":    {"type": "integer"},
                    "byte_range": {
                        "type": "object",
                        "required": ["start", "end"],
                        "properties": {
                            "start": {"type": "integer", "minimum": 0},
                            "end":   {"type": "integer", "minimum": 0},
                        },
                    },
                    "text": {"type": "string"},
                },
            })),
            tool_def("tx.begin", "Open a transaction against `buffer_id` with a stated intent. Subsequent edits to that buffer join this tx until tx.commit or tx.rollback.", json!({
                "type": "object",
                "required": ["buffer_id", "intent"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "intent": {"type": "string"},
                    "conversation_id": {"type": "string"},
                },
            })),
            tool_def("tx.commit", "Close the current transaction and record a Change in flat history. Returns change_id.", json!({
                "type": "object",
                "required": ["tx_id"],
                "properties": {
                    "tx_id": {"type": "integer"},
                },
            })),
            tool_def("tx.rollback", "Discard the current transaction and restore the buffer to its pre-tx state.", json!({
                "type": "object",
                "required": ["tx_id"],
                "properties": {
                    "tx_id": {"type": "integer"},
                },
            })),
            tool_def("history.recent", "Return the most-recent `limit` history entries.", json!({
                "type": "object",
                "required": ["limit"],
                "properties": {
                    "limit": {"type": "integer", "minimum": 0},
                },
            })),
            tool_def("symbol.definition", "Find the LSP definition location for a symbol at the given zero-based line/character. Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id", "line", "character"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "line":      {"type": "integer", "minimum": 0},
                    "character": {"type": "integer", "minimum": 0},
                },
            })),
            tool_def("symbol.references", "Find all references to the symbol at (line, character) via LSP `textDocument/references`. `include_declaration` (default true) controls whether the symbol's own declaration is in the result. Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id", "line", "character"],
                "properties": {
                    "buffer_id":           {"type": "integer"},
                    "line":                {"type": "integer", "minimum": 0},
                    "character":           {"type": "integer", "minimum": 0},
                    "include_declaration": {"type": "boolean"},
                },
            })),
            tool_def("symbol.hover", "Plain-text hover/signature body for the symbol at (line, character), via LSP `textDocument/hover`. Returns {text: string | null}; null means the server had nothing to report. Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id", "line", "character"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "line":      {"type": "integer", "minimum": 0},
                    "character": {"type": "integer", "minimum": 0},
                },
            })),
            tool_def("buffer.version", "Current version of a buffer (the optimistic-concurrency token edits must reference). Cheaper than buffer.read when the agent only wants to check whether something has moved.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("proposals.count", "Number of proposals currently in the queue. Cheap status check that avoids the cost of a full proposals.list.", json!({
                "type": "object",
                "properties": {},
            })),
            tool_def("symbol.workspace_search", "Fuzzy-search workspace symbols (types, functions, etc) via LSP `workspace/symbol`. `buffer_id` picks the language server to query; the search itself is workspace-wide. Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id", "query"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "query":     {"type": "string"},
                },
            })),
            tool_def("diag.current", "Return cached LSP diagnostics for a buffer (severity 1=error..4=hint). Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("diag.wait_until_idle", "Block until the LSP serving `buffer_id` has acknowledged the latest sync with a publishDiagnostics, and (for rust-analyzer / metals) is no longer indexing. Returns {caught_up, diagnostics}; caught_up=false means the timeout fired and the diagnostics may be stale. Pair with an edit.* call to do edit-then-verify without polling. `timeout_ms` defaults to 3000.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id":  {"type": "integer"},
                    "timeout_ms": {"type": "integer", "minimum": 0},
                },
            })),
            tool_def("tasks.list", "Scan the workspace beneath `buffer_id` for inline agent-task markers (`CLAUDE: ...`, `TODO(claude): ...`, case-insensitive on the keyword). Walks the buffer's git repo when present, else its parent directory. Returns [{path, line, kind, text}] sorted by path+line; `kind` is `claude` or `todo`. Lets you drop intent into comments and pick it up on the next pass without copy-paste.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                },
            })),
            tool_def("edit.rename_symbol", "Rename the symbol at (line, character) to new_name. Applies in-buffer edits as one transaction; cross-file changes come back in skipped_files. Requires a running language server (rust-analyzer for .rs, metals for .scala/.sc/.sbt).", json!({
                "type": "object",
                "required": ["buffer_id", "version", "line", "character", "new_name"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "version":   {"type": "integer"},
                    "line":      {"type": "integer", "minimum": 0},
                    "character": {"type": "integer", "minimum": 0},
                    "new_name":  {"type": "string"},
                },
            })),
        ]
    })
}

fn tool_def(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

// ---------- tools/call dispatch ----------

fn handle_tools_call(
    state: &mut ProtocolState,
    params: Value,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "tools/call missing `name`".to_string()))?
        .to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome = dispatch_tool(state, &name, arguments);
    Ok(call_tool_result(outcome))
}

/// Map a single tool name to a `ProtocolState` method. Returns a JSON
/// `Value` on success or an `anyhow::Error` so the caller can surface
/// it as a `CallToolResult { isError: true }`.
fn dispatch_tool(
    state: &mut ProtocolState,
    name: &str,
    args: Value,
) -> Result<Value> {
    match name {
        "buffer.list" => Ok(json!(state.buffer_list())),
        "buffer.open" => {
            #[derive(Deserialize)]
            struct Args {
                path: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let id = state.buffer_open(std::path::PathBuf::from(a.path))?;
            Ok(json!({ "buffer_id": id }))
        }
        "buffer.close" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            state.buffer_close(a.buffer_id)?;
            Ok(json!({}))
        }
        "clients.list" => Ok(json!(state.clients_list())),
        "git.diff" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            let diff = state.git_diff(a.buffer_id)?;
            Ok(json!({ "diff": diff }))
        }
        "git.status" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.git_status(a.buffer_id)?))
        }
        "git.log" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                limit: usize,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.git_log(a.buffer_id, a.limit)?))
        }
        "git.show" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                sha: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let patch = state.git_show(a.buffer_id, &a.sha)?;
            Ok(json!({ "patch": patch }))
        }
        "git.stage" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                path: Option<String>,
            }
            let a: Args = serde_json::from_value(args)?;
            state.git_stage(a.buffer_id, a.path.as_deref())?;
            Ok(json!({}))
        }
        "git.unstage" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                path: Option<String>,
            }
            let a: Args = serde_json::from_value(args)?;
            state.git_unstage(a.buffer_id, a.path.as_deref())?;
            Ok(json!({}))
        }
        "git.commit" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                message: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let output = state.git_commit(a.buffer_id, &a.message)?;
            Ok(json!({ "output": output }))
        }
        "edit.propose_range" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                version: u64,
                range: CharRange,
                text: String,
                intent: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let id = state.propose_replace_range(
                a.buffer_id,
                a.version,
                a.range,
                a.text,
                a.intent,
            )?;
            Ok(json!({ "proposal_id": id }))
        }
        "proposals.list" => Ok(json!(state.proposals_list())),
        "proposals.accept" => {
            #[derive(Deserialize)]
            struct Args {
                proposal_id: crate::proposals::ProposalId,
            }
            let a: Args = serde_json::from_value(args)?;
            let new_version = state.proposal_accept(a.proposal_id)?;
            Ok(json!({ "version": new_version }))
        }
        "proposals.reject" => {
            #[derive(Deserialize)]
            struct Args {
                proposal_id: crate::proposals::ProposalId,
            }
            let a: Args = serde_json::from_value(args)?;
            state.proposal_reject(a.proposal_id)?;
            Ok(json!({}))
        }
        "proposals.accept_all" => {
            let result = state.proposals_accept_all();
            Ok(json!(result))
        }
        "proposals.reject_all" => {
            let rejected = state.proposals_reject_all();
            Ok(json!({ "rejected": rejected }))
        }
        "buffer.read" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                range: Option<CharRange>,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.buffer_read(a.buffer_id, a.range)?))
        }
        "ast.query" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                query: String,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.ast_query(a.buffer_id, &a.query)?))
        }
        "edit.replace_range" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                version: u64,
                range: CharRange,
                text: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let v = state.edit_replace_range(a.buffer_id, a.version, a.range, &a.text)?;
            Ok(json!({ "version": v }))
        }
        "edit.replace_node" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                version: u64,
                byte_range: ByteRange,
                text: String,
            }
            let a: Args = serde_json::from_value(args)?;
            let v = state.edit_replace_node(a.buffer_id, a.version, a.byte_range, &a.text)?;
            Ok(json!({ "version": v }))
        }
        "tx.begin" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                intent: String,
                conversation_id: Option<String>,
            }
            let a: Args = serde_json::from_value(args)?;
            let tx_id = state.tx_begin(a.buffer_id, a.intent, a.conversation_id)?;
            Ok(json!({ "tx_id": tx_id }))
        }
        "tx.commit" => {
            #[derive(Deserialize)]
            struct Args {
                tx_id: TxId,
            }
            let a: Args = serde_json::from_value(args)?;
            let change_id = state.tx_commit(a.tx_id)?;
            Ok(json!({ "change_id": change_id }))
        }
        "tx.rollback" => {
            #[derive(Deserialize)]
            struct Args {
                tx_id: TxId,
            }
            let a: Args = serde_json::from_value(args)?;
            state.tx_rollback(a.tx_id)?;
            Ok(json!({}))
        }
        "history.recent" => {
            #[derive(Deserialize)]
            struct Args {
                limit: usize,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.history_recent(a.limit)))
        }
        "symbol.definition" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                line: u32,
                character: u32,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.symbol_definition(a.buffer_id, a.line, a.character)?))
        }
        "symbol.references" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                line: u32,
                character: u32,
                #[serde(default = "default_include_declaration")]
                include_declaration: bool,
            }
            fn default_include_declaration() -> bool {
                true
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.symbol_references(
                a.buffer_id,
                a.line,
                a.character,
                a.include_declaration,
            )?))
        }
        "symbol.hover" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                line: u32,
                character: u32,
            }
            let a: Args = serde_json::from_value(args)?;
            let text = state.symbol_hover(a.buffer_id, a.line, a.character)?;
            Ok(json!({ "text": text }))
        }
        "buffer.version" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            let v = state.buffer_version(a.buffer_id)?;
            Ok(json!({ "version": v }))
        }
        "proposals.count" => Ok(json!({ "count": state.proposals_count() })),
        "symbol.workspace_search" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                query: String,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.symbol_workspace_search(a.buffer_id, &a.query)?))
        }
        "diag.current" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.diag_current(a.buffer_id)?))
        }
        "tasks.list" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.tasks_list(a.buffer_id)?))
        }
        "diag.wait_until_idle" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                #[serde(default = "default_diag_timeout_ms")]
                timeout_ms: u64,
            }
            fn default_diag_timeout_ms() -> u64 {
                3000
            }
            let a: Args = serde_json::from_value(args)?;
            let result = state.diag_wait_until_idle(
                a.buffer_id,
                std::time::Duration::from_millis(a.timeout_ms),
            )?;
            Ok(json!(result))
        }
        "edit.rename_symbol" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
                version: u64,
                line: u32,
                character: u32,
                new_name: String,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.edit_rename_symbol(
                a.buffer_id,
                a.version,
                a.line,
                a.character,
                a.new_name,
            )?))
        }
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    }
}

fn call_tool_result(outcome: Result<Value>) -> Value {
    match outcome {
        Ok(value) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
            }],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{
                "type": "text",
                "text": e.to_string(),
            }],
            "isError": true,
        }),
    }
}

// ---------- JSON-RPC envelope helpers ----------

fn success_response(id: Value, result: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SOLE_BUFFER_ID;

    fn fresh_state(name: &str) -> ProtocolState {
        let path = std::env::temp_dir()
            .join(format!("dyad_mcp_{}_{}.rs", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        ProtocolState::open(path).unwrap()
    }

    fn request(method: &str, params: Value, id: u64) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string()
    }

    fn call_tool(state: &mut ProtocolState, name: &str, args: Value, id: u64) -> Value {
        let line = request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            id,
        );
        let response = handle_line(state, &line).expect("tools/call must reply");
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn initialize_responds_with_server_info() {
        let mut state = fresh_state("initialize");
        let response = handle_line(&mut state, &request("initialize", json!({}), 1)).unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "dyad");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialized_notification_returns_no_response() {
        let mut state = fresh_state("initialized");
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })
        .to_string();
        assert!(handle_line(&mut state, &notification).is_none());
    }

    #[test]
    fn tools_list_includes_each_protocol_verb() {
        let mut state = fresh_state("tools_list");
        let response = handle_line(&mut state, &request("tools/list", json!({}), 2)).unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "buffer.list",
            "buffer.read",
            "buffer.version",
            "ast.query",
            "edit.replace_range",
            "edit.replace_node",
            "tx.begin",
            "tx.commit",
            "tx.rollback",
            "history.recent",
            "symbol.definition",
            "symbol.references",
            "symbol.hover",
            "symbol.workspace_search",
            "diag.current",
            "edit.rename_symbol",
            "buffer.open",
            "buffer.close",
            "clients.list",
            "git.diff",
            "git.status",
            "git.log",
            "git.show",
            "git.stage",
            "git.unstage",
            "git.commit",
            "edit.propose_range",
            "proposals.list",
            "proposals.accept",
            "proposals.reject",
            "proposals.count",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn unknown_method_yields_minus_32601() {
        let mut state = fresh_state("unknown");
        let response = handle_line(&mut state, &request("does/not/exist", json!({}), 3)).unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn parse_error_yields_minus_32700() {
        let mut state = fresh_state("parse_err");
        let response = handle_line(&mut state, "{this is not json").unwrap();
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn end_to_end_buffer_read_round_trip() {
        let mut state = fresh_state("buffer_read");
        // Seed via the protocol layer to keep this test purely MCP-shaped.
        let v0 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v0,
                CharRange { start: 0, end: 0 },
                "hello world",
            )
            .unwrap();

        let response = call_tool(
            &mut state,
            "buffer.read",
            json!({"buffer_id": SOLE_BUFFER_ID}),
            42,
        );
        let payload: Value = serde_json::from_str(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["text"], "hello world");
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn end_to_end_edit_then_history() {
        let mut state = fresh_state("edit_history");
        // edit.replace_range via MCP.
        let v0 = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        let edit_resp = call_tool(
            &mut state,
            "edit.replace_range",
            json!({
                "buffer_id": SOLE_BUFFER_ID,
                "version": v0,
                "range": {"start": 0, "end": 0},
                "text": "hi",
            }),
            10,
        );
        assert_eq!(edit_resp["result"]["isError"], false);

        // history.recent should have one entry: the auto-tx for the edit.
        let hist_resp = call_tool(
            &mut state,
            "history.recent",
            json!({"limit": 10}),
            11,
        );
        let history: Value = serde_json::from_str(
            hist_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let entries = history.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]["intent"]
                .as_str()
                .unwrap()
                .contains("edit.replace_range")
        );
    }

    #[test]
    fn buffer_version_tool_returns_current_version_after_edit() {
        let mut state = fresh_state("buffer_version_tool");
        let v_pre = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .edit_replace_range(
                SOLE_BUFFER_ID,
                v_pre,
                CharRange { start: 0, end: 0 },
                "hi",
            )
            .unwrap();

        let resp = call_tool(
            &mut state,
            "buffer.version",
            json!({ "buffer_id": SOLE_BUFFER_ID }),
            21,
        );
        assert_eq!(resp["result"]["isError"], false);
        let payload: Value = serde_json::from_str(
            resp["result"]["content"][0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(payload["version"], state.buffer_version(SOLE_BUFFER_ID).unwrap());
        assert!(payload["version"].as_u64().unwrap() > v_pre);
    }

    #[test]
    fn proposals_count_tool_reflects_queue_size() {
        let mut state = fresh_state("proposals_count_tool");
        // Empty queue.
        let empty = call_tool(&mut state, "proposals.count", json!({}), 30);
        let body: Value = serde_json::from_str(
            empty["result"]["content"][0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(body["count"], 0);

        // Enqueue one proposal.
        let v = state.buffer_version(SOLE_BUFFER_ID).unwrap();
        state
            .propose_replace_range(
                SOLE_BUFFER_ID,
                v,
                CharRange { start: 0, end: 0 },
                "x".into(),
                "test".into(),
            )
            .unwrap();
        let one = call_tool(&mut state, "proposals.count", json!({}), 31);
        let body: Value = serde_json::from_str(
            one["result"]["content"][0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(body["count"], 1);
    }

    #[test]
    fn version_mismatch_returns_is_error_true() {
        let mut state = fresh_state("version_err");
        let bad_version = state.buffer_version(SOLE_BUFFER_ID).unwrap() + 99;
        let resp = call_tool(
            &mut state,
            "edit.replace_range",
            json!({
                "buffer_id": SOLE_BUFFER_ID,
                "version": bad_version,
                "range": {"start": 0, "end": 0},
                "text": "x",
            }),
            7,
        );
        assert_eq!(resp["result"]["isError"], true);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("version mismatch")
        );
    }
}
