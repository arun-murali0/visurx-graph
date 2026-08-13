//! `oxc_resolver::FileSystem` implementation backed by our in-memory `Vfs`,
//! so import resolution runs entirely against zip-derived data — never
//! touches the real disk, works identically on native and (eventually)
//! wasm.
//!
//! Verified directly against the real pinned `oxc_resolver` version
//! (`v11.23.0`, matching `Cargo.toml`) rather than assumed:
//! `FileMetadata::new(is_file, is_dir, is_symlink)` takes THREE bools, not
//! two — exactly the kind of parameter-order mistake flagged in project
//! notes as a recurring failure mode when guessing at this crate's API.

use crate::vfs::Vfs;
use oxc_resolver::{FileMetadata, FileSystem, ResolveError};
use std::io;
use std::path::{Path, PathBuf};

/// Normalizes a resolver-provided path (which may carry a leading `/`,
/// platform separators, or un-collapsed `..`/`.` components) into the
/// flat, no-leading-slash key scheme our `Vfs` uses internally.
///
/// The `..`/`.` collapsing is load-bearing, not cosmetic: when the
/// resolver joins a file's directory with a relative specifier (e.g.
/// `src/auth` + `../helpers/asyncHandler`), `Path::join` does NOT collapse
/// the result — it stays literally `src/auth/../helpers/asyncHandler`.
/// Without collapsing that here, the lookup against our `Vfs` (which only
/// ever has clean keys like `src/helpers/asyncHandler.ts`) always misses,
/// silently failing resolution for every `../`-style import. Confirmed as
/// a real bug via actual repo output before this fix — every relative
/// import one directory level up or more was falling through to
/// `ImportTarget::External("..")`, a nonsensical "package name".
pub fn normalize_key(path: &Path) -> String {
    let mut segments: Vec<&str> = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                segments.pop();
            }
            std::path::Component::CurDir | std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
            std::path::Component::Normal(s) => {
                if let Some(s) = s.to_str() {
                    segments.push(s);
                }
            }
        }
    }

    segments.join("/")
}

#[derive(Clone)]
pub struct VfsFileSystem {
    vfs: Vfs,
}

impl VfsFileSystem {
    pub fn new(vfs: Vfs) -> Self {
        Self { vfs }
    }
}

impl FileSystem for VfsFileSystem {
    fn new() -> Self {
        // The trait requires a zero-arg constructor for generic contexts we
        // never use — we always construct via `VfsFileSystem::new(vfs)`
        // and pass that instance to `ResolverGeneric::new_with_file_system`.
        // Panicking here is intentional: it means something called this
        // unreachable path, which is itself a bug worth surfacing loudly
        // rather than silently returning an empty, useless filesystem.
        panic!(
            "VfsFileSystem must be constructed via VfsFileSystem::new(vfs); \
             the zero-arg FileSystem::new() is not supported"
        )
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let key = normalize_key(path);
        self.vfs.read(&key).map(<[u8]>::to_vec).ok_or_else(|| not_found(&key))
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let key = normalize_key(path);
        self.vfs.read_utf8(&key).map(str::to_string).ok_or_else(|| not_found(&key))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let key = normalize_key(path);

        if self.vfs.exists(&key) {
            return Ok(FileMetadata::new(true, false, false));
        }

        // Directory-index resolution fix (adopted from a proven earlier
        // version of this resolver integration): the resolver needs to
        // know a path like `src/database` is a real directory (so it can
        // then try `src/database/index.ts`) even though our `Vfs` only
        // ever stores flat file keys, never explicit directory entries.
        // A directory "exists" here if any stored key starts with this
        // path as a genuine path-segment prefix (trailing `/`), not just
        // a string prefix (`src/db` must not match `src/database/x.ts`).
        let dir_prefix = format!("{}/", key.trim_end_matches('/'));
        let is_dir = key.is_empty() || self.vfs.paths().any(|p| p.starts_with(&dir_prefix));

        if is_dir {
            Ok(FileMetadata::new(false, true, false))
        } else {
            Err(not_found(&key))
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        // No symlinks exist in a zip-derived Vfs — same check as a real
        // file/directory lookup.
        self.metadata(path)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        Err(ResolveError::PathNotSupported(path.to_path_buf()))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        // Paths in this Vfs are already flat and relative — nothing to
        // resolve further, just normalize separators/leading slash the
        // same way every other method here does.
        Ok(PathBuf::from(normalize_key(path)))
    }
}

fn not_found(key: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_existing_file() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/add.ts", b"export const x = 1;".to_vec());
        let fs = VfsFileSystem::new(vfs);

        let content = fs.read_to_string(Path::new("src/add.ts")).unwrap();
        assert_eq!(content, "export const x = 1;");
    }

    #[test]
    fn missing_file_is_not_found_error() {
        let fs = VfsFileSystem::new(Vfs::empty());
        assert!(fs.read(Path::new("src/missing.ts")).is_err());
    }

    #[test]
    fn directory_prefix_is_recognized_without_explicit_entry() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/database/index.ts", b"export {};".to_vec());
        let fs = VfsFileSystem::new(vfs);

        let meta = fs.metadata(Path::new("src/database")).unwrap();
        assert!(meta.is_dir());
        assert!(!meta.is_file());
    }

    #[test]
    fn similarly_named_directory_does_not_false_match() {
        // "src/db" must not be treated as a directory just because
        // "src/database/index.ts" shares a string prefix.
        let mut vfs = Vfs::empty();
        vfs.insert("src/database/index.ts", b"export {};".to_vec());
        let fs = VfsFileSystem::new(vfs);

        assert!(fs.metadata(Path::new("src/db")).is_err());
    }

    #[test]
    fn normalize_key_collapses_parent_dir_components() {
        // Regression: the exact real-world bug — src/auth/../helpers/x
        // must collapse to src/helpers/x, not stay literal (which would
        // never match any real Vfs key and silently break every
        // "../"-style import).
        let path = Path::new("src/auth/../helpers/asyncHandler.ts");
        assert_eq!(normalize_key(path), "src/helpers/asyncHandler.ts");
    }

    #[test]
    fn normalize_key_collapses_multiple_parent_dir_levels() {
        let path = Path::new("src/database/repository/../../helpers/utils.ts");
        assert_eq!(normalize_key(path), "src/helpers/utils.ts");
    }
}
