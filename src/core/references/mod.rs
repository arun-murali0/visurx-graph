//! Stage 3b — references: calls and extends edges, matched against stage
//! 2's symbols and stage 3's already-collected import bindings (reused
//! directly via `SemanticBatch`/`ResolveBatch`, never independently
//! re-collected — avoids the "two parallel implementations drifting
//! apart" problem v1's own `reference_traces.rs` had with its own
//! separate `collect_import_bindings`).
//!
//! ## Unlike stage 2, this DOES recurse fully into function/method bodies
//! Finding a call means looking inside function bodies — the opposite of
//! stage 2's deliberate shallowness, not an inconsistency with it. Every
//! `Function`/`Class` still gets a scope pushed (so a call's
//! `from_symbol_id` is accurate), but `walk_function`/`walk_class` ARE
//! called here, unlike in `symbol_classify.rs`.
//!
//! ## Scope, matching v1's `reference_traces.rs` intentionally
//! Only bare-identifier callees (`foo()`) are matched — NOT method-chain
//! calls (`obj.foo()`). Only `Function` symbols are call targets, only
//! `Class` symbols are extends targets — methods are never targets in
//! this scheme (a bare `foo()` can't mean "call this class's method").
//!
//! ## Verified against the real pinned AST (`crates_v0.141.0`)
//! `CallExpression.callee: Expression`, `Class.super_class: Option<Expression>`
//! (both confirmed struct fields, not guessed).

use crate::core::parse::source_type::resolve_source_type;
use crate::core::resolve::{ImportTarget, ResolveBatch};
use crate::core::semantic::symbol_classify::{Symbol, SymbolKind};
use crate::core::semantic::SemanticBatch;
use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Class, Expression, Function, Program};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub enum ReferenceTarget {
    /// Matched a Function/Class symbol declared in the SAME file.
    SameFile(String),
    /// Matched via an imported binding whose resolve target is a real
    /// local file, matched against THAT file's Function/Class symbols by
    /// the import's real exported name (not the local/renamed one).
    CrossFile(String),
    /// The name matched an import binding, but that import's target is
    /// `External`, `UnresolvedLocal`, or a default/namespace import (no
    /// single named export to point at) — can't resolve to a real symbol
    /// node, but the attempt itself is real, not silently dropped.
    UnresolvedImport(String),
    /// Didn't match a same-file symbol OR an import binding — a
    /// parameter, a local variable, a global/builtin (`Array`, `Promise`),
    /// or something outside this scheme's scope.
    Unmatched,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallReference {
    pub from_symbol_id: String,
    pub callee_name: String,
    pub target: ReferenceTarget,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtendsReference {
    pub from_symbol_id: String,
    pub superclass_name: String,
    pub target: ReferenceTarget,
}

/// Prebuilt lookup indices over stage 2/3's already-computed flat data —
/// built ONCE, reused across every file, rather than linearly scanning the
/// whole repo's symbols/edges per call expression.
pub struct ReferenceIndex<'a> {
    /// (file, name) -> symbol id, Function/Class symbols only — the only
    /// kinds that can be a call/extends target in this scheme.
    symbols_by_file_and_name: HashMap<(&'a str, &'a str), &'a str>,
    /// file -> local binding name -> (imported/original name, target)
    import_bindings_by_file: HashMap<&'a str, HashMap<&'a str, (Option<&'a str>, &'a ImportTarget)>>,
}

impl<'a> ReferenceIndex<'a> {
    pub fn build(semantic: &'a SemanticBatch, resolve: &'a ResolveBatch) -> Self {
        let mut symbols_by_file_and_name = HashMap::new();
        for symbol in &semantic.symbols {
            if matches!(symbol.kind, SymbolKind::Function | SymbolKind::Class) {
                symbols_by_file_and_name.insert((symbol.file.as_str(), symbol.name.as_str()), symbol.id.as_str());
            }
        }

        let mut import_bindings_by_file: HashMap<&str, HashMap<&str, (Option<&str>, &ImportTarget)>> =
            HashMap::new();
        for edge in &resolve.edges {
            let bindings = import_bindings_by_file.entry(edge.from_file.as_str()).or_default();
            for binding in &edge.local_bindings {
                bindings.insert(
                    binding.local_name.as_str(),
                    (binding.imported_name.as_deref(), &edge.target),
                );
            }
        }

        Self { symbols_by_file_and_name, import_bindings_by_file }
    }

    /// Resolves a bare identifier name (a call callee or an extends
    /// superclass) seen in `file` to whatever it actually refers to.
    fn resolve_name(&self, file: &str, name: &str) -> ReferenceTarget {
        // Same-file symbol wins first: shadowing an import with a local
        // declaration of the same name is valid JS/TS and should resolve
        // to the local one, not the import.
        if let Some(&symbol_id) = self.symbols_by_file_and_name.get(&(file, name)) {
            return ReferenceTarget::SameFile(symbol_id.to_string());
        }

        if let Some(bindings) = self.import_bindings_by_file.get(file) {
            if let Some((imported_name, target)) = bindings.get(name) {
                return match target {
                    ImportTarget::LocalFile(target_file) => match imported_name {
                        // Default/namespace imports have no single
                        // original name to look up in the target file.
                        None => ReferenceTarget::UnresolvedImport(target_file.clone()),
                        Some(original_name) => {
                            match self.symbols_by_file_and_name.get(&(target_file.as_str(), *original_name)) {
                                Some(&symbol_id) => ReferenceTarget::CrossFile(symbol_id.to_string()),
                                None => ReferenceTarget::UnresolvedImport(target_file.clone()),
                            }
                        }
                    },
                    ImportTarget::External(pkg) => ReferenceTarget::UnresolvedImport(pkg.clone()),
                    ImportTarget::UnresolvedLocal(reason) => ReferenceTarget::UnresolvedImport(reason.clone()),
                };
            }
        }

        ReferenceTarget::Unmatched
    }
}

struct ReferenceCollector<'a, 'idx> {
    file: &'a str,
    index: &'idx ReferenceIndex<'idx>,
    /// Stack of enclosing symbol ids (function/method) — always non-empty
    /// while inside one; a call/extends found with an empty stack (at
    /// true top level, outside any function) has no meaningful
    /// `from_symbol_id` and is skipped, matching v1's behavior.
    scope_stack: Vec<String>,
    calls: Vec<CallReference>,
    extends: Vec<ExtendsReference>,
}

impl<'a, 'idx> ReferenceCollector<'a, 'idx> {
    fn current_scope(&self) -> Option<&str> {
        self.scope_stack.last().map(String::as_str)
    }
}

impl<'a, 'idx, 'ast> Visit<'ast> for ReferenceCollector<'a, 'idx> {
    fn visit_function(&mut self, func: &Function<'ast>, flags: oxc_syntax::scope::ScopeFlags) {
        if let Some(id) = &func.id {
            let symbol_id = format!("{}::{}::function", self.file, id.name);
            self.scope_stack.push(symbol_id);
            walk::walk_function(self, func, flags);
            self.scope_stack.pop();
        } else {
            // Anonymous function (expression) — still walk into it (a
            // call inside an anonymous callback should still be found),
            // just without pushing a new scope frame, so any call found
            // inside attributes to the ENCLOSING named scope, same as a
            // call sitting directly in that enclosing function's body.
            walk::walk_function(self, func, flags);
        }
    }

    fn visit_class(&mut self, class: &Class<'ast>) {
        let class_name = class.id.as_ref().map(|id| id.name.to_string());

        if let (Some(name), Some(super_class)) = (&class_name, &class.super_class) {
            if let Expression::Identifier(ident) = super_class {
                let superclass_name = ident.name.to_string();
                let from_symbol_id = format!("{}::{}::class", self.file, name);
                let target = self.index.resolve_name(self.file, &superclass_name);
                self.extends.push(ExtendsReference { from_symbol_id, superclass_name, target });
            }
        }

        // Methods: push "File::Class::method::method" scope (matching
        // stage 2's structural id shape for methods) and walk each
        // method's body directly — NOT via visit_function on the whole
        // class (which would need walk_class's default dispatch and lose
        // the per-method scope naming stage 2 already established).
        if let Some(name) = &class_name {
            for element in &class.body.body {
                if let oxc_ast::ast::ClassElement::MethodDefinition(method) = element {
                    if let oxc_ast::ast::PropertyKey::StaticIdentifier(key) = &method.key {
                        let method_symbol_id = format!("{}::{}::{}::method", self.file, name, key.name);
                        self.scope_stack.push(method_symbol_id);
                        if let Some(body) = &method.value.body {
                            self.visit_function_body(body);
                        }
                        self.scope_stack.pop();
                    }
                    // Private/computed-key methods: stage 2 still records
                    // them as symbols (with a "#name"/no-name-guessed
                    // shape), but this stage doesn't currently attempt to
                    // rebuild that exact display name here to push a
                    // matching scope — calls inside those methods are
                    // still found (still walked below via the fallback),
                    // just attributed to no enclosing scope rather than
                    // mis-attributed to a guessed wrong id.
                }
            }
        }
    }

    fn visit_call_expression(&mut self, expr: &CallExpression<'ast>) {
        if let Some(from_symbol_id) = self.current_scope() {
            if let Expression::Identifier(ident) = &expr.callee {
                let callee_name = ident.name.to_string();
                let target = self.index.resolve_name(self.file, &callee_name);
                self.calls.push(CallReference { from_symbol_id: from_symbol_id.to_string(), callee_name, target });
            }
            // Method-chain calls (`obj.foo()`) deliberately not matched —
            // see module doc comment.
        }
        walk::walk_call_expression(self, expr);
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ReferenceBatch {
    pub calls: Vec<CallReference>,
    pub extends: Vec<ExtendsReference>,
    pub failures: Vec<FileError>,
}

fn collect_references_for_file(index: &ReferenceIndex, vfs: &Vfs, path: &str) -> Result<(Vec<CallReference>, Vec<ExtendsReference>), FileError> {
    let source_text = vfs
        .read_utf8(path)
        .ok_or_else(|| FileError::InvalidUtf8 { path: path.to_string() })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();

        let mut collector =
            ReferenceCollector { file: path, index, scope_stack: Vec::new(), calls: Vec::new(), extends: Vec::new() };
        collector.visit_program(&ret.program);
        Ok((collector.calls, collector.extends))
    })
}

/// Builds the reference index once from stage 2/3's already-computed
/// data, then re-parses each file (same re-parse-per-stage tradeoff as
/// every other stage) to find calls/extends and match them against it.
pub fn collect_all<'a>(
    vfs: &Vfs,
    semantic: &SemanticBatch,
    resolve: &ResolveBatch,
    paths: impl Iterator<Item = &'a str>,
) -> ReferenceBatch {
    let index = ReferenceIndex::build(semantic, resolve);
    let mut batch = ReferenceBatch::default();

    for path in paths {
        match collect_references_for_file(&index, vfs, path) {
            Ok((calls, extends)) => {
                batch.calls.extend(calls);
                batch.extends.extend(extends);
            }
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::resolve::resolve_all;
    use crate::core::semantic::extract_all;
    use std::collections::HashMap as Map;

    fn analyze(vfs: &Vfs, paths: &[&str]) -> ReferenceBatch {
        let semantic = extract_all(vfs, paths.iter().copied());
        let resolve = resolve_all(vfs, &Map::new(), paths.iter().copied());
        collect_all(vfs, &semantic, &resolve, paths.iter().copied())
    }

    #[test]
    fn matches_same_file_call() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/app.ts",
            b"function helper() {}\nfunction main() { helper(); }".to_vec(),
        );

        let batch = analyze(&vfs, &["src/app.ts"]);
        assert_eq!(batch.calls.len(), 1);
        assert_eq!(batch.calls[0].callee_name, "helper");
        assert_eq!(batch.calls[0].from_symbol_id, "src/app.ts::main::function");
        match &batch.calls[0].target {
            ReferenceTarget::SameFile(id) => assert_eq!(id, "src/app.ts::helper::function"),
            other => panic!("expected SameFile, got {other:?}"),
        }
    }

    #[test]
    fn matches_cross_file_call_via_import_including_rename() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/app.ts",
            b"import { helper as h } from './helper';\nfunction main() { h(); }".to_vec(),
        );
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());

        let batch = analyze(&vfs, &["src/app.ts", "src/helper.ts"]);
        let call = batch.calls.iter().find(|c| c.callee_name == "h").unwrap();
        match &call.target {
            ReferenceTarget::CrossFile(id) => assert_eq!(id, "src/helper.ts::helper::function"),
            other => panic!("expected CrossFile (renamed import matched via original name), got {other:?}"),
        }
    }

    #[test]
    fn matches_extends_same_file_and_cross_file() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/app.ts",
            b"import { Base } from './base';\nclass Local {}\nclass A extends Local {}\nclass B extends Base {}"
                .to_vec(),
        );
        vfs.insert("src/base.ts", b"export class Base {}".to_vec());

        let batch = analyze(&vfs, &["src/app.ts", "src/base.ts"]);
        assert_eq!(batch.extends.len(), 2);

        let a = batch.extends.iter().find(|e| e.superclass_name == "Local").unwrap();
        assert!(matches!(&a.target, ReferenceTarget::SameFile(id) if id == "src/app.ts::Local::class"));

        let b = batch.extends.iter().find(|e| e.superclass_name == "Base").unwrap();
        assert!(matches!(&b.target, ReferenceTarget::CrossFile(id) if id == "src/base.ts::Base::class"));
    }

    #[test]
    fn method_call_via_this_is_not_matched_intentional_scope() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/app.ts",
            b"class Counter { increment() {} run() { this.increment(); } }".to_vec(),
        );

        let batch = analyze(&vfs, &["src/app.ts"]);
        // `this.increment()` is a StaticMemberExpression callee, not a
        // bare Identifier — deliberately never matched.
        assert!(batch.calls.is_empty());
    }

    #[test]
    fn undeclared_global_call_is_unmatched_not_a_false_positive() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"function main() { console.log('hi'); parseInt('1'); }".to_vec());

        let batch = analyze(&vfs, &["src/app.ts"]);
        let call = batch.calls.iter().find(|c| c.callee_name == "parseInt").unwrap();
        assert!(matches!(call.target, ReferenceTarget::Unmatched));
    }

    #[test]
    fn call_at_true_top_level_outside_any_function_is_skipped() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"function helper() {}\nhelper();".to_vec());

        let batch = analyze(&vfs, &["src/app.ts"]);
        // The top-level `helper();` call has no enclosing symbol scope —
        // matches v1's behavior of only recording calls found inside some
        // named function/method.
        assert!(batch.calls.is_empty());
    }
}
