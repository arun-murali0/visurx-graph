//! Stage 4 — CFG: cyclomatic complexity + the real per-function block
//! graph, for every Function/Method symbol stage 2 already found. Pure
//! annotation — no new nodes, no new files walked beyond what's needed to
//! build the CFG for files that already have symbols.
//!
//! Adapted directly from a proven, previously-fixed version of this exact
//! logic (the virtual-exit-node collapse in `complexity.rs` took multiple
//! attempts to get right there — see that file's doc comment) — re-wired
//! onto our `Symbol`/`SymbolKind` types and panic-safety pattern, with
//! every `oxc_cfg`/`oxc_semantic` API call re-verified against the real
//! pinned source rather than trusted from memory.

pub mod complexity;

use crate::core::parse::source_type::resolve_source_type;
use crate::core::semantic::symbol_classify::{Symbol, SymbolKind};
use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use complexity::complexity_for_span;
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::GetSpan;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CfgBlock {
    pub block_id: String,
    pub span_start: u32,
    pub span_end: u32,
    pub next: Vec<String>,
    pub is_unreachable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCfgReport {
    pub symbol_id: String,
    pub name: String,
    pub cyclomatic_complexity: u32,
    pub blocks: Vec<CfgBlock>,
}

fn build_cfg_blocks(
    semantic: &oxc_semantic::Semantic,
    span_start: u32,
    span_end: u32,
) -> Vec<CfgBlock> {
    let Some(cfg) = semantic.cfg() else {
        return Vec::new();
    };
    let graph = cfg.graph();
    let mut blocks = Vec::new();

    for block_ix in graph.node_indices() {
        let block = cfg.basic_block(block_ix);
        let Some(node_id) = block.instructions.iter().find_map(|inst| inst.node_id) else {
            continue;
        };

        let node_span = semantic.nodes().get_node(node_id).span();
        if node_span.start < span_start || node_span.start >= span_end {
            continue;
        }

        let next: Vec<String> = graph
            .neighbors(block_ix)
            .filter_map(|neighbor_ix| {
                let neighbor_node_id = cfg
                    .basic_block(neighbor_ix)
                    .instructions
                    .iter()
                    .find_map(|inst| inst.node_id)?;
                let neighbor_start = semantic.nodes().get_node(neighbor_node_id).span().start;
                (neighbor_start >= span_start && neighbor_start < span_end)
                    .then(|| format!("block_{}", neighbor_ix.index()))
            })
            .collect();

        blocks.push(CfgBlock {
            block_id: format!("block_{}", block_ix.index()),
            span_start: node_span.start,
            span_end: node_span.end,
            next,
            is_unreachable: block.is_unreachable(),
        });
    }

    blocks
}

/// Builds CFG reports for every Function/Method symbol in `symbols` that
/// belongs to this one file (already-parsed once here — same re-parse-
/// per-stage tradeoff as every other stage, since building a real CFG
/// needs its own `SemanticBuilder` pass, not something stage 1/2's parses
/// could hand off).
pub fn analyze_file_cfg(
    vfs: &Vfs,
    path: &str,
    symbols: &[Symbol],
) -> Result<Vec<FunctionCfgReport>, FileError> {
    let source_text = vfs.read_utf8(path).ok_or_else(|| FileError::InvalidUtf8 {
        path: path.to_string(),
    })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();
        // `.with_build_nodes(true)` is REQUIRED here, not optional — its
        // default is `false` (oxc's "compiler pipeline" default, which
        // doesn't need random node access). Without it, `semantic.nodes()`
        // is an EMPTY table, and every `get_node(node_id)` call below
        // panics on ANY index at all ("len is 0 but the index is N") —
        // confirmed via a real panic trace before this fix, not assumed.
        let semantic_ret = SemanticBuilder::new_compiler()
            .with_build_nodes(true)
            .with_cfg(true)
            .build(&ret.program);

        let reports = symbols
            .iter()
            .filter(|s| {
                s.file == path && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
            })
            .map(|symbol| {
                let cyclomatic_complexity =
                    complexity_for_span(&semantic_ret.semantic, symbol.span_start, symbol.span_end);
                let blocks =
                    build_cfg_blocks(&semantic_ret.semantic, symbol.span_start, symbol.span_end);
                FunctionCfgReport {
                    symbol_id: symbol.id.clone(),
                    name: symbol.name.clone(),
                    cyclomatic_complexity,
                    blocks,
                }
            })
            .collect();

        Ok(reports)
    })
}

#[derive(Debug, Default, Serialize)]
pub struct CfgBatch {
    pub reports: Vec<FunctionCfgReport>,
    pub failures: Vec<FileError>,
}

/// Runs `analyze_file_cfg` for every path yielded by `paths`, pulling each
/// file's own symbols out of the already-computed (stage 2) `symbols`
/// list — reused, not re-derived, same principle as stage 3b reusing
/// stage 2/3's data.
pub fn analyze_all<'a>(
    vfs: &Vfs,
    symbols: &[Symbol],
    paths: impl Iterator<Item = &'a str>,
) -> CfgBatch {
    let mut batch = CfgBatch::default();

    for path in paths {
        match analyze_file_cfg(vfs, path, symbols) {
            Ok(reports) => batch.reports.extend(reports),
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::semantic::extract_all;

    #[test]
    fn simple_function_with_no_branches_has_complexity_one() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/add.ts",
            b"export function add(a, b) { return a + b; }".to_vec(),
        );

        let semantic_batch = extract_all(&vfs, std::iter::once("src/add.ts"));
        let batch = analyze_all(&vfs, &semantic_batch.symbols, std::iter::once("src/add.ts"));

        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].cyclomatic_complexity, 1);
        assert!(!batch.reports[0].blocks.is_empty());
    }

    #[test]
    fn multiple_returns_do_not_inflate_complexity_past_real_branch_count() {
        // Two early returns (three total exits) but only ONE real
        // decision point (the `if`) — complexity should reflect that one
        // branch, not be inflated by having three separate return points.
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/check.ts",
            b"export function check(x: number) { if (x > 0) { return 1; } return 0; }".to_vec(),
        );

        let semantic_batch = extract_all(&vfs, std::iter::once("src/check.ts"));
        let batch = analyze_all(
            &vfs,
            &semantic_batch.symbols,
            std::iter::once("src/check.ts"),
        );

        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].cyclomatic_complexity, 2);
    }

    #[test]
    fn more_branches_yield_monotonically_higher_complexity() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/branchy.ts",
            b"export function branchy(x: number) { if (x > 0) { if (x > 10) { return 2; } return 1; } return 0; }"
                .to_vec(),
        );

        let semantic_batch = extract_all(&vfs, std::iter::once("src/branchy.ts"));
        let batch = analyze_all(
            &vfs,
            &semantic_batch.symbols,
            std::iter::once("src/branchy.ts"),
        );

        // Two real decision points -> complexity should exceed the
        // single-if case above (2), not just match it.
        assert!(batch.reports[0].cyclomatic_complexity > 2);
    }

    #[test]
    fn only_function_and_method_symbols_get_cfg_reports() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/mixed.ts",
            b"export interface Foo { x: number }\nexport function bar() {}".to_vec(),
        );

        let semantic_batch = extract_all(&vfs, std::iter::once("src/mixed.ts"));
        let batch = analyze_all(
            &vfs,
            &semantic_batch.symbols,
            std::iter::once("src/mixed.ts"),
        );

        // Only "bar" (function) gets a report, not the "Foo" interface.
        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].name, "bar");
    }
}
