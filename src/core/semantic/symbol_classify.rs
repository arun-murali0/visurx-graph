//! Stage 2 symbol extraction. Architecture adopted from the proven v1
//! design (a pre-pass collecting every top-level exported name into a
//! `HashSet` BEFORE the main visitor runs, so exported-ness is a simple
//! by-name lookup during collection — no transient "am I currently inside
//! an export wrapper" flag needed, and no risk of it leaking across
//! sibling statements). Extended to cover kinds v1's own version didn't
//! yet handle: interface, type_alias, enum, variable — v1's
//! `symbol_classify.rs` only ever implemented `visit_function`/
//! `visit_class`, the same gap this closes here.
//!
//! ## Verified against the REAL pinned oxc version (`crates_v0.141.0`)
//! `ExportNamedDeclaration` in this version carries `declaration:
//! Option<Declaration>`, `specifiers`, AND `source` all on one struct —
//! covering `export function f(){}` (`declaration: Some`), `export {a,b};`
//! (`declaration: None`, `source: None`), and `export {a,b} from 'x';`
//! (`declaration: None`, `source: Some`) all at once. `declaration_name()`
//! below has TSInterfaceDeclaration/TSTypeAliasDeclaration/TSEnumDeclaration
//! arms added — v1's version only handled Function/Class/Variable, so
//! `export interface X {}`/`export type X = ...`/`export enum X {}` would
//! have been collected as symbols but never correctly marked `exported`.
//!
//! ## Deliberately shallow (unlike v1's actual behavior — see note below)
//! `visit_function`/`visit_class` do NOT call `walk_function`/`walk_class`
//! here — matching the documented design decision that a deeply-nested
//! inner function/class shouldn't get its own graph node. Worth flagging
//! plainly: v1's actual `symbol_classify.rs` DOES call
//! `walk_function`/`walk_class` unconditionally, which — read literally —
//! means a named function nested inside another function WOULD get
//! visited and recorded there, contradicting that same design principle
//! stated in the project notes. Treated here as an inconsistency in v1 to
//! not carry forward, not as a deliberate behavior to preserve — flag if
//! that read is wrong and the recursion was actually intentional.
//!
//! ## Real bug found (and fixed) via actual repo output, not review alone
//! `ArrowFunctionExpression` is a DISTINCT AST node from `Function` in oxc
//! — the "no recursion into function bodies" guard above only stops the
//! walker at `Function`/`Class`, so without also overriding
//! `visit_arrow_function_expression`, the DEFAULT (fully recursive) walker
//! sailed straight through every arrow-function callback. Since test
//! frameworks almost universally write `describe(...)`/`it(...)` callbacks
//! as arrow functions, this meant every `const`/`let` inside a test block
//! was picked up as a false top-level `variable` symbol — confirmed
//! directly: a real run against `nodejs-backend-architecture-typescript`
//! showed dozens of duplicate `response`/`request`/`blog` "top-level"
//! symbols, one per test case, all with no scope segment in their `id`.
//! `visit_arrow_function_expression` below is now a no-op (no `walk_*`
//! call), closing this the same way `visit_function` already does.

use super::scope_path::{build_structural_id, ScopePath};
use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPattern, Class, ClassElement, Declaration,
    ExportDefaultDeclarationKind, Expression, Function, MethodDefinitionKind, ObjectPropertyKind,
    Program, PropertyKey, Statement, TSEnumDeclaration, TSInterfaceDeclaration,
    TSTypeAliasDeclaration, VariableDeclaration,
};
use oxc_ast_visit::Visit;
use oxc_span::GetSpan;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Interface,
    TypeAlias,
    Enum,
    Variable,
}

impl SymbolKind {
    fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Enum => "enum",
            SymbolKind::Variable => "variable",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub id: String,
    pub file: String,
    pub name: String,
    pub kind: SymbolKind,
    pub span_start: u32,
    pub span_end: u32,
    pub exported: bool,
}

pub fn extract_symbols(program: &Program, file_path: &str) -> Vec<Symbol> {
    let mut collector = SymbolCollector {
        file_path: file_path.to_string(),
        scope_path: ScopePath::new(),
        exported_names: collect_top_level_exported_names(program),
        symbols: Vec::new(),
    };
    collector.visit_program(program);
    collector.symbols
}

/// Pre-pass: collects every name that's exported at module top level, by
/// ANY of the real patterns — direct (`export function/class/interface/
/// type_alias/enum/variable`), default (`export default function/class`,
/// `export default someExistingBinding`, `export default { a, b, c }`
/// object-literal re-export — confirmed to be the dominant style in a real
/// tested repo per project notes), or named (`export { a, b };`, in either
/// order relative to the declaration, since this is a name-set lookup, not
/// an in-order flag). Explicitly excludes `export {a,b} from './other'` —
/// re-exporting FROM another module doesn't reference a symbol declared in
/// THIS file.
fn collect_top_level_exported_names(program: &Program) -> HashSet<String> {
    let mut names = HashSet::new();

    for stmt in &program.body {
        match stmt {
            Statement::ExportNamedDeclaration(export) => {
                if let Some(decl) = &export.declaration {
                    if let Some(name) = declaration_name(decl) {
                        names.insert(name);
                    }
                } else if export.source.is_none() {
                    // `export { a, b };` — same-file re-export only
                    // (source: None). `export { a, b } from 'x'` (source:
                    // Some) is deliberately NOT collected here.
                    for specifier in &export.specifiers {
                        names.insert(specifier.local.name().to_string());
                    }
                }
            }
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    if let Some(id) = &f.id {
                        names.insert(id.name.to_string());
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                    if let Some(id) = &c.id {
                        names.insert(id.name.to_string());
                    }
                }
                ExportDefaultDeclarationKind::ObjectExpression(obj) => {
                    for prop in &obj.properties {
                        if let ObjectPropertyKind::ObjectProperty(p) = prop {
                            if let Expression::Identifier(ident) = &p.value {
                                names.insert(ident.name.to_string());
                            } else if let PropertyKey::StaticIdentifier(key) = &p.key {
                                names.insert(key.name.to_string());
                            }
                        }
                    }
                }
                ExportDefaultDeclarationKind::Identifier(ident) => {
                    names.insert(ident.name.to_string());
                }
                _ => {} // export default <other expression> — still deferred
            },
            _ => {}
        }
    }

    names
}

fn declaration_name(decl: &Declaration) -> Option<String> {
    match decl {
        Declaration::FunctionDeclaration(f) => f.id.as_ref().map(|id| id.name.to_string()),
        Declaration::ClassDeclaration(c) => c.id.as_ref().map(|id| id.name.to_string()),
        Declaration::TSInterfaceDeclaration(i) => Some(i.id.name.to_string()),
        Declaration::TSTypeAliasDeclaration(t) => Some(t.id.name.to_string()),
        Declaration::TSEnumDeclaration(e) => Some(e.id.name.to_string()),
        Declaration::VariableDeclaration(v) => v.declarations.first().and_then(|d| match &d.id {
            BindingPattern::BindingIdentifier(id) => Some(id.name.to_string()),
            _ => None,
        }),
        _ => None,
    }
}

struct SymbolCollector {
    file_path: String,
    scope_path: ScopePath,
    exported_names: HashSet<String>,
    symbols: Vec<Symbol>,
}

impl SymbolCollector {
    fn push_symbol(&mut self, name: &str, kind: SymbolKind, span_start: u32, span_end: u32) {
        let id = build_structural_id(&self.file_path, &self.scope_path, name, kind.as_str());
        let exported = self.scope_path.is_top_level() && self.exported_names.contains(name);
        self.symbols.push(Symbol {
            id,
            file: self.file_path.clone(),
            name: name.to_string(),
            kind,
            span_start,
            span_end,
            exported,
        });
    }
}

impl<'a> Visit<'a> for SymbolCollector {
    fn visit_function(&mut self, func: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
        if let Some(id) = &func.id {
            let span = func.span();
            self.push_symbol(
                &id.name.to_string(),
                SymbolKind::Function,
                span.start,
                span.end,
            );
        }
        // Deliberately NOT calling walk::walk_function — no recursion into
        // function bodies. See module doc comment re: v1's divergence here.
    }

    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {
        // Deliberately a no-op, no walk::walk_arrow_function_expression
        // call — see module doc comment "Real bug found (and fixed)".
        // Arrow functions are anonymous by nature (any name comes from
        // whatever they're assigned to, handled separately by
        // visit_variable_declaration for `const f = () => {}` at true top
        // level) — there's nothing to record for the arrow function node
        // itself, and NOT recursing here is the actual point: without this
        // override, every describe()/it()-style callback body would be
        // walked in full, leaking its local variables as false top-level
        // symbols.
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        let class_name = class.id.as_ref().map(|id| id.name.to_string());
        let span = class.span();

        if let Some(name) = &class_name {
            self.push_symbol(name, SymbolKind::Class, span.start, span.end);
        }

        // Manual push/pop rather than ScopeGuard here: the guard would hold
        // a live `&mut self.scope_path` for this whole block, which
        // conflicts with `push_symbol` needing `&mut self` (the whole
        // struct) for each method below. `ScopeGuard` stays available in
        // `scope_path.rs` for a spot where nothing else needs `&mut self`
        // while it's held — this isn't that spot.
        if let Some(name) = &class_name {
            self.scope_path.push(name);
        }

        for element in &class.body.body {
            if let ClassElement::MethodDefinition(method) = element {
                let method_name = match &method.key {
                    PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
                    PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name)),
                    // Computed keys (`[Symbol.iterator]() {}`) skipped —
                    // no stable nameable identifier, not guessed at.
                    _ => None,
                };

                if let Some(name) = method_name {
                    let method_span = method.span();
                    let display_name = match method.kind {
                        MethodDefinitionKind::Constructor => format!("{name} (constructor)"),
                        MethodDefinitionKind::Get => format!("{name} (get)"),
                        MethodDefinitionKind::Set => format!("{name} (set)"),
                        MethodDefinitionKind::Method => name,
                    };
                    self.push_symbol(
                        &display_name,
                        SymbolKind::Method,
                        method_span.start,
                        method_span.end,
                    );
                }
            }
        }

        if class_name.is_some() {
            self.scope_path.pop();
        }
        // Deliberately NOT calling walk::walk_class — methods already
        // collected above without needing the default traversal, and this
        // avoids it recursing into method bodies via its own
        // visit_function dispatch.
    }

    fn visit_ts_interface_declaration(&mut self, it: &TSInterfaceDeclaration<'a>) {
        let span = it.span();
        self.push_symbol(
            &it.id.name.to_string(),
            SymbolKind::Interface,
            span.start,
            span.end,
        );
    }

    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        let span = it.span();
        self.push_symbol(
            &it.id.name.to_string(),
            SymbolKind::TypeAlias,
            span.start,
            span.end,
        );
    }

    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        let span = it.span();
        self.push_symbol(
            &it.id.name.to_string(),
            SymbolKind::Enum,
            span.start,
            span.end,
        );
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        // Only ever fires for top-level declarations, since nothing here
        // recurses into function/method bodies (see module doc comment).
        for declarator in &it.declarations {
            if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                let span = declarator.span();
                self.push_symbol(
                    &id.name.to_string(),
                    SymbolKind::Variable,
                    span.start,
                    span.end,
                );
            }
            // Destructuring patterns skipped — no single stable name.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(source: &str) -> Vec<Symbol> {
        let allocator = Allocator::default();
        let source_type = SourceType::default()
            .with_typescript(true)
            .with_module(true);
        let ret = Parser::new(&allocator, source, source_type).parse();
        extract_symbols(&ret.program, "test.ts")
    }

    #[test]
    fn extracts_bare_top_level_function() {
        let symbols = extract("function add(a, b) { return a + b; }");
        assert_eq!(symbols.len(), 1);
        assert!(!symbols[0].exported);
    }

    #[test]
    fn extracts_exported_interface_type_alias_enum_variable() {
        let source = r#"
            export interface Foo { x: number }
            export type Bar = string;
            export enum Color { Red, Green }
            export const CONFIG = 1;
        "#;
        let symbols = extract(source);
        assert_eq!(symbols.len(), 4);
        assert!(symbols.iter().all(|s| s.exported));
    }

    #[test]
    fn named_reexport_marks_symbol_exported_regardless_of_order() {
        let symbols = extract("function helper() {}\nexport { helper };");
        assert_eq!(symbols.len(), 1);
        assert!(symbols[0].exported);
    }

    #[test]
    fn reexport_from_other_module_does_not_falsely_mark_local_symbol() {
        // A same-named local declaration should NOT be marked exported just
        // because an unrelated `export { x } from './other'` exists.
        let symbols = extract("function helper() {}\nexport { helper } from './other';");
        assert_eq!(symbols.len(), 1);
        assert!(!symbols[0].exported);
    }

    #[test]
    fn export_default_object_literal_marks_referenced_names_exported() {
        let source = "function a() {}\nfunction b() {}\nexport default { a, b };";
        let symbols = extract(source);
        assert_eq!(symbols.len(), 2);
        assert!(symbols.iter().all(|s| s.exported));
    }

    #[test]
    fn class_methods_include_private_and_kind_suffix_get_set_constructor() {
        let source = r#"
            export class Counter {
                constructor(start) {}
                get value() { return 1; }
                set value(v) {}
                #privateHelper() {}
            }
        "#;
        let symbols = extract(source);
        let methods: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert_eq!(methods.len(), 4);
        assert!(methods.iter().any(|m| m.name.contains("constructor")));
        assert!(methods.iter().any(|m| m.name.contains("get")));
        assert!(methods.iter().any(|m| m.name.contains("set")));
        assert!(methods.iter().any(|m| m.name == "#privateHelper"));
        assert!(methods
            .iter()
            .all(|m| m.id.starts_with("test.ts::Counter::")));
    }

    #[test]
    fn computed_class_key_is_skipped_not_guessed() {
        let symbols = extract("class Foo { [Symbol.iterator]() {} }");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].kind, SymbolKind::Class);
    }

    #[test]
    fn does_not_descend_into_function_bodies() {
        let source = "function outer() { function inner() {} const x = 1; }";
        let symbols = extract(source);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "outer");
    }

    #[test]
    fn does_not_leak_variables_from_arrow_function_callbacks() {
        // Regression: the exact real-world pattern that exposed this bug —
        // a top-level `describe()`/`it()` call whose callback is an arrow
        // function (not a `Function`, a genuinely different AST node) used
        // to be fully walked by default, leaking `response`/`request` as
        // false top-level symbols. Confirmed against real repo output
        // before this fix; this test would have failed against the
        // previous version of this file.
        let source = r#"
            describe("suite", () => {
                it("does a thing", async () => {
                    const response = await doSomething();
                    const request = buildRequest();
                });
            });
        "#;
        let symbols = extract(source);
        assert_eq!(
            symbols.len(),
            0,
            "arrow-function callback bodies must not leak local variables as symbols"
        );
    }
}
