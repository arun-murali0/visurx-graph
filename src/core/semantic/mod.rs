//! Stage 2 — semantic: extract symbols (function/class/method/interface/
//! type_alias/enum/variable) from every parseable file. Re-parses each
//! file independently from stage 1 (see project notes on the current
//! re-parse-per-stage tradeoff) — this stage's job is purely "what symbols
//! does this file declare", nothing about imports/calls/complexity yet.
//!
//! `cfg_available` (adopted from v1's `stage2_semantic`, `cfg_built` field):
//! a cheap early signal that oxc's CFG feature will actually build for this
//! file, checked now rather than waiting until stage 4 needs it for real.
//! Verified directly against the real pinned oxc_semantic API
//! (`SemanticBuilder::new_compiler().with_cfg(true).build(program)`,
//! `SemanticBuilderReturn.semantic`, `Semantic::cfg() -> Option<&ControlFlowGraph>`)
//! rather than assumed from v1's usage.

pub mod scope_path;
pub mod symbol_classify;

use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use serde::Serialize;
use symbol_classify::Symbol;

use crate::core::parse::source_type::resolve_source_type;

#[derive(Debug, Clone, Serialize)]
pub struct FileSemanticResult {
    pub symbols: Vec<Symbol>,
    pub cfg_available: bool,
}

/// Extracts every top-level (and one-level-nested, for class methods)
/// symbol from a single file, panic-safe, plus the `cfg_available` smoke
/// test. Returns `Err(FileError::InvalidUtf8)` for a missing/non-UTF-8
/// path — same convention as stage 1's `parse_file`.
///
/// Note: a file with unrecoverable syntax errors (oxc's own `panicked` flag,
/// see `core::parse`) is not specially rejected here — whatever oxc managed
/// to parse before giving up still gets walked, so a partially-valid file
/// yields whatever symbols appear before the error point, not zero. If the
/// caller wants to skip semantic analysis entirely for files stage 1 already
/// flagged as broken, that's a decision made by whoever calls this, using
/// stage 1's `ParseOutcome.panicked` — not enforced here.
pub fn extract_symbols(vfs: &Vfs, path: &str) -> Result<FileSemanticResult, FileError> {
    let source_text = vfs
        .read_utf8(path)
        .ok_or_else(|| FileError::InvalidUtf8 { path: path.to_string() })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();

        let symbols = symbol_classify::extract_symbols(&ret.program, path);

        let semantic_ret = SemanticBuilder::new_compiler().with_cfg(true).build(&ret.program);
        let cfg_available = semantic_ret.semantic.cfg().is_some();

        Ok(FileSemanticResult { symbols, cfg_available })
    })
}

#[derive(Debug, Default, Serialize)]
pub struct SemanticBatch {
    pub symbols: Vec<Symbol>,
    /// Paths where `cfg_available` came back `false` — tracked explicitly
    /// (rather than just a count) so it's possible to go look at exactly
    /// which files would need attention once stage 4 (CFG) is built.
    pub cfg_unavailable: Vec<String>,
    /// Per-file failures, same convention as `core::parse::ParseBatch`.
    pub failures: Vec<FileError>,
}

/// Extracts symbols (+ `cfg_available`) for every path yielded by `paths`
/// (typically `scan_result.parseable_files.iter().map(|f| f.path.as_str())`).
pub fn extract_all<'a>(vfs: &Vfs, paths: impl Iterator<Item = &'a str>) -> SemanticBatch {
    let mut batch = SemanticBatch::default();

    for path in paths {
        match extract_symbols(vfs, path) {
            Ok(result) => {
                batch.symbols.extend(result.symbols);
                if !result.cfg_available {
                    batch.cfg_unavailable.push(path.to_string());
                }
            }
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_symbols_from_a_real_file_in_the_vfs() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/add.ts",
            b"export function add(a: number, b: number) { return a + b; }".to_vec(),
        );

        let result = extract_symbols(&vfs, "src/add.ts").unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "add");
        assert!(result.symbols[0].exported);
    }

    #[test]
    fn cfg_is_available_for_a_normal_valid_file() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/add.ts", b"export function add(a, b) { return a + b; }".to_vec());

        let result = extract_symbols(&vfs, "src/add.ts").unwrap();
        assert!(result.cfg_available);
    }

    #[test]
    fn extract_all_tracks_failures_separately() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/ok.ts", b"export const x = 1;".to_vec());

        let batch = extract_all(&vfs, vec!["src/ok.ts", "src/missing.ts"].into_iter());
        assert_eq!(batch.symbols.len(), 1);
        assert_eq!(batch.failures.len(), 1);
    }
}
