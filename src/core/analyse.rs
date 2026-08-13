use crate::core::parse::source_type::resolve_source_type;
use crate::core::parse::ParseOutcome;
use crate::core::semantic::symbol_classify::{self, Symbol};
use crate::error::FileError;
use crate::panic_safety::run_file_safely;
use crate::vfs::Vfs;
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct FileAnalysis {
    pub parse: ParseOutcome,
    pub symbols: Vec<Symbol>,
    pub cfg_available: bool,
}

pub fn analyze_file(vfs: &Vfs, path: &str) -> Result<FileAnalysis, FileError> {
    let source_text = vfs.read_utf8(path).ok_or_else(|| FileError::InvalidUtf8 {
        path: path.to_string(),
    })?;
    let source_type = resolve_source_type(path);

    run_file_safely(path, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source_text, source_type).parse();

        let parse = ParseOutcome {
            path: path.to_string(),
            panicked: ret.panicked,
            diagnostic_count: ret.diagnostics.len(),
            is_typescript: source_type.is_typescript(),
            is_jsx: source_type.is_jsx(),
        };

        let symbols = symbol_classify::extract_symbols(&ret.program, path);

        let semantic_ret = SemanticBuilder::new_compiler()
            .with_cfg(true)
            .build(&ret.program);
        let cfg_available = semantic_ret.semantic.cfg().is_some();

        Ok(FileAnalysis {
            parse,
            symbols,
            cfg_available,
        })
    })
}

#[derive(Debug, Default, Serialize)]
pub struct AnalysisBatch {
    pub files: Vec<FileAnalysis>,
    pub failures: Vec<FileError>,
}

pub fn analyze_all<'a>(vfs: &Vfs, paths: impl Iterator<Item = &'a str>) -> AnalysisBatch {
    let mut batch = AnalysisBatch::default();

    for path in paths {
        match analyze_file(vfs, path) {
            Ok(analysis) => batch.files.push(analysis),
            Err(err) => batch.failures.push(err),
        }
    }

    batch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pass_yields_both_parse_health_and_symbols() {
        let mut vfs = Vfs::empty();
        vfs.insert(
            "src/add.ts",
            b"export function add(a: number, b: number) { return a + b; }".to_vec(),
        );

        let analysis = analyze_file(&vfs, "src/add.ts").unwrap();
        assert!(!analysis.parse.has_parse_errors());
        assert_eq!(analysis.symbols.len(), 1);
        assert_eq!(analysis.symbols[0].name, "add");
        assert!(analysis.symbols[0].exported);
    }

    #[test]
    fn analyze_all_tracks_failures_separately_from_successful_files() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/ok.ts", b"export const x = 1;".to_vec());

        let batch = analyze_all(&vfs, vec!["src/ok.ts", "src/missing.ts"].into_iter());
        assert_eq!(batch.files.len(), 1);
        assert_eq!(batch.failures.len(), 1);
    }
}
