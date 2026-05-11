//! Disk-touching integration tests that aren't strictly MCP-shaped.
//!
//! These spin up the binary as a subprocess (same pattern as
//! `tests/mcp_integration.rs`) and verify behavior that crosses
//! module boundaries — buffer + filesystem + protocol — through the
//! actual binary, not via unit-test scaffolding.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const DYAD_BIN: &str = env!("CARGO_BIN_EXE_dyad");

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    fixture: PathBuf,
}

impl Session {
    fn start(seed: &str, label: &str) -> Self {
        let fixture = unique(label, "rs");
        std::fs::write(&fixture, seed).unwrap();
        let mut child = Command::new(DYAD_BIN)
            .arg("--mcp")
            .arg(&fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, fixture }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        writeln!(
            self.stdin,
            "{}",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
        )
        .unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn call_tool(&mut self, id: u64, name: &str, args: Value) -> Value {
        self.call(
            id,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        )
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.fixture);
    }
}

fn unique(label: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "dyad_bio_{label}_{pid}_{nanos}.{ext}",
        pid = std::process::id(),
    ))
}

fn payload(resp: &Value) -> Value {
    let s = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(s).unwrap()
}

#[test]
fn opening_a_missing_file_yields_an_empty_buffer() {
    // Start with a path that does not exist on disk.
    let label = "missing";
    let path = unique(label, "rs");
    assert!(!path.exists());

    let mut child = Command::new(DYAD_BIN)
        .arg("--mcp")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"buffer.read","arguments":{"buffer_id":1}}})
    )
    .unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    let body = payload(&resp);
    assert_eq!(body["text"], "");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn edit_replace_node_through_protocol_rewrites_identifier() {
    let mut s = Session::start("fn hello() {}\n", "replace_node");
    let _ = s.call(1, "initialize", json!({}));

    // Find the function-name node first.
    let q = s.call_tool(
        2,
        "ast.query",
        json!({
            "buffer_id": 1,
            "query": "(function_item name: (identifier) @name)",
        }),
    );
    let matches = payload(&q);
    let name = matches
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["capture"] == "name")
        .expect("function name match");
    let bs = name["byte_start"].as_u64().unwrap();
    let be = name["byte_end"].as_u64().unwrap();

    let v0 = payload(&s.call_tool(3, "buffer.read", json!({ "buffer_id": 1 })))
        ["version"].as_u64().unwrap();
    let _ = s.call_tool(
        4,
        "edit.replace_node",
        json!({
            "buffer_id": 1,
            "version": v0,
            "byte_range": { "start": bs, "end": be },
            "text": "farewell",
        }),
    );

    let post = payload(&s.call_tool(5, "buffer.read", json!({ "buffer_id": 1 })));
    assert_eq!(post["text"], "fn farewell() {}\n");
}

#[test]
fn buffer_read_with_range_returns_substring() {
    let mut s = Session::start("abcdefgh\n", "range");
    let _ = s.call(1, "initialize", json!({}));
    let part = payload(&s.call_tool(
        2,
        "buffer.read",
        json!({ "buffer_id": 1, "range": { "start": 2, "end": 6 } }),
    ));
    assert_eq!(part["text"], "cdef");
}

#[test]
fn buffer_read_out_of_range_errors() {
    let mut s = Session::start("ab\n", "out_of_range");
    let _ = s.call(1, "initialize", json!({}));
    let resp = s.call_tool(
        2,
        "buffer.read",
        json!({ "buffer_id": 1, "range": { "start": 0, "end": 99 } }),
    );
    assert_eq!(resp["result"]["isError"], true);
}

#[test]
fn clients_list_reports_a_single_agent_session() {
    let mut s = Session::start("fn x() {}\n", "clients");
    let _ = s.call(1, "initialize", json!({}));
    let resp = s.call_tool(2, "clients.list", json!({}));
    let clients = payload(&resp);
    let arr = clients.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["kind"], "agent");
    assert_eq!(arr[0]["focus"], 1);
}

#[test]
fn buffer_close_drops_id_and_shifts_focus() {
    let mut s = Session::start("fn one() {}\n", "close_first");
    let _ = s.call(1, "initialize", json!({}));

    let second = unique("close_second", "rs");
    std::fs::write(&second, "fn two() {}\n").unwrap();
    let open = s.call_tool(
        2,
        "buffer.open",
        json!({ "path": second.display().to_string() }),
    );
    assert_eq!(payload(&open)["buffer_id"], 2);

    let _ = s.call_tool(3, "buffer.close", json!({ "buffer_id": 2 }));
    let list = payload(&s.call_tool(4, "buffer.list", json!({})));
    let ids: Vec<u64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![1]);

    let _ = std::fs::remove_file(&second);
}
