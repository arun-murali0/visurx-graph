use crate::error::EngineError;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use walkdir::WalkDir;
use zip::ZipArchive;

const EXCLUDED_PATH_SEGMENTS: [&str; 2] = ["node_modules", ".git"];

fn is_excluded_path(path: &str) -> bool {
    path.split('/')
        .any(|segment| EXCLUDED_PATH_SEGMENTS.contains(&segment))
}

const ZIP_MAGIC_BYTES: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

#[derive(Clone)]
pub struct Vfs {
    files: Arc<HashMap<String, Vec<u8>>>,
}

impl Vfs {
    pub fn empty() -> Self {
        Vfs {
            files: Arc::new(HashMap::new()),
        }
    }

    pub fn from_dir(root: &Path) -> Result<Self, EngineError> {
        let mut files = HashMap::new();

        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }

            let full_path = entry.path();
            let relative_path = full_path
                .strip_prefix(root)
                .map_err(|e| EngineError::Io {
                    path: full_path.display().to_string(),
                    detail: format!("could not compute path relative to {}: {e}", root.display()),
                })?
                .to_string_lossy()
                .replace('\\', "/");

            if is_excluded_path(&relative_path) {
                continue;
            }

            let bytes = std::fs::read(full_path).map_err(|e| EngineError::Io {
                path: full_path.display().to_string(),
                detail: e.to_string(),
            })?;

            files.insert(relative_path, bytes);
        }

        Ok(Vfs {
            files: Arc::new(files),
        })
    }

    pub fn from_zip_bytes(bytes: &[u8]) -> Result<Self, EngineError> {
        if bytes.len() < ZIP_MAGIC_BYTES.len()
            || &bytes[..ZIP_MAGIC_BYTES.len()] != &ZIP_MAGIC_BYTES[..]
        {
            return Err(EngineError::InvalidZipHeader {
                detail: format!(
                    "expected a zip file starting with {ZIP_MAGIC_BYTES:02x?}, got {} byte(s)",
                    bytes.len()
                ),
            });
        }

        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| EngineError::ZipCorrupted {
                reason: e.to_string(),
            })?;

        let mut files = HashMap::with_capacity(archive.len());

        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| EngineError::ZipCorrupted {
                    reason: format!("could not read entry {index}: {e}"),
                })?;

            if entry.is_dir() {
                continue;
            }

            let raw_name = entry.name().to_string();
            let path = raw_name
                .split_once('/')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or(raw_name)
                .replace('\\', "/");

            if path.is_empty() || is_excluded_path(&path) {
                continue;
            }

            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| EngineError::ZipCorrupted {
                    reason: format!("could not read contents of '{path}': {e}"),
                })?;

            files.insert(path, buf);
        }

        files.shrink_to_fit();

        Ok(Vfs {
            files: Arc::new(files),
        })
    }

    pub fn read(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(|v| v.as_slice())
    }

    pub fn read_utf8(&self, path: &str) -> Option<&str> {
        self.read(path).and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn insert(&mut self, path: &str, content: Vec<u8>) {
        Arc::make_mut(&mut self.files).insert(path.to_string(), content);
    }

    pub fn remove(&mut self, path: &str) -> Option<Vec<u8>> {
        Arc::make_mut(&mut self.files).remove(path)
    }

    pub fn get_size(&self, path: &str) -> Option<usize> {
        self.files.get(path).map(|v| v.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_git_and_node_modules_at_any_depth() {
        assert!(is_excluded_path(".git/HEAD"));
        assert!(is_excluded_path("node_modules/react/index.js"));
        assert!(is_excluded_path("packages/api/node_modules/lodash/get.js"));
        assert!(!is_excluded_path("src/node_modules_helper.ts"));
        assert!(!is_excluded_path("src/components/Button.tsx"));
    }
}
