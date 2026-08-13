//! Stage 3 — resolve: for every file, collect its import specifiers
//! (`import ... from`, `export {...} from`, `export * from`) and resolve
//! each one against the real repo structure via `oxc_resolver`. Produces a
//! flat `Vec<ImportEdge>` per the "each stage independently callable,
//! flat data, graph assembled once at the end" plan — this stage does NOT
//! touch `graph::CodeGraph` (doesn't exist yet).
//!
//! Verified directly against the real pinned `oxc_resolver` (`v11.23.0`)
//! and `oxc_ast` (`crates_v0.141.0`) source rather than assumed:
//! - `ExportNamedDeclaration.source: Option<StringLiteral>` — only
//!   `Some` for `export {a} from './x'` (a re-export), `None` for
//!   `export {a};` (same-file, not an import at all — correctly not
//!   collected here).
//! - `ExportAllDeclaration.source: StringLiteral` — always present,
//!   `export * from './x'` / `export * as ns from './x'`.
//! - `FileMetadata::new(is_file, is_dir, is_symlink)` — three bools, see
//!   `vfs_filesystem.rs`'s doc comment.
//!
//! ## What every import candidate resolves to — never a silent gap
//! - `ImportTarget::LocalFile(path)` — resolved to a real path AND that
//!   path is a file our `Vfs` actually has.
//! - `ImportTarget::External(package_name)` — a non-relative specifier
//!   (no `./`/`../` prefix) that didn't resolve to a real `Vfs` file: a
//!   genuine npm package, OR one that resolved into `node_modules` (hard-
//!   excluded — see `vfs.rs`). Both are indistinguishable from here and
//!   treated the same, matching the existing project convention.
//! - `ImportTarget::UnresolvedLocal(reason)` — a relative specifier (`./`/`../`)
//!   that didn't resolve to a real `Vfs` file. Kept strictly separate from
//!   `External`: a fallback that ran `reduce_to_package_name` on ANY
//!   unresolved specifier regardless of shape used to produce nonsense
//!   like `External("..")` for `../helpers/asyncHandler` — a genuine,
//!   confirmed bug (found via real repo output, fixed here) caused by
//!   `normalize_key` not collapsing `..`/`.` path components before
//!   comparing against `Vfs` keys.
//!
//! A DIFFERENT kind of drop — a `LocalFile` target whose file failed
//! stage 1 parsing and so will never become a graph node — can't be
//! detected here at all (this stage doesn't know what other files
//! succeeded/failed). That's necessarily a graph-construction-time
//! concern, not a resolve-stage concern, given stages are being kept flat
//! and independent until graph assembly.
//!
//! ## Known gaps, unchanged from before this stage was built
//! - No `require(...)` (CommonJS) or dynamic `import()` support — only
//!   static ESM forms are collected. Real, documented gap, not silent.
//! - No re-export-from-elsewhere transitive following (barrel files are
//!   collected as `is_re_export: true` edges here, but nothing yet resolves
//!   "the symbol behind this re-export" — that's references-stage work).

pub mod external_classify;
pub mod vfs_filesystem;

use crate::core::parse::source_type::resolve_source_type;
use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement};
use oxc_parser::Parser;
use oxc_resolver::{Alias, AliasValue, ResolveOptions, ResolverGeneric};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use vfs_filesystem::{normalize_key, VfsFileSystem};

#[derive(Debug, Clone, Serialize)]
pub enum ImportTarget {
    LocalFile(String),
    External(String),
    /// The specifier was clearly a relative import (`./` or `../` prefix)
    /// but didn't resolve to any real file in the `Vfs` — a genuine
    /// broken/dropped local import. Deliberately never conflated with
    /// `External`: a naive fallback that ran `reduce_to_package_name` on
    /// a relative specifier regardless of shape used to produce nonsense
    /// like `External("..")` for `../helpers/asyncHandler` — confirmed as
    /// a real bug via actual repo output before this fix. Carries the
    /// real failure reason (the resolver's own error text, or the
    /// resolved-but-not-in-Vfs path) rather than a bare unit variant —
    /// needed to diagnose real accuracy problems instead of guessing.
    UnresolvedLocal(String),
}

/// One binding introduced by an import statement — both names, not just
/// the local one. Needed for cross-file symbol matching: `import { foo as
/// bar }` binds locally as `bar`, but the TARGET file exports `foo` — a
/// lookup using only the local name would silently fail for every renamed
/// import.
#[derive(Debug, Clone, Serialize)]
pub struct LocalBinding {
    pub local_name: String,
    /// The original/exported name from the source module, when there is
    /// one to name: `None` for a default import (binds the module's
    /// default export, not a specific named export) or a namespace import
    /// (`* as ns`, represents the whole module, not one symbol) — both
    /// deliberately left unresolvable to a single target symbol here.
    pub imported_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportEdge {
    pub from_file: String,
    pub specifier: String,
    /// `export {a} from './x'` / `export * from './x'` — collected the
    /// same way as a plain import, tagged separately since it matters for
    /// barrel-file detection later (not yet acted on here).
    pub is_re_export: bool,
    /// `import type { Foo } from './foo'` (whole-declaration form) or
    /// `export type { Foo } from './foo'` — a type-only edge isn't a real
    /// runtime dependency. Deliberately whole-declaration-level only, not
    /// per-specifier (`import { type Bar, baz }` mixed imports aren't
    /// split further here) — the coarser signal is what matters for
    /// dead-code/confidence accuracy, per-specifier type tracking would be
    /// real but unnecessary precision for this pass.
    pub is_type_only: bool,
    /// Bindings this import statement introduces. Always empty for a
    /// re-export (`export {a} from`/`export * from` bind nothing locally
    /// in this file). Captured here specifically so stage 3b (references)
    /// can reuse this data directly — matching both the call-site name
    /// AND the target file's real export name — instead of independently
    /// re-parsing every file's imports a second time.
    pub local_bindings: Vec<LocalBinding>,
    pub target: ImportTarget,
}

struct ImportCandidate {
    specifier: String,
    is_re_export: bool,
    is_type_only: bool,
    local_bindings: Vec<LocalBinding>,
}

fn collect_import_specifiers(program: &Program) -> Vec<ImportCandidate> {
    use oxc_ast::ast::{ImportDeclarationSpecifier, ImportOrExportKind};

    let mut out = Vec::new();

    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                let local_bindings = decl
                    .specifiers
                    .as_ref()
                    .map(|specifiers| {
                        specifiers
                            .iter()
                            .map(|spec| match spec {
                                ImportDeclarationSpecifier::ImportSpecifier(s) => LocalBinding {
                                    local_name: s.local.name.to_string(),
                                    imported_name: Some(s.imported.name().to_string()),
                                },
                                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                    LocalBinding { local_name: s.local.name.to_string(), imported_name: None }
                                }
                                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                                    LocalBinding { local_name: s.local.name.to_string(), imported_name: None }
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                out.push(ImportCandidate {
                    specifier: decl.source.value.to_string(),
                    is_re_export: false,
                    is_type_only: decl.import_kind == ImportOrExportKind::Type,
                    local_bindings,
                });
            }
            Statement::ExportNamedDeclaration(decl) => {
                // Only `Some` for `export {a} from './x'` — `export {a};`
                // (source: None) is a same-file re-export, not an import,
                // and correctly not collected here (that's
                // `symbol_classify.rs`'s concern, already handled there).
                if let Some(source) = &decl.source {
                    out.push(ImportCandidate {
                        specifier: source.value.to_string(),
                        is_re_export: true,
                        is_type_only: decl.export_kind == ImportOrExportKind::Type,
                        local_bindings: Vec::new(),
                    });
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                out.push(ImportCandidate {
                    specifier: decl.source.value.to_string(),
                    is_re_export: true,
                    is_type_only: decl.export_kind == ImportOrExportKind::Type,
                    local_bindings: Vec::new(),
                });
            }
            _ => {}
        }
    }

    out
}

/// `"lodash/get"` -> `"lodash"`, `"@scope/pkg/sub"` -> `"@scope/pkg"`,
/// `"react"` -> `"react"` — the external-package identity a specifier
/// belongs to, independent of which specific submodule was imported.
pub fn reduce_to_package_name(specifier: &str) -> String {
    if let Some(stripped) = specifier.strip_prefix('@') {
        let mut parts = stripped.splitn(2, '/');
        match parts.next() {
            Some(scope) => match parts.next() {
                Some(name) => format!("@{scope}/{}", name.split('/').next().unwrap_or(name)),
                None => format!("@{scope}"),
            },
            None => specifier.to_string(),
        }
    } else {
        specifier.split('/').next().unwrap_or(specifier).to_string()
    }
}

/// Converts tsconfig `paths` (already extends-chain-resolved by
/// `scan::manifest::parse_tsconfig_resolved`) into `oxc_resolver`'s
/// webpack-style `Alias` — closing the gap flagged earlier in this
/// project: aliases were extracted but never fed to the resolver.
///
/// Handles the common `"@/*": ["src/*"]` glob-suffix convention by
/// stripping the trailing `/*` from both sides (tsconfig's convention;
/// `oxc_resolver`'s `alias` is plain prefix-replacement, no wildcard
/// needed since prefix matching is inherent to it).
///
/// A bare wildcard-only pattern like `"*": ["./src/*"]` (confirmed to
/// appear in real-world tsconfigs — seen in this project's own test repo)
/// is explicitly checked for and skipped — NOT via the `/*`-suffix strip
/// alone, which does nothing useful here (`"*"` doesn't end with `/*`, so
/// it falls through UNCHANGED, not to an empty string). Left unhandled,
/// this is a genuine, confirmed-in-production bug, not a cosmetic one: a
/// literal `"*"` alias matches every specifier, which sends
/// `oxc_resolver` into its own alias-substitution recursion guard
/// (`ResolveError::Recursion`) on every single import — 0/374 resolved in
/// a real run before this was caught and fixed.
pub fn tsconfig_aliases_to_resolver_alias(path_aliases: &HashMap<String, Vec<String>>) -> Alias {
    path_aliases
        .iter()
        .filter_map(|(pattern, targets)| {
            let alias_prefix = pattern.strip_suffix("/*").unwrap_or(pattern);
            // Two distinct ways a pattern can have no well-defined
            // prefix-alias meaning: strips down to genuinely empty
            // (`"/*"` alone), OR is a BARE `"*"` — no slash, so
            // `strip_suffix("/*")` doesn't match it at all and it falls
            // through UNCHANGED as `"*"`. That second case is the one
            // that actually broke this in real use: a literal `"*"` alias
            // matches every specifier, which sent `oxc_resolver` into its
            // own alias-substitution recursion guard
            // (`ResolveError::Recursion`, "alias paths reference each
            // other") on EVERY single import in a real repo — 0/374
            // resolved before this fix. My own unit test asserted this
            // was already handled; it wasn't, because it was never
            // actually run against a working toolchain until real output
            // exposed the gap between the assertion and the real logic.
            if alias_prefix.is_empty() || alias_prefix == "*" {
                return None;
            }
            let values: Vec<AliasValue> = targets
                .iter()
                .map(|target| {
                    let target_prefix = target.strip_suffix("/*").unwrap_or(target);
                    AliasValue::Path(target_prefix.to_string())
                })
                .collect();
            (!values.is_empty()).then_some((alias_prefix.to_string(), values))
        })
        .collect()
}

/// Includes `.d.ts`/`.d.mts`/`.d.cts` (TypeScript declaration files) as
/// distinct entries, NOT just implied by `.ts`/`.mts`/`.cts` — a bare
/// extension-appending resolver tries `"app-request" + ".ts"` ->
/// `"app-request.ts"`, which does NOT match a real file named
/// `"app-request.d.ts"` (a compound, double extension). Confirmed as a
/// real gap via actual repo output (`src/types/app-request.d.ts` failing
/// to resolve from a bare `import ... from '../types/app-request'`) —
/// and confirmed this isn't something v1 already solved either: BOTH its
/// standalone `resolve/mod.rs` and the actually-live inline resolver in
/// `pipeline_builder.rs` have the identical extensions list, missing
/// `.d.ts` in both places.
const RESOLVABLE_EXTENSIONS: [&str; 12] = [
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".d.mts", ".cts", ".d.cts", ".json",
];

/// Builds one resolver, meant to be reused across every file in the repo
/// (constructing `ResolverGeneric` per-file would be wasteful — matches
/// the efficiency choice already made in the wider project for this exact
/// reason).
pub fn build_resolver(vfs: &Vfs, path_aliases: &HashMap<String, Vec<String>>) -> ResolverGeneric<VfsFileSystem> {
    let options = ResolveOptions {
        extensions: RESOLVABLE_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
        condition_names: vec!["node".to_string(), "import".to_string(), "require".to_string()],
        alias: tsconfig_aliases_to_resolver_alias(path_aliases),
        ..ResolveOptions::default()
    };
    ResolverGeneric::new_with_file_system(VfsFileSystem::new(vfs.clone()), options)
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

fn resolve_imports_for_file(
    resolver: &ResolverGeneric<VfsFileSystem>,
    vfs: &Vfs,
    file_path: &str,
    program: &Program,
) -> Vec<ImportEdge> {
    let from_dir = Path::new(file_path).parent().unwrap_or_else(|| Path::new(""));

    collect_import_specifiers(program)
        .into_iter()
        .map(|candidate| {
            let target = match resolver.resolve(from_dir, &candidate.specifier) {
                Ok(resolution) => {
                    let resolved_key = normalize_key(resolution.path());
                    if vfs.exists(&resolved_key) {
                        ImportTarget::LocalFile(resolved_key)
                    } else if is_relative_specifier(&candidate.specifier) {
                        // Resolved to SOME path on the resolver's own
                        // terms, but that path isn't a file our Vfs has —
                        // and the specifier was unambiguously a local
                        // import (not a package name), so this is a
                        // genuine broken local reference, never External.
                        // Reason carries exactly what it resolved to, so
                        // this is diagnosable rather than a bare flag.
                        ImportTarget::UnresolvedLocal(format!(
                            "resolved to '{resolved_key}' but that path is not in the Vfs (from_dir='{}')",
                            from_dir.display()
                        ))
                    } else {
                        ImportTarget::External(reduce_to_package_name(&candidate.specifier))
                    }
                }
                Err(err) => {
                    if is_relative_specifier(&candidate.specifier) {
                        ImportTarget::UnresolvedLocal(format!(
                            "resolver error from_dir='{}': {err}",
                            from_dir.display()
                        ))
                    } else {
                        ImportTarget::External(reduce_to_package_name(&candidate.specifier))
                    }
                }
            };

            ImportEdge {
                from_file: file_path.to_string(),
                specifier: candidate.specifier,
                is_re_export: candidate.is_re_export,
                is_type_only: candidate.is_type_only,
                local_bindings: candidate.local_bindings,
                target,
            }
        })
        .collect()
}

fn resolve_file(
    vfs: &Vfs,
    resolver: &ResolverGeneric<VfsFileSystem>,
    path: &str,
) -> Result<Vec<ImportEdge>, FileError> {
    let source_text = vfs
        .read_utf8(path)
        .ok_or_else(|| FileError::InvalidUtf8 { path: path.to_string() })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();
        Ok(resolve_imports_for_file(resolver, vfs, path, &ret.program))
    })
}

#[derive(Debug, Default, Serialize)]
pub struct ResolveBatch {
    pub edges: Vec<ImportEdge>,
    pub failures: Vec<FileError>,
}

/// Resolves imports for every path yielded by `paths` (typically
/// `scan_result.parseable_files.iter().map(|f| f.path.as_str())`), using
/// the repo's own `tsconfig` `path_aliases` (from `scan::ScanResult`).
pub fn resolve_all<'a>(
    vfs: &Vfs,
    path_aliases: &HashMap<String, Vec<String>>,
    paths: impl Iterator<Item = &'a str>,
) -> ResolveBatch {
    let resolver = build_resolver(vfs, path_aliases);
    let mut batch = ResolveBatch::default();

    for path in paths {
        match resolve_file(vfs, &resolver, path) {
            Ok(edges) => batch.edges.extend(edges),
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<'a>(allocator: &'a Allocator, source: &'a str) -> oxc_parser::ParserReturn<'a> {
        let source_type = resolve_source_type("test.ts");
        Parser::new(allocator, source, source_type).parse()
    }

    #[test]
    fn resolves_local_import_to_real_file() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"import { helper } from './helper';".to_vec());
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        assert!(!edges[0].is_re_export);
        match &edges[0].target {
            ImportTarget::LocalFile(path) => assert_eq!(path, "src/helper.ts"),
            other => panic!("expected LocalFile, got {other:?}"),
        }
    }

    #[test]
    fn unresolvable_specifier_falls_back_to_external_never_errors() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"import express from 'express';".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        match &edges[0].target {
            ImportTarget::External(name) => assert_eq!(name, "express"),
            other => panic!("expected External, got {other:?}"),
        }
    }

    #[test]
    fn scoped_and_deep_import_paths_reduce_to_package_root() {
        assert_eq!(reduce_to_package_name("lodash/get"), "lodash");
        assert_eq!(reduce_to_package_name("@scope/pkg/sub/path"), "@scope/pkg");
        assert_eq!(reduce_to_package_name("react"), "react");
    }

    #[test]
    fn export_from_and_export_star_are_collected_as_re_exports() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/index.ts",
            b"export { helper } from './helper';\nexport * from './other';".to_vec(),
        );
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());
        vfs.insert("src/other.ts", b"export const x = 1;".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/index.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/index.ts", &ret.program);

        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.is_re_export));
    }

    #[test]
    fn same_file_export_named_declaration_is_not_collected_as_an_import() {
        // `export { helper };` with NO `from` clause is a same-file
        // re-export, not an import — must not appear here at all.
        let mut vfs = Vfs::empty();
        vfs.insert("src/index.ts", b"function helper() {}\nexport { helper };".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/index.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/index.ts", &ret.program);

        assert_eq!(edges.len(), 0);
    }

    #[test]
    fn tsconfig_alias_resolves_to_real_file() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"import { helper } from '@/helper';".to_vec());
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());

        let mut aliases = HashMap::new();
        aliases.insert("@/*".to_string(), vec!["src/*".to_string()]);

        let resolver = build_resolver(&vfs, &aliases);
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        match &edges[0].target {
            ImportTarget::LocalFile(path) => assert_eq!(path, "src/helper.ts"),
            other => panic!("expected LocalFile via alias, got {other:?}"),
        }
    }

    #[test]
    fn bare_wildcard_alias_pattern_is_skipped_not_guessed() {
        let mut aliases = HashMap::new();
        aliases.insert("*".to_string(), vec!["./src/types/*".to_string()]);
        let alias = tsconfig_aliases_to_resolver_alias(&aliases);
        assert!(alias.is_empty(), "bare '*' pattern has no well-defined alias meaning and should be skipped");
    }

    #[test]
    fn mixed_valid_and_bare_wildcard_aliases_keeps_only_the_valid_one() {
        let mut aliases = HashMap::new();
        aliases.insert("*".to_string(), vec!["./src/types/*".to_string()]);
        aliases.insert("@/*".to_string(), vec!["src/*".to_string()]);
        let alias = tsconfig_aliases_to_resolver_alias(&aliases);
        assert_eq!(alias.len(), 1);
        assert_eq!(alias[0].0, "@");
    }

    #[test]
    fn bare_wildcard_alias_does_not_cause_resolver_recursion_end_to_end() {
        // Regression: the exact real-world failure — this pattern, left
        // unfiltered, sent oxc_resolver into ResolveError::Recursion on
        // EVERY relative import (0/374 resolved in a real run). This test
        // reproduces the actual resolve() call path, not just the alias
        // list construction, so a future regression here would be caught
        // by an actual resolution failure, not just an empty-list check.
        let mut vfs = Vfs::empty();
        vfs.insert("src/auth/authorization.ts", b"import { helper } from '../helpers/asyncHandler';".to_vec());
        vfs.insert("src/helpers/asyncHandler.ts", b"export function helper() {}".to_vec());

        let mut aliases = HashMap::new();
        aliases.insert("*".to_string(), vec!["./src/types/*".to_string()]);

        let resolver = build_resolver(&vfs, &aliases);
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/auth/authorization.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/auth/authorization.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        match &edges[0].target {
            ImportTarget::LocalFile(path) => assert_eq!(path, "src/helpers/asyncHandler.ts"),
            other => panic!("expected LocalFile, got {other:?} — bare '*' alias likely leaked through again"),
        }
    }

    #[test]
    fn detects_whole_declaration_type_only_import() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"import type { Config } from './config';".to_vec());
        vfs.insert("src/config.ts", b"export interface Config {}".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        assert!(edges[0].is_type_only);
    }

    #[test]
    fn regular_value_import_is_not_type_only() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/app.ts", b"import { helper } from './helper';".to_vec());
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert!(!edges[0].is_type_only);
    }

    #[test]
    fn captures_local_binding_names_across_all_specifier_kinds() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/app.ts",
            b"import def, { named as renamed, plain } from './mixed';\nimport * as ns from './mixed';"
                .to_vec(),
        );
        vfs.insert("src/mixed.ts", b"export const x = 1;".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/app.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/app.ts", &ret.program);

        assert_eq!(edges.len(), 2);

        let local_names: Vec<&str> = edges[0].local_bindings.iter().map(|b| b.local_name.as_str()).collect();
        assert_eq!(local_names, vec!["def", "renamed", "plain"]);

        // "named as renamed" -> local "renamed", but imported_name must be
        // the ORIGINAL exported name "named" — this is exactly the
        // distinction that matters for cross-file symbol matching in
        // stage 3b: the target file exports "named", not "renamed".
        let renamed_binding = &edges[0].local_bindings[1];
        assert_eq!(renamed_binding.local_name, "renamed");
        assert_eq!(renamed_binding.imported_name.as_deref(), Some("named"));

        // "plain" (no "as") -> imported_name equals local_name.
        let plain_binding = &edges[0].local_bindings[2];
        assert_eq!(plain_binding.imported_name.as_deref(), Some("plain"));

        // Default import -> no single named export to point at.
        assert_eq!(edges[0].local_bindings[0].imported_name, None);

        // import * as ns -> single local binding "ns", no single original name.
        assert_eq!(edges[1].local_bindings.len(), 1);
        assert_eq!(edges[1].local_bindings[0].local_name, "ns");
        assert_eq!(edges[1].local_bindings[0].imported_name, None);
    }

    #[test]
    fn re_export_has_no_local_bindings() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/index.ts", b"export { helper } from './helper';".to_vec());
        vfs.insert("src/helper.ts", b"export function helper() {}".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/index.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/index.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        assert!(edges[0].local_bindings.is_empty());
    }

    #[test]
    fn resolves_bare_import_to_a_declaration_file() {
        // Regression: the exact real-world failure — a bare
        // "../types/app-request" import must resolve to
        // "app-request.d.ts" (a real, common pattern for shared TS type
        // definitions), not silently fail because only ".ts" was tried.
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/helpers/permission.ts",
            b"import { ProtectedRequest } from '../types/app-request';".to_vec(),
        );
        vfs.insert("src/types/app-request.d.ts", b"export interface ProtectedRequest {}".to_vec());

        let resolver = build_resolver(&vfs, &HashMap::new());
        let allocator = Allocator::default();
        let ret = parse(&allocator, vfs.read_utf8("src/helpers/permission.ts").unwrap());
        let edges = resolve_imports_for_file(&resolver, &vfs, "src/helpers/permission.ts", &ret.program);

        assert_eq!(edges.len(), 1);
        match &edges[0].target {
            ImportTarget::LocalFile(path) => assert_eq!(path, "src/types/app-request.d.ts"),
            other => panic!("expected LocalFile resolving to the .d.ts file, got {other:?}"),
        }
    }
}
