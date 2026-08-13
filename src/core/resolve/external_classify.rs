//! Classifies every `ImportTarget::External` against two independent
//! signals — is it a Node.js builtin, and is it actually declared in this
//! repo's `package.json` — instead of leaving "external" as one flat,
//! unqueryable bucket.
//!
//! `ExternalCategory::Undeclared` is the interesting one: an import that's
//! neither a Node builtin nor listed in `dependencies`/`devDependencies`
//! at all — a real signal (phantom dependency riding in via a transitive
//! install, a typo, or a genuinely missing `package.json` entry), not
//! just a classification nicety.

use crate::scan::PackageJsonData;
use serde::Serialize;
use std::collections::HashMap;

/// Node.js builtin module names (unprefixed — `"fs"`, not `"node:fs""`).
/// Deliberately a fixed, hand-maintained list rather than pulled from some
/// runtime introspection (there is no such thing available here — this
/// crate never runs inside Node itself). Covers the stable, commonly
/// imported builtins; less common ones (e.g. `"diagnostics_channel"`,
/// `"inspector"`) can be added if a real repo run surfaces them missing.
pub const NODE_BUILTIN_MODULES: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// `"node:fs"` and `"fs"` are the same builtin — the `node:` prefix is an
/// explicit-builtin marker some codebases use, not a different module.
pub fn is_node_builtin(package_name: &str) -> bool {
    let name = package_name.strip_prefix("node:").unwrap_or(package_name);
    NODE_BUILTIN_MODULES.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExternalCategory {
    NodeBuiltin,
    DeclaredDependency,
    DeclaredDevDependency,
    /// Imported somewhere in the repo, but not a Node builtin AND not
    /// listed in `package.json`'s `dependencies` or `devDependencies` at
    /// all — the real signal this classification exists to surface.
    Undeclared,
}

pub fn classify_external(
    package_name: &str,
    package_data: Option<&PackageJsonData>,
) -> ExternalCategory {
    if is_node_builtin(package_name) {
        return ExternalCategory::NodeBuiltin;
    }
    if let Some(data) = package_data {
        if data.dependencies.iter().any(|d| d == package_name) {
            return ExternalCategory::DeclaredDependency;
        }
        if data.dev_dependencies.iter().any(|d| d == package_name) {
            return ExternalCategory::DeclaredDevDependency;
        }
    }
    ExternalCategory::Undeclared
}

#[derive(Debug, Default, Serialize)]
pub struct ExternalSummary {
    /// Each map is package/module name -> number of import sites across
    /// the repo, so this is directly queryable ("how many places import
    /// X", "what's actually undeclared") without re-deriving it from the
    /// raw edge list every time.
    pub node_builtins: HashMap<String, usize>,
    pub declared_dependencies: HashMap<String, usize>,
    pub declared_dev_dependencies: HashMap<String, usize>,
    pub undeclared: HashMap<String, usize>,
}

pub fn summarize_externals<'a>(
    external_package_names: impl Iterator<Item = &'a str>,
    package_data: Option<&PackageJsonData>,
) -> ExternalSummary {
    let mut summary = ExternalSummary::default();

    for name in external_package_names {
        let bucket = match classify_external(name, package_data) {
            ExternalCategory::NodeBuiltin => &mut summary.node_builtins,
            ExternalCategory::DeclaredDependency => &mut summary.declared_dependencies,
            ExternalCategory::DeclaredDevDependency => &mut summary.declared_dev_dependencies,
            ExternalCategory::Undeclared => &mut summary.undeclared,
        };
        *bucket.entry(name.to_string()).or_insert(0) += 1;
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_data(deps: &[&str], dev_deps: &[&str]) -> PackageJsonData {
        PackageJsonData {
            scripts: Default::default(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            dev_dependencies: dev_deps.iter().map(|s| s.to_string()).collect(),
            main_entry: None,
        }
    }

    #[test]
    fn classifies_node_builtin_with_and_without_prefix() {
        assert_eq!(classify_external("fs", None), ExternalCategory::NodeBuiltin);
        assert_eq!(
            classify_external("node:fs", None),
            ExternalCategory::NodeBuiltin
        );
    }

    #[test]
    fn classifies_declared_dependency_vs_dev_dependency() {
        let data = package_data(&["express"], &["jest"]);
        assert_eq!(
            classify_external("express", Some(&data)),
            ExternalCategory::DeclaredDependency
        );
        assert_eq!(
            classify_external("jest", Some(&data)),
            ExternalCategory::DeclaredDevDependency
        );
    }

    #[test]
    fn classifies_undeclared_when_not_a_builtin_and_not_in_package_json() {
        let data = package_data(&["express"], &[]);
        assert_eq!(
            classify_external("left-pad", Some(&data)),
            ExternalCategory::Undeclared
        );
    }

    #[test]
    fn no_package_data_at_all_still_correctly_identifies_builtins() {
        assert_eq!(
            classify_external("path", None),
            ExternalCategory::NodeBuiltin
        );
        assert_eq!(
            classify_external("some-package", None),
            ExternalCategory::Undeclared
        );
    }

    #[test]
    fn summarize_counts_repeated_imports_of_the_same_name() {
        let data = package_data(&["express"], &[]);
        let names = vec!["express", "express", "fs", "left-pad"];
        let summary = summarize_externals(names.into_iter(), Some(&data));

        assert_eq!(summary.declared_dependencies.get("express"), Some(&2));
        assert_eq!(summary.node_builtins.get("fs"), Some(&1));
        assert_eq!(summary.undeclared.get("left-pad"), Some(&1));
    }
}
