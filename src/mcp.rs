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
            tool_def("tx.begin", "Open a transaction with a stated intent. Subsequent edits join this tx until tx.commit or tx.rollback.", json!({
                "type": "object",
                "required": ["intent"],
                "properties": {
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
            tool_def("symbol.definition", "Find the LSP definition location for a symbol at the given zero-based line/character. Requires rust-analyzer.", json!({
                "type": "object",
                "required": ["buffer_id", "line", "character"],
                "properties": {
                    "buffer_id": {"type": "integer"},
                    "line":      {"type": "integer", "minimum": 0},
                    "character": {"type": "integer", "minimum": 0},
                },
            })),
            tool_def("diag.current", "Return cached LSP diagnostics for a buffer (severity 1=error..4=hint). Requires rust-analyzer.", json!({
                "type": "object",
                "required": ["buffer_id"],
                "properties": {
                    "buffer_id": {"type": "integer"},
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
                intent: String,
                conversation_id: Option<String>,
            }
            let a: Args = serde_json::from_value(args)?;
            let tx_id = state.tx_begin(a.intent, a.conversation_id)?;
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
        "diag.current" => {
            #[derive(Deserialize)]
            struct Args {
                buffer_id: u64,
            }
            let a: Args = serde_json::from_value(args)?;
            Ok(json!(state.diag_current(a.buffer_id)?))
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
            "ast.query",
            "edit.replace_range",
            "edit.replace_node",
            "tx.begin",
            "tx.commit",
            "tx.rollback",
            "history.recent",
            "symbol.definition",
            "diag.current",
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
        let v0 = state.buffer_version();
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
        let v0 = state.buffer_version();
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
    fn version_mismatch_returns_is_error_true() {
        let mut state = fresh_state("version_err");
        let bad_version = state.buffer_version() + 99;
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
