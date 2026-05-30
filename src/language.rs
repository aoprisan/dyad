//! Language registry — the single source of truth for per-language data
//! threaded through the syntax, LSP, and protocol layers.
//!
//! Adding a new language is two steps: extend the enum, fill out the
//! descriptor methods. The match arms are exhaustive so the compiler
//! flags every site that needs a new branch.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Scala,
    Elm,
}

/// Selects the output parser `test_runner` uses for a language's test
/// command. Each variant names a stable textual format we know how to
/// scrape — `CargoTest` is libtest's human summary (`test result: ok.
/// N passed; …` plus the `failures:` blocks). New runners (e.g. an sbt
/// or elm-test parser) get their own variant alongside `test_command`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestRunnerKind {
    CargoTest,
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
            "elm" => Some(Self::Elm),
            _ => None,
        }
    }

    /// User-visible name used in status messages and error strings.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Scala => "scala",
            Self::Elm => "elm",
        }
    }

    /// `languageId` sent in `textDocument/didOpen` (LSP spec §Text Document
    /// Items).
    pub fn lsp_language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Scala => "scala",
            Self::Elm => "elm",
        }
    }

    /// Server binary spawned for this language.
    pub fn lsp_binary(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Scala => "metals",
            Self::Elm => "elm-language-server",
        }
    }

    /// Hint shown when the LSP binary is missing from `PATH`.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Rust => "rustup component add rust-analyzer",
            Self::Scala => "coursier install metals",
            Self::Elm => "npm install -g @elm-tooling/elm-language-server",
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
            Self::Elm => &["elm.json"],
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
            Self::Elm => Duration::from_secs(30),
        }
    }

    /// Argv for running this language's test suite, run with the
    /// workspace root as the working directory (cargo has no `-C`, so
    /// `test_runner` sets `current_dir`). Returns `None` for languages
    /// without a wired-up runner yet — `test.run` bails cleanly rather
    /// than guessing. A `target` filter, when supplied, is appended by
    /// the caller.
    pub fn test_command(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Rust => Some(&["cargo", "test"]),
            // No agreed-on machine-parseable runner wired up yet; bail
            // cleanly rather than shell out to the wrong tool.
            Self::Scala | Self::Elm => None,
        }
    }

    /// Tree-sitter query whose `@import` capture selects this language's
    /// import declarations, for the LSP-free `scope.imports`. The node
    /// kinds are grammar-specific: Rust `use_declaration`, Scala
    /// `import_declaration`, Elm `import_clause`. Returns `None` for
    /// languages with no grammar wired up.
    pub fn import_query(self) -> Option<&'static str> {
        match self {
            Self::Rust => Some("(use_declaration) @import"),
            Self::Scala => Some("(import_declaration) @import"),
            Self::Elm => Some("(import_clause) @import"),
        }
    }

    /// Tree-sitter query whose `@fn` capture selects function-like
    /// definitions, used by `context.pack` to find the enclosing
    /// function (the pack anchor). Rust-only in this iteration; other
    /// languages return `None` and `context.pack` falls back to an
    /// import-only pack. Node kinds are grammar-specific.
    pub fn function_query(self) -> Option<&'static str> {
        match self {
            Self::Rust => Some("(function_item) @fn"),
            Self::Scala | Self::Elm => None,
        }
    }

    /// Tree-sitter query whose `@item` capture selects the file's
    /// top-level items, used by `context.pack` to gather sibling
    /// signatures. Rust-only for now (the `source_file` root node is
    /// grammar-specific); other languages return `None`.
    pub fn module_items_query(self) -> Option<&'static str> {
        match self {
            Self::Rust => Some("(source_file (_) @item)"),
            Self::Scala | Self::Elm => None,
        }
    }

    /// Which output parser `test_runner` should apply to this language's
    /// `test_command` output. `None` exactly when `test_command` is
    /// `None`.
    pub fn test_runner_kind(self) -> Option<TestRunnerKind> {
        match self {
            Self::Rust => Some(TestRunnerKind::CargoTest),
            Self::Scala | Self::Elm => None,
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
            Self::Elm => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn for_path_recognizes_rust_extension() {
        assert_eq!(Language::for_path(&PathBuf::from("foo.rs")), Some(Language::Rust));
    }

    #[test]
    fn for_path_routes_all_scala_filetypes_to_scala() {
        for ext in ["scala", "sc", "sbt"] {
            let p = PathBuf::from(format!("foo.{ext}"));
            assert_eq!(Language::for_path(&p), Some(Language::Scala), "{ext}");
        }
    }

    #[test]
    fn for_path_recognizes_elm_extension() {
        assert_eq!(
            Language::for_path(&PathBuf::from("Main.elm")),
            Some(Language::Elm)
        );
    }

    #[test]
    fn for_path_returns_none_for_unknown_extensions() {
        assert!(Language::for_path(&PathBuf::from("README.md")).is_none());
        assert!(Language::for_path(&PathBuf::from("noext")).is_none());
    }

    #[test]
    fn descriptor_strings_are_stable_and_non_empty() {
        for lang in [Language::Rust, Language::Scala, Language::Elm] {
            assert!(!lang.display_name().is_empty());
            assert!(!lang.lsp_binary().is_empty());
            assert!(!lang.lsp_language_id().is_empty());
            assert!(!lang.install_hint().is_empty());
            assert!(!lang.workspace_markers().is_empty());
            assert!(lang.initialize_timeout().as_secs() > 0);
        }
    }

    #[test]
    fn workspace_markers_match_per_language() {
        assert_eq!(Language::Rust.workspace_markers(), &["Cargo.toml"]);
        let scala_markers = Language::Scala.workspace_markers();
        assert!(scala_markers.contains(&"build.sbt"));
        assert_eq!(Language::Elm.workspace_markers(), &["elm.json"]);
    }

    #[test]
    fn capability_flags_match_language() {
        // Rust advertises rust-analyzer status; Scala and Elm do not.
        assert!(Language::Rust.advertises_rust_analyzer_server_status());
        assert!(!Language::Scala.advertises_rust_analyzer_server_status());
        assert!(!Language::Elm.advertises_rust_analyzer_server_status());

        // Rust and Scala emit indexing-status notifications we understand;
        // elm-language-server doesn't, so we don't gate the badge on it.
        assert!(Language::Rust.tracks_indexing_status());
        assert!(Language::Scala.tracks_indexing_status());
        assert!(!Language::Elm.tracks_indexing_status());

        // Only Rust has the type-from-source-line fallback.
        assert!(Language::Rust.supports_type_from_source_line());
        assert!(!Language::Scala.supports_type_from_source_line());
        assert!(!Language::Elm.supports_type_from_source_line());
    }

    #[test]
    fn test_command_wired_for_rust_only() {
        assert_eq!(Language::Rust.test_command(), Some(&["cargo", "test"][..]));
        assert!(Language::Scala.test_command().is_none());
        assert!(Language::Elm.test_command().is_none());

        // The parser selector tracks command availability exactly.
        assert_eq!(
            Language::Rust.test_runner_kind(),
            Some(TestRunnerKind::CargoTest)
        );
        for lang in [Language::Rust, Language::Scala, Language::Elm] {
            assert_eq!(
                lang.test_command().is_some(),
                lang.test_runner_kind().is_some(),
                "{lang:?}: command and parser availability must agree"
            );
        }
    }

    #[test]
    fn initialization_options_present_only_for_scala() {
        assert!(Language::Rust.initialization_options().is_none());
        let opts = Language::Scala.initialization_options().unwrap();
        assert_eq!(opts["isHttpEnabled"], false);
        assert_eq!(opts["statusBarProvider"], "on");
        assert!(Language::Elm.initialization_options().is_none());
    }
}
