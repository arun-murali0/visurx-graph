//! Tracks the enclosing named-scope stack while walking a file's AST, used
//! to build a symbol's structural ID (`file::scope_path::name::kind`).
//! Adopted from the proven v1 design: `ScopeGuard` (RAII push/pop via
//! `Drop`) instead of manual paired `push()`/`pop()` calls, so an early
//! return inside a visitor method can never accidentally leave a stale
//! scope segment behind.

#[derive(Debug, Clone, Default)]
pub struct ScopePath {
    segments: Vec<String>,
}

impl ScopePath {
    pub fn new() -> Self {
        Self::default()
    }

    /// `pub(super)`: visible to sibling modules under `core::semantic`
    /// (e.g. `symbol_classify`), which call this directly in spots where
    /// `ScopeGuard` would hold a conflicting borrow — see
    /// `symbol_classify::visit_class`'s comment for why.
    pub(super) fn push(&mut self, name: &str) {
        self.segments.push(name.to_string());
    }

    pub(super) fn pop(&mut self) {
        self.segments.pop();
    }

    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    pub fn is_top_level(&self) -> bool {
        self.segments.is_empty()
    }

    /// `::`-joined path of enclosing scopes, e.g. `"Counter"` while inside
    /// that class. Empty string at top level.
    pub fn as_str(&self) -> String {
        self.segments.join("::")
    }
}

/// Push `name` onto `path` now, pop it automatically when this guard drops
/// — including on an early `return` inside whatever scope it's held in.
pub struct ScopeGuard<'a> {
    path: &'a mut ScopePath,
}

impl<'a> ScopeGuard<'a> {
    pub fn enter(path: &'a mut ScopePath, name: &str) -> Self {
        path.push(name);
        Self { path }
    }

    pub fn depth(&self) -> usize {
        self.path.depth()
    }
}

impl<'a> Drop for ScopeGuard<'a> {
    fn drop(&mut self) {
        self.path.pop();
    }
}

/// Lets a caller read the scoped path (e.g. for `build_structural_id`)
/// THROUGH the guard while it's held, instead of via the original
/// `&ScopePath` variable — which would conflict with the guard's own held
/// `&mut` borrow of that same value for as long as the guard is alive.
impl<'a> std::ops::Deref for ScopeGuard<'a> {
    type Target = ScopePath;

    fn deref(&self) -> &ScopePath {
        self.path
    }
}

pub fn build_structural_id(
    file_path: &str,
    scope_path: &ScopePath,
    name: &str,
    kind: &str,
) -> String {
    if scope_path.is_top_level() {
        format!("{file_path}::{name}::{kind}")
    } else {
        format!("{file_path}::{}::{name}::{kind}", scope_path.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_symbol_has_no_scope_segment() {
        let path = ScopePath::new();
        let id = build_structural_id("src/counter.ts", &path, "add", "function");
        assert_eq!(id, "src/counter.ts::add::function");
    }

    #[test]
    fn nested_method_includes_class_scope() {
        let mut path = ScopePath::new();
        {
            let guard = ScopeGuard::enter(&mut path, "Counter");
            // Read THROUGH the guard (via Deref), not the original `path`
            // variable — `path` is under a live `&mut` borrow held by
            // `guard` for as long as it exists, so `&path` directly here
            // would conflict with it, same as the bug this was written to
            // catch in the first place.
            let id = build_structural_id("src/counter.ts", &guard, "increment", "method");
            assert_eq!(id, "src/counter.ts::Counter::increment::method");
        }
        assert!(path.is_top_level());
    }

    #[test]
    fn guard_pops_automatically_on_drop_even_via_early_return() {
        let mut path = ScopePath::new();

        fn enters_and_returns_early(path: &mut ScopePath, bail: bool) -> usize {
            let guard = ScopeGuard::enter(path, "Counter");
            if bail {
                return guard.depth(); // early return — guard still drops here
            }
            0
        }

        let depth_at_return = enters_and_returns_early(&mut path, true);
        assert_eq!(depth_at_return, 1);
        assert!(
            path.is_top_level(),
            "guard must have popped despite the early return"
        );
    }

    #[test]
    fn two_methods_same_name_different_class_get_different_ids() {
        let mut path_a = ScopePath::new();
        path_a.push("Dog");
        let id_a = build_structural_id("src/animals.ts", &path_a, "speak", "method");

        let mut path_b = ScopePath::new();
        path_b.push("Cat");
        let id_b = build_structural_id("src/animals.ts", &path_b, "speak", "method");

        assert_ne!(id_a, id_b);
    }
}
