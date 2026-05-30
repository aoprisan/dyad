//! Phase 1 (agent-native build-out) — `test.run` backing module.
//!
//! Mirrors `git.rs`: a thin shell-out plus a pure parser, so the agent's
//! edit→verify loop can close without a human in the seat. We run the
//! language's `test_command` with the workspace root as the working
//! directory (cargo has no `-C`) and scrape libtest's *stable human
//! summary* — `test result: ok. N passed; M failed; …` plus the
//! `---- <name> stdout ----` failure blocks — rather than the nightly
//! `--format=json`, which isn't available on stable.
//!
//! The parser is the unit-tested core; the live `cargo test` run is
//! exercised by `scripts/mcp-smoke.sh` and an `#[ignore]`-gated protocol
//! test (running `cargo test` from inside `cargo test` is the obvious
//! hazard). Captured output is capped so a noisy build can't balloon the
//! MCP response.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::language::TestRunnerKind;

/// Keep the raw output tail bounded — same spirit as `protocol.rs`'s
/// `TASKS_MAX_FILE_BYTES`. A failing build with thousands of lines of
/// compiler noise shouldn't blow up the JSON response; the agent gets
/// the structured counts/failures plus the most recent ~4KB verbatim.
const RAW_TAIL_MAX_BYTES: usize = 4096;

/// One failed test: its libtest path (`module::test_name`) and the
/// captured panic / assertion message, trimmed of surrounding blank
/// lines. The message can be empty when a test failed without printing
/// (e.g. a timeout) — we still record the name.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TestFailure {
    pub name: String,
    pub message: String,
}

/// Parsed outcome of a test run. Counts are summed across every test
/// binary in the run (a cargo workspace emits one `test result:` line
/// per binary). `exit_ok` is the process exit status — distinct from
/// `failed == 0`, since a compile error fails the run with no test
/// results at all.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TestResults {
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub failures: Vec<TestFailure>,
    pub exit_ok: bool,
    /// Last ~4KB of combined stdout+stderr, capped at a char boundary.
    pub raw_tail: String,
}

/// Run `cmd` (argv) in `workspace_root`, optionally appending a `target`
/// filter (cargo treats a bare positional as a test-name substring).
/// Returns `Err` only when the process can't be spawned at all; a test
/// *failure* — or even a compile error — is a successful run that
/// produced `exit_ok: false`.
pub fn run(
    workspace_root: &Path,
    cmd: &[&str],
    kind: TestRunnerKind,
    target: Option<&str>,
) -> Result<TestResults> {
    let (program, rest) = cmd
        .split_first()
        .context("empty test command (language descriptor bug)")?;
    let mut command = Command::new(program);
    command.current_dir(workspace_root);
    command.args(rest);
    // `--color never` (cargo's own flag) keeps ANSI escapes out of the
    // captured summary so the parser sees plain `test result:` lines.
    command.arg("--color").arg("never");
    // The `target` filter is client-supplied. A `--` separator stops
    // cargo from parsing it as one of *its* own flags (argv smuggling —
    // e.g. `--manifest-path=…`): everything after `--` is forwarded to
    // the libtest harness, where a bare positional is a test-name
    // substring filter. Reject a leading `-` too, as defence-in-depth
    // and a clearer error than libtest's.
    if let Some(t) = target {
        if t.starts_with('-') {
            anyhow::bail!("test target filter must not start with '-': {t:?}");
        }
        command.arg("--").arg(t);
    }

    let output = command
        .output()
        .with_context(|| format!("spawning `{}` (is it installed?)", program))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Summary + failure blocks land on stdout; compiler errors land on
    // stderr. Parse stdout for structure, but keep both in the tail so a
    // compile failure still gives the agent something to read.
    let mut results = parse(kind, &stdout);
    results.exit_ok = output.status.success();
    results.raw_tail = tail(&format!("{stdout}{stderr}"), RAW_TAIL_MAX_BYTES);
    Ok(results)
}

/// Parse libtest's human-readable output into counts + failures. Pure
/// and total — the unit-tested core. `exit_ok` / `raw_tail` are filled
/// in by `run`; here they default to `true` / empty.
pub fn parse(kind: TestRunnerKind, text: &str) -> TestResults {
    match kind {
        TestRunnerKind::CargoTest => parse_cargo_test(text),
    }
}

fn parse_cargo_test(text: &str) -> TestResults {
    let mut passed = 0;
    let mut failed = 0;
    let mut ignored = 0;
    let failures = parse_failure_blocks(text);

    for line in text.lines() {
        if let Some((p, f, i)) = parse_summary_line(line) {
            passed += p;
            failed += f;
            ignored += i;
        }
    }

    TestResults {
        passed,
        failed,
        ignored,
        failures,
        exit_ok: true,
        raw_tail: String::new(),
    }
}

/// Pull `(passed, failed, ignored)` out of a `test result:` line, e.g.
/// `test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0
/// filtered out; finished in 0.00s`. Returns `None` for any other line.
/// We read `<number> <label>` pairs from the `;`-separated segments,
/// which dodges the `ok.` / `FAILED.` prefix and the trailing
/// `finished in 0.00s` (whose `0.00` would break a naive `.`-split).
fn parse_summary_line(line: &str) -> Option<(usize, usize, usize)> {
    let idx = line.find("test result:")?;
    let rest = &line[idx + "test result:".len()..];
    let mut passed = 0;
    let mut failed = 0;
    let mut ignored = 0;
    for segment in rest.split(';') {
        let mut tokens = segment.split_whitespace();
        // Find the first numeric token; its successor is the label.
        while let Some(tok) = tokens.next() {
            if let Ok(n) = tok.parse::<usize>() {
                match tokens.next() {
                    Some("passed") => passed = n,
                    Some("failed") => failed = n,
                    Some("ignored") => ignored = n,
                    _ => {}
                }
                break;
            }
        }
    }
    Some((passed, failed, ignored))
}

/// Collect the `---- <name> stdout ----` detail blocks libtest prints
/// before the summary. Each block's body (panic location + assertion
/// message) runs until the next `---- ` marker, a `failures:` summary
/// line, or a `test result:` line. Blank lines around the body are
/// trimmed.
fn parse_failure_blocks(text: &str) -> Vec<TestFailure> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(name) = parse_failure_header(line) else {
            continue;
        };
        let mut body: Vec<&str> = Vec::new();
        while let Some(peek) = lines.peek() {
            let trimmed = peek.trim();
            if parse_failure_header(peek).is_some()
                || trimmed == "failures:"
                || trimmed.starts_with("test result:")
            {
                break;
            }
            body.push(lines.next().unwrap());
        }
        // Drop leading/trailing blank lines without disturbing interior
        // structure (the panic message can legitimately span blanks).
        while body.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
            body.remove(0);
        }
        while body.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            body.pop();
        }
        out.push(TestFailure {
            name,
            message: body.join("\n"),
        });
    }
    out
}

/// `---- some::test stdout ----` → `Some("some::test")`. Also accepts
/// the `stderr` variant and a bare `---- name ----`. Anything else is
/// `None`.
fn parse_failure_header(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix("---- ")?.strip_suffix(" ----")?;
    let name = inner
        .strip_suffix(" stdout")
        .or_else(|| inner.strip_suffix(" stderr"))
        .unwrap_or(inner);
    Some(name.to_string())
}

/// Keep the last `max_bytes` of `s`, snapped forward to a UTF-8 char
/// boundary so we never split a multi-byte sequence.
fn tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative two-binary libtest run: one suite all-green, one
    // with a failing test and its captured panic block.
    const SAMPLE: &str = "\
running 2 tests
test tests::adds ... ok
test tests::subtracts ... ok

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 1 test
test tests::divides ... FAILED

failures:

---- tests::divides stdout ----
thread 'tests::divides' panicked at src/math.rs:42:9:
assertion `left == right` failed
  left: 3
 right: 2

failures:
    tests::divides

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    #[test]
    fn sums_counts_across_binaries() {
        let r = parse(TestRunnerKind::CargoTest, SAMPLE);
        assert_eq!(r.passed, 2);
        assert_eq!(r.failed, 1);
        assert_eq!(r.ignored, 1);
    }

    #[test]
    fn captures_failure_name_and_message() {
        let r = parse(TestRunnerKind::CargoTest, SAMPLE);
        assert_eq!(r.failures.len(), 1);
        let f = &r.failures[0];
        assert_eq!(f.name, "tests::divides");
        assert!(
            f.message.starts_with("thread 'tests::divides' panicked"),
            "message was: {:?}",
            f.message
        );
        assert!(f.message.contains("left: 3"));
        // Trailing blank lines trimmed, no `failures:` summary leaked in.
        assert!(!f.message.contains("failures:"));
        assert!(!f.message.ends_with('\n'));
    }

    #[test]
    fn all_green_run_has_no_failures() {
        let text = "\
running 1 test
test tests::works ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let r = parse(TestRunnerKind::CargoTest, text);
        assert_eq!((r.passed, r.failed, r.ignored), (1, 0, 0));
        assert!(r.failures.is_empty());
    }

    #[test]
    fn non_test_output_parses_to_zeros() {
        // A compile error produces no `test result:` lines at all.
        let r = parse(TestRunnerKind::CargoTest, "error[E0425]: cannot find value `x`\n");
        assert_eq!((r.passed, r.failed, r.ignored), (0, 0, 0));
        assert!(r.failures.is_empty());
    }

    #[test]
    fn tail_snaps_to_char_boundary() {
        // 'é' is two bytes; cutting mid-char must not panic and must
        // yield valid UTF-8 no longer than the limit's worth of chars.
        let s = "é".repeat(10); // 20 bytes
        let t = tail(&s, 5);
        assert!(t.len() <= 5);
        assert!(t.chars().all(|c| c == 'é'));
    }

    #[test]
    fn tail_returns_whole_string_when_short() {
        assert_eq!(tail("short", 4096), "short");
    }

    #[test]
    fn run_rejects_a_filter_that_smells_like_a_flag() {
        // A leading `-` is bounced before the process is ever spawned,
        // so this stays hermetic — no `cargo test` recursion.
        let err = run(
            Path::new("."),
            &["cargo", "test"],
            TestRunnerKind::CargoTest,
            Some("--manifest-path=/etc/passwd"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("must not start with '-'"),
            "unexpected error: {err}",
        );
    }
}
