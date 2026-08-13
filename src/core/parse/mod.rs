//! Stage 1 — parse: for every parseable file, run the real oxc parser and
//! record whether it succeeded. This stage answers "does this file parse,
//! and if not, how" — nothing more. It deliberately does NOT extract
//! symbols; that's stage 2 (`core::semantic`), which re-parses
//! independently (see project notes on the current re-parse-per-stage
//! tradeoff — this file doesn't try to change that on its own).
//!
//! Two distinct kinds of "didn't parse cleanly" are tracked separately,
//! since they mean different things:
//! - `ParserReturn::panicked` (oxc's own field) — oxc's *recoverable* error
//!   mode: it hit unrecoverable syntax and stopped early, but did so
//!   cleanly, no Rust `panic!` involved. Surfaced as `ParseOutcome.panicked`.
//! - An actual Rust panic inside the parser/allocator (a real bug, not
//!   normal bad input) — caught via `panicsafety::run_file_safely`
//!   (`catch_unwind`) and surfaced as `FileError::RustPanic`, which aborts
//!   the whole batch under `ErrorProfile::Dev` (fail-closed) via
//!   `apply_batch_policy`. Note `catch_unwind` cannot catch a genuine stack
//!   overflow (e.g. from pathologically deep AST nesting) — that aborts the
//!   process regardless; there is no way to make that recoverable in Rust.
//!
//! This closes a real gap that existed before: `FileDetails.has_parse_errors`
//! / `parser_panicked` in the proto schema were previously always hardcoded
//! `false` because nothing upstream ever recorded parse health.

pub mod source_type;

use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use serde::Serialize;
use source_type::resolve_source_type;

/// Outcome of parsing a single file. Deliberately does not retain the
/// `Program` AST: the AST borrows from the `Allocator` it was parsed with,
/// which is dropped at the end of `parse_file` — this struct is just the
/// small, owned summary that survives past that.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParseOutcome {
    pub path: String,
    /// oxc's own recoverable "stopped early" flag — see module doc comment.
    /// NOT the same as a Rust panic.
    pub panicked: bool,
    pub diagnostic_count: usize,
    pub is_typescript: bool,
    pub is_jsx: bool,
}

impl ParseOutcome {
    pub fn has_parse_errors(&self) -> bool {
        self.panicked || self.diagnostic_count > 0
    }
}

/// Parses a single file already present in the `Vfs`, panic-safe. Returns
/// `Err(FileError::InvalidUtf8)` if the path isn't in the `Vfs` or isn't
/// valid UTF-8 — both mean "there is no source text to parse", a different
/// situation from a file that exists and fails to parse cleanly (that's
/// `ParseOutcome::panicked`) or one that crashes the parser itself (that's
/// `FileError::RustPanic`, from the `catch_unwind` wrapper below).
pub fn parse_file(vfs: &Vfs, path: &str) -> Result<ParseOutcome, FileError> {
    let source_text = vfs.read_utf8(path).ok_or_else(|| FileError::InvalidUtf8 {
        path: path.to_string(),
    })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();

        Ok(ParseOutcome {
            path: path.to_string(),
            panicked: ret.panicked,
            diagnostic_count: ret.diagnostics.len(),
            is_typescript: source_type.is_typescript(),
            is_jsx: source_type.is_jsx(),
        })
    })
}

#[derive(Debug, Default, Serialize)]
pub struct ParseBatch {
    pub outcomes: Vec<ParseOutcome>,
    /// Per-file failures (missing/non-UTF-8, or a genuine Rust panic),
    /// tracked with their real `FileError` rather than flattened to a
    /// path string — so a caller can tell "this file doesn't exist as
    /// text" apart from "this file crashed the parser".
    pub failures: Vec<FileError>,
}

/// Parses every path yielded by `paths` (typically
/// `scan_result.parseable_files.iter().map(|f| f.path.as_str())`). A
/// per-file failure is recorded in `failures` and does NOT stop the batch —
/// whether that should abort the whole run is a policy decision made by the
/// caller via `panicsafety::apply_batch_policy` against each failure, not
/// something this function decides on its own.
pub fn parse_all<'a>(vfs: &Vfs, paths: impl Iterator<Item = &'a str>) -> ParseBatch {
    let mut batch = ParseBatch::default();

    for path in paths {
        match parse_file(vfs, path) {
            Ok(outcome) => batch.outcomes.push(outcome),
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_typescript_cleanly() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/add.ts",
            b"export function add(a: number, b: number) { return a + b; }".to_vec(),
        );

        let outcome = parse_file(&vfs, "src/add.ts").unwrap();
        assert!(!outcome.panicked);
        assert_eq!(outcome.diagnostic_count, 0);
        assert!(outcome.is_typescript);
        assert!(!outcome.has_parse_errors());
    }

    #[test]
    fn missing_file_yields_invalid_utf8_error_not_a_panic() {
        let vfs = Vfs::empty();
        let err = parse_file(&vfs, "src/does-not-exist.ts").unwrap_err();
        assert!(matches!(err, FileError::InvalidUtf8 { .. }));
    }

    #[test]
    fn unrecoverable_syntax_error_is_flagged_panicked_not_a_rust_panic() {
        let mut vfs = Vfs::empty();
        // Deliberately unrecoverable: an unterminated string literal.
        // This should surface as ParseOutcome::panicked (oxc's own
        // recoverable flag), NOT as Err(FileError::RustPanic).
        vfs.insert("src/broken.ts", b"const x = \"unterminated".to_vec());

        let outcome = parse_file(&vfs, "src/broken.ts").unwrap();
        assert!(outcome.has_parse_errors());
    }

    #[test]
    fn parse_all_tracks_failures_separately_from_outcomes() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/ok.ts", b"export const x = 1;".to_vec());

        let batch = parse_all(&vfs, vec!["src/ok.ts", "src/missing.ts"].into_iter());
        assert_eq!(batch.outcomes.len(), 1);
        assert_eq!(batch.failures.len(), 1);
        assert!(matches!(batch.failures[0], FileError::InvalidUtf8 { .. }));
    }
}
