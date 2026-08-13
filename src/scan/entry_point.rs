use crate::vfs::Vfs;

const ENTRY_POINT_CANDIDATES: [&str; 6] = [
    "src/index.ts",
    "src/index.tsx",
    "src/main.ts",
    "index.ts",
    "index.js",
    "src/index.js",
];

fn guess_entry_point(vfs: &Vfs, active_root: &str) -> Option<String> {
    ENTRY_POINT_CANDIDATES
        .iter()
        .map(|candidate| {
            if active_root.is_empty() {
                candidate.to_string()
            } else {
                format!("{active_root}{candidate}")
            }
        })
        .find(|candidate_path| vfs.exists(candidate_path))
}

pub fn resolve_entry_point(
    vfs: &Vfs,
    active_root: &str,
    manual_override: Option<&str>,
) -> Option<String> {
    if let Some(path) = manual_override {
        return vfs.exists(path).then(|| path.to_string());
    }
    guess_entry_point(vfs, active_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_override_wins_when_valid() {
        let mut vfs = Vfs::empty();
        vfs.insert("src/main.ts", b"export {}".to_vec());
        vfs.insert("src/app.ts", b"export {}".to_vec());
        assert_eq!(
            resolve_entry_point(&vfs, "", Some("src/app.ts")),
            Some("src/app.ts".to_string())
        );
    }

    #[test]
    fn invalid_manual_override_returns_none_rather_than_silently_falling_back() {
        let mut vfs = Vfs::empty();
        vfs.insert("index.ts", b"export {}".to_vec());
        assert_eq!(
            resolve_entry_point(&vfs, "", Some("src/does-not-exist.ts")),
            None
        );
    }
}
