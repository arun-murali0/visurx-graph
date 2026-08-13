use oxc_span::SourceType;
use std::path::Path;

pub fn resolve_source_type(file_path: &str) -> SourceType {
    let path = Path::new(file_path);
    SourceType::from_path(path).unwrap_or_else(|_| {
        SourceType::default()
            .with_typescript(true)
            .with_jsx(true)
            .with_module(true)
    })
}
