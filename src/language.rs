//! Language registry — the single source of truth for per-language data
//! threaded through the syntax, LSP, and protocol layers.
//!
//! Adding a third language is two steps: extend the enum, fill out the
//! descriptor methods. The match arms are exhaustive so the compiler
//! flags every site that needs a new branch.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Scala,
}

impl Language {
    /// Pick a language from a file path's extension. Returns `None` for
    /// paths we don't recognize — the caller falls back to plain text.
    pub fn for_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            // Metals treats .scala source, .sc worksheets, and .sbt build
            // files as first-class — all three route to the same server.
            "scala" | "sc" | "sbt" => Some(Self::Scala),
            _ => None,
        }
    }

    /// User-visible name used in status messages and error strings.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Scala => "scala",
        }
    }

    /// `languageId` sent in `textDocument/didOpen` (LSP spec §Text Document
    /// Items).
    pub fn lsp_language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Scala => "scala",
        }
    }

    /// Server binary spawned for this language.
    pub fn lsp_binary(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Scala => "metals",
        }
    }

    /// Hint shown when the LSP binary is missing from `PATH`.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Rust => "rustup component add rust-analyzer",
            Self::Scala => "coursier install metals",
        }
    }

    /// Files whose presence marks a workspace root for this language.
    /// `workspace_root_for` walks up the tree looking for any match.
    pub fn workspace_markers(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["Cargo.toml"],
            Self::Scala => &[
                "build.sbt",
                "build.sc",
                "build.mill",
                "project/build.properties",
            ],
        }
    }

    /// `true` when the server emits rust-analyzer's
    /// `experimental/serverStatus { quiescent }` extension. Gates the
    /// client capability we advertise during initialize.
    pub fn advertises_rust_analyzer_server_status(self) -> bool {
        matches!(self, Self::Rust)
    }

    /// `true` when the server emits indexing-status notifications we
    /// know how to interpret (rust-analyzer's `experimental/serverStatus`
    /// or Metals' `metals/status`). Drives `LspClient::is_indexing`.
    pub fn tracks_indexing_status(self) -> bool {
        matches!(self, Self::Rust | Self::Scala)
    }

    /// `true` when the `type_from_source_line` hover fallback (parses
    /// `: Type` out of the source line) is useful. Rust-only for now —
    /// Scala's hover format is different and the heuristic doesn't fit.
    pub fn supports_type_from_source_line(self) -> bool {
        matches!(self, Self::Rust)
    }

    /// How long to wait for the server's `initialize` response. Metals
    /// frequently takes >30s on first import; rust-analyzer is faster.
    pub fn initialize_timeout(self) -> Duration {
        match self {
            Self::Rust => Duration::from_secs(30),
            Self::Scala => Duration::from_secs(60),
        }
    }

    /// `initializationOptions` payload merged into the initialize params.
    /// Returns `None` to omit the field entirely.
    pub fn initialization_options(self) -> Option<Value> {
        match self {
            Self::Rust => None,
            Self::Scala => Some(json!({
                "isHttpEnabled": false,
                "compilerOptions": { "snippetAutoIndent": false },
                // Opt into `metals/status` notifications so we can
                // surface indexing/compiling state in the LSP badge.
                "statusBarProvider": "on",
            })),
        }
    }
}
