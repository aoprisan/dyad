//! Phase 6 — generic LSP client.
//!
//! Spawns a language server (rust-analyzer, Metals, …) as a child
//! process, speaks JSON-RPC 2.0 over its stdin/stdout with the standard
//! LSP Content-Length framing, tracks publishDiagnostics into a cache,
//! and answers `textDocument/{definition, hover, rename}`. The per-
//! language details (binary name, languageId, init capabilities,
//! workspace markers, install hint) come from [`crate::language::Language`].
//!
//! Threading model:
//!   - Main thread holds the `LspClient` and writes requests over
//!     stdin (mutex-guarded).
//!   - A reader thread reads framed LSP messages from stdout. For
//!     responses it looks up the pending oneshot Sender by id and
//!     forwards the result. For notifications it updates the shared
//!     diagnostics cache.
//!
//! The client is fail-graceful: if the binary isn't on PATH or
//! initialize times out, `spawn` returns `Err` and the caller continues
//! without LSP features (see `ProtocolState::open`).

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

use crate::language::Language;

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

/// One range-replacement inside a [`WorkspaceEdit`]. LSP positions are
/// line + UTF-16 code units within the line.
#[derive(Deserialize, Clone, Debug)]
pub struct TextEdit {
    pub range: Range,
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// Server response to `textDocument/rename`. We only consume the
/// `changes` map for now — `documentChanges` (the versioned variant)
/// is not negotiated in our `initialize` capabilities, so rust-analyzer
/// falls back to `changes`.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct WorkspaceEdit {
    #[serde(default)]
    pub changes: std::collections::HashMap<String, Vec<TextEdit>>,
}

/// Subset of LSP `SymbolInformation` / `WorkspaceSymbol` used by
/// `workspace/symbol`. Both spec variants share `name`, `kind`, and a
/// `location` whose `uri`+`range` we can navigate to; that's the only
/// payload the type-search dialog needs.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SymbolInformation {
    pub name: String,
    pub kind: u32,
    pub location: Location,
    #[serde(rename = "containerName", default)]
    pub container_name: Option<String>,
}

struct LspState {
    pending: HashMap<i64, Sender<Value>>,
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// `true` once rust-analyzer reports it has finished indexing
    /// (`experimental/serverStatus` with `quiescent: true`). Starts
    /// `false` — newly spawned servers are always still indexing.
    /// Only servers whose `Language::supports_quiescent_status()` is
    /// true ever flip this; for the rest `is_indexing()` short-circuits.
    quiescent: bool,
}

pub struct LspClient {
    language: Language,
    state: Arc<Mutex<LspState>>,
    next_id: AtomicI64,
    /// Shared with the reader thread so server-initiated requests
    /// (`workspace/configuration`, `client/registerCapability`, …) can
    /// be auto-acked without bouncing through the main thread. We don't
    /// actually serve any of these, but the server hangs waiting on
    /// them if we don't reply.
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    // Thread is detached on drop — the reader exits when stdout closes
    // (which happens when we kill the child below).
    _reader: JoinHandle<()>,
}

impl LspClient {
    /// Spawn the language server for `language`, run initialize, send
    /// initialized, and didOpen the seed file. Returns the client once
    /// the initialize response arrives. Each language gets its own
    /// stderr log at `/tmp/dyad-lsp-{lang}.log`.
    pub fn spawn(
        language: Language,
        workspace_root: &Path,
        file_uri: &str,
        initial_text: &str,
    ) -> Result<Self> {
        let log_path = format!("/tmp/dyad-lsp-{}.log", language.display_name());
        let log_file = std::fs::File::create(&log_path)
            .with_context(|| format!("opening {log_path} for {} stderr", language.lsp_binary()))?;
        let binary = language.lsp_binary();
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log_file))
            .spawn()
            .with_context(|| {
                format!(
                    "spawning {binary} (try `{}`)",
                    language.install_hint()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .with_context(|| format!("{binary} stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("{binary} stdout unavailable"))?;
        let state = Arc::new(Mutex::new(LspState {
            pending: HashMap::new(),
            diagnostics: HashMap::new(),
            quiescent: false,
        }));
        let stdin = Arc::new(Mutex::new(stdin));
        let reader_state = Arc::clone(&state);
        let reader_stdin = Arc::clone(&stdin);
        let reader = std::thread::spawn(move || reader_loop(stdout, reader_state, reader_stdin));
        let client = Self {
            language,
            state,
            next_id: AtomicI64::new(1),
            stdin,
            child: Mutex::new(child),
            _reader: reader,
        };
        client.initialize(workspace_root)?;
        client.notify("initialized", json!({}))?;
        client.did_open(file_uri, language.lsp_language_id(), initial_text)?;
        Ok(client)
    }

    /// Which language this client was spawned for. Surfaced via
    /// `is_indexing`, error messages, and per-language routing in
    /// `App` / `ProtocolState`.
    pub fn language(&self) -> Language {
        self.language
    }

    fn initialize(&self, workspace_root: &Path) -> Result<()> {
        let root_uri = path_to_uri(workspace_root);
        let mut capabilities = json!({
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
        });
        if self.language.advertises_rust_analyzer_server_status() {
            // rust-analyzer extension: emits `experimental/serverStatus`
            // notifications with a `quiescent` flag once indexing is
            // done. We use it to drive the LSP-alive badge state.
            capabilities["experimental"] = json!({ "serverStatusNotification": true });
        }
        let mut params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": capabilities,
            "clientInfo": { "name": "dyad" },
        });
        if let Some(init_opts) = self.language.initialization_options() {
            params["initializationOptions"] = init_opts;
        }
        self.request("initialize", params, self.language.initialize_timeout())?;
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

    pub fn did_close(&self, uri: &str) -> Result<()> {
        self.notify(
            "textDocument/didClose",
            json!({
                "textDocument": { "uri": uri }
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

    /// Run `textDocument/hover` at the given position. Returns the
    /// extracted plain-text body (joined MarkedString / MarkupContent
    /// payloads), or `None` if the server has nothing to say there.
    pub fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Option<String>> {
        let result = self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
            Duration::from_secs(10),
        )?;
        if result.is_null() {
            return Ok(None);
        }
        let Some(contents) = result.get("contents") else {
            return Ok(None);
        };
        let text = stringify_hover_contents(contents);
        Ok(if text.trim().is_empty() {
            None
        } else {
            Some(text)
        })
    }

    /// Run `workspace/symbol` with `query`. Returns the server's
    /// (already fuzzy-ranked) candidate list. The caller filters by
    /// `kind` if it only wants types — we keep the raw response so
    /// MCP clients can pick their own slice.
    pub fn workspace_symbol(&self, query: &str) -> Result<Vec<SymbolInformation>> {
        let result = self.request(
            "workspace/symbol",
            json!({ "query": query }),
            Duration::from_secs(10),
        )?;
        match result {
            Value::Null => Ok(Vec::new()),
            Value::Array(_) => Ok(serde_json::from_value(result)?),
            _ => Ok(Vec::new()),
        }
    }

    /// Ask the server for the workspace edits required to rename the
    /// symbol at the given position to `new_name`. Returns the raw
    /// `WorkspaceEdit`; the caller applies the in-buffer subset and
    /// reports any cross-file changes that need a separate buffer.
    pub fn rename(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<WorkspaceEdit> {
        let result = self.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "newName": new_name,
            }),
            Duration::from_secs(15),
        )?;
        match result {
            Value::Null => Ok(WorkspaceEdit::default()),
            other => Ok(serde_json::from_value(other)?),
        }
    }

    /// `true` while the server is still loading the workspace and
    /// indexing — definition / hover / diagnostics requests can return
    /// empty in this window. For rust-analyzer this tracks
    /// `experimental/serverStatus { quiescent }`; for Metals it
    /// tracks `metals/status` text. Languages that don't track any
    /// status notification report `false` immediately.
    pub fn is_indexing(&self) -> bool {
        if !self.language.tracks_indexing_status() {
            return false;
        }
        !self.state.lock().unwrap().quiescent
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
        write_message(&self.stdin, msg)
    }
}

/// Frame a JSON-RPC message and write it to the LSP server's stdin.
/// Shared between `LspClient::send` and the reader thread's
/// auto-reply path.
fn write_message(stdin: &Mutex<ChildStdin>, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut stdin = stdin.lock().unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
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

fn reader_loop(
    stdout: ChildStdout,
    state: Arc<Mutex<LspState>>,
    stdin: Arc<Mutex<ChildStdin>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let msg = match read_message(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => break, // clean EOF
            Err(_) => break,   // malformed frame — bail rather than spin
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str);

        match (id, method) {
            // Server-initiated request: any message with both id and
            // method. We don't actually serve most of these (config,
            // capability registration, progress tokens, …) but the
            // server hangs if we don't reply, which blocks workspace
            // load and makes definition queries return empty.
            //
            // `window/showMessageRequest` is special: Metals uses it to
            // ask the user whether to import the build. Replying `null`
            // would be read as "Not now" and Metals would never index
            // anything. Auto-pick the affirmative action so first-open
            // diagnostics actually flow.
            (Some(id), Some(m)) => {
                let result = if m == "window/showMessageRequest" {
                    auto_pick_show_message_action(msg.get("params"))
                } else {
                    Value::Null
                };
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                });
                let _ = write_message(&stdin, &reply);
            }
            // Response to one of our pending requests.
            (Some(id), None) => {
                if let Some(id) = id.as_i64() {
                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                    let mut s = state.lock().unwrap();
                    if let Some(tx) = s.pending.remove(&id) {
                        let _ = tx.send(result);
                    }
                }
            }
            // Server notification (no id). We only consume the ones we
            // care about; window/logMessage and `$/progress` are dropped.
            (None, Some(method)) => match method {
                "textDocument/publishDiagnostics" => {
                    if let Some(params) = msg.get("params") {
                        update_diagnostics(&state, params);
                    }
                }
                "experimental/serverStatus" => {
                    // rust-analyzer extension.
                    if let Some(params) = msg.get("params") {
                        update_quiescent(&state, params);
                    }
                }
                "metals/status" => {
                    // Metals extension — same idea, different schema.
                    if let Some(params) = msg.get("params") {
                        update_metals_status(&state, params);
                    }
                }
                _ => {}
            },
            (None, None) => {}
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

/// Handle a rust-analyzer `experimental/serverStatus` notification.
/// The `quiescent` field flips to `true` once the workspace has been
/// loaded and indexed — that's when LSP requests start returning real
/// results instead of empty.
fn update_quiescent(state: &Arc<Mutex<LspState>>, params: &Value) {
    let Some(q) = params.get("quiescent").and_then(Value::as_bool) else {
        return;
    };
    state.lock().unwrap().quiescent = q;
}

/// Handle a Metals `metals/status` notification. Metals broadcasts its
/// indexing / build-import / compilation phases through this — the
/// `text` field carries a human-readable label like "Compiling foo" or
/// "Indexing"; `hide: true` (or empty text) means idle.
///
/// Heuristic: any `text` starting with a known busy verb leaves the
/// server marked as still indexing; anything else (including the bare
/// brand name "Metals") flips it to quiescent. Conservative on the busy
/// side — better to delay a definition lookup than to silently return
/// empty.
fn update_metals_status(state: &Arc<Mutex<LspState>>, params: &Value) {
    let hide = params.get("hide").and_then(Value::as_bool).unwrap_or(false);
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    let busy = !hide
        && ["Indexing", "Compiling", "Importing", "Building", "Loading"]
            .iter()
            .any(|prefix| text.contains(prefix));
    state.lock().unwrap().quiescent = !busy;
}

/// Pick a response for `window/showMessageRequest`. Metals fires this on
/// first open ("Import build?", "Reset build?", …); auto-replying `null`
/// makes the user effectively answer "no", which leaves Metals idle.
/// Prefer a build-import action if present; otherwise pick the first
/// offered action so we never silently decline.
fn auto_pick_show_message_action(params: Option<&Value>) -> Value {
    let Some(params) = params else { return Value::Null };
    let Some(actions) = params.get("actions").and_then(Value::as_array) else {
        return Value::Null;
    };
    if actions.is_empty() {
        return Value::Null;
    }
    actions
        .iter()
        .find(|a| {
            a.get("title")
                .and_then(Value::as_str)
                .map(|t| t.eq_ignore_ascii_case("Import build"))
                .unwrap_or(false)
        })
        .unwrap_or(&actions[0])
        .clone()
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

/// Encode a filesystem path as an absolute `file://` URI. Tries
/// `canonicalize` first (works for existing files); for new files
/// (e.g. opened to be created on first save) falls back to
/// `current_dir().join(path)` so the URI is still absolute. A relative
/// `file://src/main.rs` URI looks valid but rust-analyzer parses the
/// empty/relative root as `/`, which breaks workspace discovery.
/// Walk an LSP hover `contents` payload (MarkedString /
/// MarkedString[] / MarkupContent) and return a flat string. The LSP
/// spec is permissive here; we accept anything with a `value` field,
/// fall back to plain strings, and join arrays with blank lines.
fn stringify_hover_contents(c: &Value) -> String {
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(obj) = c.as_object()
        && let Some(v) = obj.get("value").and_then(|v| v.as_str())
    {
        return v.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .map(stringify_hover_contents)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    String::new()
}

pub fn path_to_uri(path: &Path) -> String {
    let abs = absolutize(path);
    format!("file://{}", abs.display())
}

/// Walk upward from `path` looking for the nearest directory that
/// contains any of the language's workspace markers (e.g. `Cargo.toml`
/// for Rust, `build.sbt` / `build.sc` for Scala). Falls back to
/// `path`'s parent if none is found — language servers cope either
/// way, just with less context.
///
/// Absolutizes `path` up-front: with a relative input like `src/main.rs`
/// the loop would otherwise terminate at an empty `PathBuf`, and a
/// downstream `file://` URI on that empty path makes rust-analyzer log
/// `failed to find any projects in [AbsPathBuf("/")]`.
///
/// Per-language markers keep polyglot repos honest: looking up Scala
/// markers from a `.scala` file walks past any `Cargo.toml` higher up
/// without grabbing it.
pub fn workspace_root_for(path: &Path, language: Language) -> PathBuf {
    let abs = absolutize(path);
    let mut cur = abs
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let markers = language.workspace_markers();
    loop {
        if markers.iter().any(|m| cur.join(m).exists()) {
            return cur;
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => {
                return abs
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
            }
        }
    }
}

/// Best-effort absolute form of `path`. Prefers `canonicalize` (resolves
/// symlinks, errors for non-existent paths); on failure joins onto the
/// current working directory; final fallback keeps the input as-is.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(abs) = path.canonicalize() {
        return abs;
    }
    if path.is_absolute() {
        return PathBuf::from(path);
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => PathBuf::from(path),
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
            quiescent: false,
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
        let root = workspace_root_for(&nested, Language::Rust);
        assert!(root.join("Cargo.toml").exists(), "found {root:?}");
    }

    #[test]
    fn workspace_root_walks_up_to_build_sbt() {
        // Scratch directory: <tmp>/dyad-ws-<pid>/scala/src/Main.scala
        // with build.sbt at <tmp>/dyad-ws-<pid>/scala/.
        let base = std::env::temp_dir().join(format!("dyad-ws-{}", std::process::id()));
        let scala_root = base.join("scala");
        let src_dir = scala_root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(scala_root.join("build.sbt"), "").unwrap();
        let nested = src_dir.join("Main.scala");
        std::fs::write(&nested, "").unwrap();

        let root = workspace_root_for(&nested, Language::Scala);
        // canonicalize both sides so symlinked tmpdirs (macOS /tmp ->
        // /private/tmp) don't trip the comparison.
        assert_eq!(
            root.canonicalize().unwrap(),
            scala_root.canonicalize().unwrap()
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
