//! End-to-end integration tests for the dyad MCP server.
//!
//! These spin up `target/.../dyad --mcp <fixture>` as a subprocess (via
//! the `CARGO_BIN_EXE_dyad` env var Cargo sets for integration tests)
//! and drive it with line-delimited JSON-RPC messages over stdin/stdout.
//! This is the same protocol surface `scripts/mcp-smoke.sh` exercises,
//! lifted into `cargo test` so it runs alongside the rest of the suite.
//!
//! Each test owns a fresh fixture file in `std::env::temp_dir()` and
//! launches its own subprocess, so tests can run in parallel without
//! sharing state.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Cargo sets this env var for integration tests; it points at the
/// freshly-built binary we want to drive.
const DYAD_BIN: &str = env!("CARGO_BIN_EXE_dyad");

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    fixture: PathBuf,
}

impl McpSession {
    fn start(seed: &str, label: &str) -> Self {
        let fixture = unique_fixture(label);
        std::fs::write(&fixture, seed).expect("write fixture");
        let mut child = Command::new(DYAD_BIN)
            .arg("--mcp")
            .arg(&fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dyad --mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, stdin, stdout, fixture }
    }

    fn send(&mut self, msg: &Value) {
        let line = msg.to_string();
        writeln!(self.stdin, "{line}").expect("send");
        self.stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("recv");
        serde_json::from_str(line.trim()).expect("parse json response")
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.recv()
    }

    fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.call(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        // Closing stdin makes the server exit its read loop cleanly.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.fixture);
    }
}

fn unique_fixture(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "dyad_it_{label}_{pid}_{nanos}.rs",
        pid = std::process::id(),
    ))
}

/// Unwrap the inner JSON payload of a CallToolResult `content` array.
fn tool_payload(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    serde_json::from_str(text).expect("tool result body is JSON")
}

#[test]
fn initialize_returns_server_info_and_tools_list() {
    let mut s = McpSession::start("fn hello() {}\n", "init");

    let init = s.call(1, "initialize", json!({}));
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "dyad");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    let tools = s.call(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for required in [
        "buffer.read",
        "edit.replace_range",
        "edit.replace_node",
        "ast.query",
        "tx.begin",
        "tx.commit",
        "tx.rollback",
        "history.recent",
        "buffer.open",
        "buffer.close",
        "buffer.list",
        "clients.list",
        "edit.propose_range",
        "proposals.list",
        "proposals.accept",
        "proposals.reject",
    ] {
        assert!(names.contains(&required), "missing tool {required}");
    }
}

#[test]
fn read_then_edit_then_read_round_trips() {
    let mut s = McpSession::start("fn hello() {}\n", "round_trip");
    let _ = s.call(1, "initialize", json!({}));

    let read_pre = s.call_tool(2, "buffer.read", json!({ "buffer_id": 1 }));
    let pre = tool_payload(&read_pre);
    assert_eq!(pre["text"], "fn hello() {}\n");

    // Replace "hello" (chars 3..8) with "farewell".
    let v0 = pre["version"].as_u64().unwrap();
    let edit = s.call_tool(
        3,
        "edit.replace_range",
        json!({
            "buffer_id": 1,
            "version": v0,
            "range": { "start": 3, "end": 8 },
            "text": "farewell",
        }),
    );
    assert_eq!(edit["result"]["isError"], false);

    let read_post = s.call_tool(4, "buffer.read", json!({ "buffer_id": 1 }));
    let post = tool_payload(&read_post);
    assert_eq!(post["text"], "fn farewell() {}\n");
    assert!(post["version"].as_u64().unwrap() > v0);
}

#[test]
fn version_mismatch_is_surfaced_as_tool_error() {
    let mut s = McpSession::start("fn x() {}\n", "version");
    let _ = s.call(1, "initialize", json!({}));
    let resp = s.call_tool(
        2,
        "edit.replace_range",
        json!({
            "buffer_id": 1,
            "version": 999,
            "range": { "start": 0, "end": 0 },
            "text": "x",
        }),
    );
    assert_eq!(resp["result"]["isError"], true);
    let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(msg.contains("version mismatch"), "got: {msg}");
}

#[test]
fn ast_query_finds_function_names_through_jsonrpc() {
    let mut s = McpSession::start("fn alpha() {}\nfn beta() {}\n", "ast");
    let _ = s.call(1, "initialize", json!({}));
    let resp = s.call_tool(
        2,
        "ast.query",
        json!({
            "buffer_id": 1,
            "query": "(function_item name: (identifier) @name)",
        }),
    );
    assert_eq!(resp["result"]["isError"], false);
    let matches = tool_payload(&resp);
    let names: Vec<&str> = matches
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["capture"] == "name")
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    // tree-sitter-rust labels function-name identifiers with `identifier`.
    assert!(!names.is_empty());
}

#[test]
fn transaction_commit_collapses_multiple_edits_into_one_history_entry() {
    let mut s = McpSession::start("", "tx");
    let _ = s.call(1, "initialize", json!({}));

    let begin = s.call_tool(
        2,
        "tx.begin",
        json!({ "buffer_id": 1, "intent": "compose" }),
    );
    let tx_id = tool_payload(&begin)["tx_id"].as_u64().unwrap();

    let v0 = tool_payload(&s.call_tool(3, "buffer.read", json!({ "buffer_id": 1 })))
        ["version"]
        .as_u64()
        .unwrap();
    let edit1 = s.call_tool(
        4,
        "edit.replace_range",
        json!({
            "buffer_id": 1,
            "version": v0,
            "range": { "start": 0, "end": 0 },
            "text": "fn a() {}\n",
        }),
    );
    let v1 = tool_payload(&edit1)["version"].as_u64().unwrap();
    let _ = s.call_tool(
        5,
        "edit.replace_range",
        json!({
            "buffer_id": 1,
            "version": v1,
            "range": { "start": 0, "end": 0 },
            "text": "// intro\n",
        }),
    );
    let _commit = s.call_tool(6, "tx.commit", json!({ "tx_id": tx_id }));

    let hist = s.call_tool(7, "history.recent", json!({ "limit": 10 }));
    let entries = tool_payload(&hist);
    let arr = entries.as_array().unwrap();
    assert_eq!(arr.len(), 1, "single history entry for the explicit tx");
    assert_eq!(arr[0]["intent"], "compose");
}

#[test]
fn tx_rollback_restores_text_and_drops_history_entry() {
    let mut s = McpSession::start("seed\n", "rollback");
    let _ = s.call(1, "initialize", json!({}));

    let begin = s.call_tool(2, "tx.begin", json!({ "buffer_id": 1, "intent": "doomed" }));
    let tx_id = tool_payload(&begin)["tx_id"].as_u64().unwrap();
    let v0 = tool_payload(&s.call_tool(3, "buffer.read", json!({ "buffer_id": 1 })))
        ["version"].as_u64().unwrap();
    let _ = s.call_tool(
        4,
        "edit.replace_range",
        json!({
            "buffer_id": 1,
            "version": v0,
            "range": { "start": 0, "end": 0 },
            "text": "junk ",
        }),
    );
    let _ = s.call_tool(5, "tx.rollback", json!({ "tx_id": tx_id }));

    let after = tool_payload(&s.call_tool(6, "buffer.read", json!({ "buffer_id": 1 })));
    assert_eq!(after["text"], "seed\n");

    let hist = tool_payload(&s.call_tool(7, "history.recent", json!({ "limit": 10 })));
    assert!(hist.as_array().unwrap().is_empty());
}

#[test]
fn proposal_lifecycle_accept_replaces_text_with_recorded_intent() {
    let mut s = McpSession::start("fn hello() {}\n", "propose");
    let _ = s.call(1, "initialize", json!({}));

    let v0 = tool_payload(&s.call_tool(2, "buffer.read", json!({ "buffer_id": 1 })))
        ["version"].as_u64().unwrap();

    let prop = s.call_tool(
        3,
        "edit.propose_range",
        json!({
            "buffer_id": 1,
            "version": v0,
            "range": { "start": 3, "end": 8 },
            "text": "farewell",
            "intent": "rename hello -> farewell",
        }),
    );
    let id = tool_payload(&prop)["proposal_id"].as_u64().unwrap();

    // The proposal is queued.
    let list = tool_payload(&s.call_tool(4, "proposals.list", json!({})));
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Accept applies it and drains the queue.
    let _ = s.call_tool(5, "proposals.accept", json!({ "proposal_id": id }));
    let post = tool_payload(&s.call_tool(6, "buffer.read", json!({ "buffer_id": 1 })));
    assert_eq!(post["text"], "fn farewell() {}\n");
    let drained = tool_payload(&s.call_tool(7, "proposals.list", json!({})));
    assert!(drained.as_array().unwrap().is_empty());

    // History carries the proposal's intent.
    let hist = tool_payload(&s.call_tool(8, "history.recent", json!({ "limit": 10 })));
    let last_intent = hist.as_array().unwrap().last().unwrap()["intent"]
        .as_str()
        .unwrap();
    assert_eq!(last_intent, "rename hello -> farewell");
}

#[test]
fn multi_buffer_open_and_list_assigns_ascending_ids() {
    let mut s = McpSession::start("fn one() {}\n", "multi");
    let _ = s.call(1, "initialize", json!({}));

    let second = unique_fixture("multi_b");
    std::fs::write(&second, "fn two() {}\n").unwrap();

    let resp = s.call_tool(
        2,
        "buffer.open",
        json!({ "path": second.display().to_string() }),
    );
    let new_id = tool_payload(&resp)["buffer_id"].as_u64().unwrap();
    assert_eq!(new_id, 2);

    let list = tool_payload(&s.call_tool(3, "buffer.list", json!({})));
    let ids: Vec<u64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2]);

    let _ = std::fs::remove_file(&second);
}

#[test]
fn parse_error_returns_minus_32700_with_id_null() {
    let mut s = McpSession::start("", "parse");
    let _ = s.call(1, "initialize", json!({}));
    // Send raw garbage (not JSON).
    writeln!(s.stdin, "{{this is not valid").unwrap();
    s.stdin.flush().unwrap();
    let mut line = String::new();
    s.stdout.read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["error"]["code"], -32700);
    assert!(v["id"].is_null());
}

#[test]
fn unknown_method_returns_minus_32601() {
    let mut s = McpSession::start("", "unknown");
    let _ = s.call(1, "initialize", json!({}));
    let v = s.call(2, "does/not/exist", json!({}));
    assert_eq!(v["error"]["code"], -32601);
}

#[test]
fn initialized_notification_returns_no_response() {
    // Notifications (no "id" field) get no reply per JSON-RPC.
    let mut s = McpSession::start("", "notify");
    let _ = s.call(1, "initialize", json!({}));
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    }));
    // Follow up with a real call; the next line we read must be that
    // call's response (proving the notification ate no output).
    let v = s.call(2, "tools/list", json!({}));
    assert_eq!(v["id"], 2);
    assert!(v["result"]["tools"].is_array());
}
