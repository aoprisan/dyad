//! Phase 6 — LSP client for `rust-analyzer`.
//!
//! Spawns rust-analyzer as a child process, speaks JSON-RPC 2.0 over
//! its stdin/stdout with the standard LSP Content-Length framing,
//! tracks publishDiagnostics into a cache, and answers
//! `textDocument/definition`. Other tools (references, hover,
//! completion) can be added without restructuring — see Phase 6 design
//! choice in DESIGN.md.
//!
//! Threading model:
//!   - Main thread holds the `LspClient` and writes requests over
//!     stdin (mutex-guarded).
//!   - A reader thread reads framed LSP messages from stdout. For
//!     responses it looks up the pending oneshot Sender by id and
//!     forwards the result. For notifications it updates the shared
//!     diagnostics cache.
//!
//! The client is fail-graceful: if rust-analyzer isn't on PATH or
//! initialize times out, `spawn_rust` returns `Err` and the caller
//! continues without LSP features (see `ProtocolState::open`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u8>,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

struct LspState {
    pending: HashMap<i64, Sender<Value>>,
    diagnostics: HashMap<String, Vec<Diagnostic>>,
}

pub struct LspClient {
    state: Arc<Mutex<LspState>>,
    next_id: AtomicI64,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    // Thread is detached on drop — the reader exits when stdout closes
    // (which happens when we kill the child below).
    _reader: JoinHandle<()>,
}

impl LspClient {
    /// Spawn rust-analyzer for `workspace_root`, run initialize, send
    /// initialized, and didOpen the seed file. Returns the client once
    /// the initialize response arrives — typically a few seconds on
    /// first launch.
    pub fn spawn_rust(
        workspace_root: &Path,
        file_uri: &str,
        initial_text: &str,
    ) -> Result<Self> {
        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning rust-analyzer (is `rustup component add rust-analyzer` done?)")?;
        let stdin = child.stdin.take().context("rust-analyzer stdin unavailable")?;
        let stdout = child.stdout.take().context("rust-analyzer stdout unavailable")?;
        let state = Arc::new(Mutex::new(LspState {
            pending: HashMap::new(),
            diagnostics: HashMap::new(),
        }));
        let reader_state = Arc::clone(&state);
        let reader = std::thread::spawn(move || reader_loop(stdout, reader_state));
        let client = Self {
            state,
            next_id: AtomicI64::new(1),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            _reader: reader,
        };
        client.initialize(workspace_root)?;
        client.notify("initialized", json!({}))?;
        client.did_open(file_uri, "rust", initial_text)?;
        Ok(client)
    }

    fn initialize(&self, workspace_root: &Path) -> Result<()> {
        let root_uri = path_to_uri(workspace_root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": false },
                    "definition": { "linkSupport": false },
                    "hover": {},
                    "synchronization": {
                        "didSave": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                    },
                },
            },
            "clientInfo": { "name": "dyad" },
        });
        // rust-analyzer can take 10+ seconds on first launch — large.
        self.request("initialize", params, Duration::from_secs(30))?;
        Ok(())
    }

    pub fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 0,
                    "text": text,
                }
            }),
        )
    }

    pub fn did_change(&self, uri: &str, version: i32, text: &str) -> Result<()> {
        // Full-document sync — simpler than computing deltas and good
        // enough for one-file workflows. Incremental sync is a worthwhile
        // follow-up if perf becomes a concern.
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    pub fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let result = self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
            Duration::from_secs(10),
        )?;
        // Per the LSP spec the response is Location | Location[] | null.
        match result {
            Value::Null => Ok(Vec::new()),
            Value::Array(_) => Ok(serde_json::from_value(result)?),
            Value::Object(_) => {
                let loc: Location = serde_json::from_value(result)?;
                Ok(vec![loc])
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn diagnostics(&self, uri: &str) -> Vec<Diagnostic> {
        self.state
            .lock()
            .unwrap()
            .diagnostics
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    // ---------- Wire layer ----------

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        use std::sync::mpsc::RecvTimeoutError;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel::<Value>();
        {
            let mut s = self.state.lock().unwrap();
            s.pending.insert(id, tx);
        }
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&msg)?;
        rx.recv_timeout(timeout).map_err(|e| match e {
            RecvTimeoutError::Timeout => anyhow!("lsp request `{method}` timed out"),
            RecvTimeoutError::Disconnected => {
                anyhow!("lsp server exited before answering `{method}`")
            }
        })
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&msg)
    }

    fn send(&self, msg: &Value) -> Result<()> {
        let body = serde_json::to_vec(msg)?;
        let mut stdin = self.stdin.lock().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        stdin.write_all(&body)?;
        stdin.flush()?;
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort polite shutdown, then kill if needed.
        let _ = self.notify("exit", json!(null));
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn reader_loop(stdout: ChildStdout, state: Arc<Mutex<LspState>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let msg = match read_message(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => break, // clean EOF
            Err(_) => break,   // malformed frame — bail rather than spin
        };
        if let Some(id) = msg.get("id").and_then(Value::as_i64) {
            let result = msg.get("result").cloned().unwrap_or(Value::Null);
            let mut s = state.lock().unwrap();
            if let Some(tx) = s.pending.remove(&id) {
                let _ = tx.send(result);
            }
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            // The only server-to-client notification we consume is
            // publishDiagnostics; window/logMessage and progress
            // updates are intentionally dropped.
            if method == "textDocument/publishDiagnostics"
                && let Some(params) = msg.get("params")
            {
                update_diagnostics(&state, params);
            }
        }
    }
    // The server's stdout is gone. Drop all pending Senders so any
    // outstanding request wakes up with a Disconnected error instead
    // of waiting out its full timeout — this is what saves us when
    // rust-analyzer dies immediately (rustup-proxy without the
    // component installed, or a missing binary).
    state.lock().unwrap().pending.clear();
}

fn update_diagnostics(state: &Arc<Mutex<LspState>>, params: &Value) {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return;
    };
    let diags: Vec<Diagnostic> = params
        .get("diagnostics")
        .and_then(|v| serde_json::from_value::<Vec<Diagnostic>>(v.clone()).ok())
        .unwrap_or_default();
    state
        .lock()
        .unwrap()
        .diagnostics
        .insert(uri.to_string(), diags);
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line_trimmed = line.trim_end_matches(['\r', '\n']);
        if line_trimmed.is_empty() {
            break;
        }
        if let Some(rest) = line_trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                rest.trim()
                    .parse()
                    .context("invalid Content-Length")?,
            );
        }
        // Other headers (Content-Type) are tolerated and ignored.
    }
    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

/// Encode a filesystem path as a `file://` URI. Uses absolute form via
/// `canonicalize` when possible.
pub fn path_to_uri(path: &Path) -> String {
    let abs = path
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path));
    format!("file://{}", abs.display())
}

/// Walk upward from `path` looking for the nearest directory that
/// contains a `Cargo.toml`. Falls back to `path`'s parent if none is
/// found — rust-analyzer copes either way, just with less context.
pub fn workspace_root_for(path: &Path) -> PathBuf {
    let mut cur = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    loop {
        if cur.join("Cargo.toml").exists() {
            return cur;
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => {
                return path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(body: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        v.extend_from_slice(body.as_bytes());
        v
    }

    #[test]
    fn read_message_parses_single_frame() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let bytes = framed(body);
        let mut reader = Cursor::new(bytes);
        let msg = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(msg["id"], 1);
    }

    #[test]
    fn read_message_handles_multiple_frames_back_to_back() {
        let a = r#"{"jsonrpc":"2.0","method":"a"}"#;
        let b = r#"{"jsonrpc":"2.0","method":"b"}"#;
        let mut bytes = framed(a);
        bytes.extend(framed(b));
        let mut reader = Cursor::new(bytes);
        let m1 = read_message(&mut reader).unwrap().unwrap();
        let m2 = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(m1["method"], "a");
        assert_eq!(m2["method"], "b");
        // Third read should be EOF.
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn read_message_tolerates_unknown_headers() {
        let body = r#"{"jsonrpc":"2.0","id":7}"#;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n");
        bytes.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        bytes.extend_from_slice(body.as_bytes());
        let mut reader = Cursor::new(bytes);
        let msg = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(msg["id"], 7);
    }

    #[test]
    fn update_diagnostics_writes_into_cache() {
        let state = Arc::new(Mutex::new(LspState {
            pending: HashMap::new(),
            diagnostics: HashMap::new(),
        }));
        let params = json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [
                {
                    "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}},
                    "severity": 1,
                    "message": "boom",
                    "source": "rustc"
                }
            ],
        });
        update_diagnostics(&state, &params);
        let cached = state
            .lock()
            .unwrap()
            .diagnostics
            .get("file:///tmp/x.rs")
            .cloned()
            .unwrap_or_default();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].message, "boom");
        assert_eq!(cached[0].severity, Some(1));
    }

    #[test]
    fn workspace_root_walks_up_to_cargo_toml() {
        // The dyad project itself has Cargo.toml at its root.
        let here = std::env::current_dir().unwrap();
        let nested = here.join("src/main.rs");
        let root = workspace_root_for(&nested);
        assert!(root.join("Cargo.toml").exists(), "found {root:?}");
    }
}
