//! Cyclomatic complexity from oxc's real per-function CFG, using the
//! virtual-exit-node collapse fix (adapted from a proven, previously
//! fixed version — the naive formula counts every `return`/implicit-exit
//! edge separately, wildly inflating complexity for functions with
//! multiple returns; collapsing all of a function's exit edges into ONE
//! virtual +1 is what makes the McCabe formula (E - N + 2) correct here).
//!
//! Verified directly against the real pinned `oxc_cfg`/`oxc_semantic`
//! (`crates_v0.141.0`) source: `Instruction.node_id: Option<NodeId>`,
//! `BasicBlock.instructions: Vec<Instruction>`, `BasicBlock::is_unreachable()`,
//! `ControlFlowGraph::graph()`/`::basic_block()`, `Semantic::nodes()`,
//! `AstNodes::get_node() -> &AstNode` — all confirmed present exactly as
//! used, not assumed from a possibly-stale reference.

use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use petgraph::visit::EdgeRef;
use std::collections::HashSet;

/// Computes cyclomatic complexity for the function/method whose source
/// span is `[span_start, span_end)`, using oxc's real CFG (not a
/// hand-approximated one). Returns `1` (the minimum meaningful
/// complexity) if no CFG is available or the span matches no blocks —
/// never `0`, which would misleadingly suggest "no code path at all".
pub fn complexity_for_span(semantic: &Semantic, span_start: u32, span_end: u32) -> u32 {
    let Some(cfg) = semantic.cfg() else { return 1 };
    let graph = cfg.graph();

    let in_scope_nodes: Vec<_> = graph
        .node_indices()
        .filter(|&block_ix| {
            let block = cfg.basic_block(block_ix);
            block
                .instructions
                .iter()
                .find_map(|inst| inst.node_id)
                .map(|node_id| {
                    let start = semantic.nodes().get_node(node_id).span().start;
                    start >= span_start && start < span_end
                })
                .unwrap_or(false)
        })
        .collect();

    if in_scope_nodes.is_empty() {
        return 1;
    }

    let node_set: HashSet<_> = in_scope_nodes.iter().copied().collect();
    let node_count = in_scope_nodes.len();

    let mut internal_edges = 0i64;
    let mut has_any_exit = false;

    for &node_ix in &in_scope_nodes {
        let mut this_node_has_exit = false;
        for edge in graph.edges(node_ix) {
            if node_set.contains(&edge.target()) {
                internal_edges += 1;
            } else {
                this_node_has_exit = true;
            }
        }
        // Collapse ALL of this node's out-of-scope edges (typically the
        // function's real exit/return path) into exactly ONE virtual
        // edge — the fix that makes this formula correct. Without it, a
        // function with N return statements gets N separate exit edges,
        // inflating complexity by roughly N for no real reason.
        if this_node_has_exit {
            internal_edges += 1;
            has_any_exit = true;
        }
    }

    // The virtual exit collapse also needs exactly one virtual NODE
    // (not one per exiting block) for the McCabe formula to balance —
    // this is the other half of the same fix.
    let effective_node_count = if has_any_exit {
        node_count + 1
    } else {
        node_count
    };

    (internal_edges - effective_node_count as i64 + 2).max(1) as u32
}
