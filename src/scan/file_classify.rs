use serde::Serialize;

const PARSEABLE_EXTENSIONS: [&str; 8] = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts"];

#[derive(Clone, Debug, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub size_bytes: u32,
    pub is_test: bool,
    pub is_parseable: bool,
    pub is_vendor: bool,
}

pub fn is_parseable_extension(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    PARSEABLE_EXTENSIONS.contains(&ext)
}

pub fn is_test_file(path: &str) -> bool {
    const TEST_DIR_SEGMENTS: [&str; 3] = ["test", "tests", "__tests__"];

    let in_test_dir = path
        .split('/')
        .rev()
        .skip(1) // skip the filename itself — only directory segments count here
        .any(|segment| TEST_DIR_SEGMENTS.contains(&segment));

    in_test_dir
        || path.ends_with(".test.js")
        || path.ends_with(".test.jsx")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.js")
        || path.ends_with(".spec.jsx")
        || path.ends_with(".spec.ts")
        || path.ends_with(".spec.tsx")
}

pub fn is_vendor_path(path: &str) -> bool {
    path.contains("node_modules/")
}

pub fn classify_file(path: &str, size_bytes: u32) -> FileEntry {
    FileEntry {
        path: path.to_string(),
        size_bytes,
        is_test: is_test_file(path),
        is_parseable: is_parseable_extension(path),
        is_vendor: is_vendor_path(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_parseable_extensions() {
        assert!(is_parseable_extension("src/app.tsx"));
        assert!(is_parseable_extension("src/util.mjs"));
        assert!(!is_parseable_extension("README.md"));
        assert!(!is_parseable_extension("package.json"));
    }

    #[test]
    fn recognizes_test_files_without_false_positives() {
        assert!(is_test_file("src/foo.test.ts"));
        assert!(is_test_file("src/__tests__/foo.ts"));
        assert!(!is_test_file("src/my.test.folder/component.ts"));
    }

    #[test]
    fn recognizes_root_level_and_nested_tests_directory() {
        assert!(is_test_file("tests/setup.ts"));
        assert!(is_test_file("tests/auth/authUtils/mock.ts"));
        assert!(is_test_file("src/tests/helper.ts"));
    }
}
