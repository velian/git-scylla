use std::path::Path;

/// Directory names never descended into: dependency and build trees, caches,
/// and macOS system trees.
pub const HARD_SKIP_NAMES: &[&str] = &[
    "node_modules",
    "target",
    ".build",
    "Pods",
    ".Trash",
    "Library",
    "System",
    "Volumes",
    ".cache",
    ".gradle",
    "DerivedData",
];

/// Should this directory be skipped outright?
///
/// `roots` are exempt from the skip list.
pub fn is_hard_skipped(dir: &Path, roots: &[std::path::PathBuf]) -> bool {
    if roots.iter().any(|r| r == dir) {
        return false;
    }
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Only skipped where macOS actually puts them; a project directory named
    // `System` is a project directory.
    if matches!(name, "Library" | "System" | "Volumes") {
        let anchored = dir.parent().is_some_and(|p| {
            p == Path::new("/")
                || p.parent() == Some(Path::new("/Users"))
                || p == Path::new("/Users")
        });
        return anchored;
    }
    HARD_SKIP_NAMES.contains(&name)
}

/// Is this a `.git` directory? Never descended into or classified: `HEAD`,
/// `objects/` and `refs/` inside it would otherwise match the bare-repository
/// test. A *bare* repository is named `<name>.git`, not `.git`, and is
/// unaffected.
pub fn is_git_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == ".git")
}

/// Does this look like an iCloud Drive dataless placeholder? Statting the
/// entry to check more precisely would itself trigger materialization, so
/// this matches on the `.icloud` sidecar naming alone.
pub fn looks_dataless(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".icloud") || (name.starts_with('.') && name.contains(".icloud"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_dirs_are_skipped_anywhere() {
        assert!(is_hard_skipped(Path::new("/a/b/node_modules"), &[]));
        assert!(is_hard_skipped(Path::new("/a/b/target"), &[]));
        assert!(!is_hard_skipped(Path::new("/a/b/src"), &[]));
    }

    #[test]
    fn system_names_are_skipped_only_where_macos_puts_them() {
        assert!(is_hard_skipped(Path::new("/System"), &[]));
        assert!(is_hard_skipped(Path::new("/Users/x/Library"), &[]));
        assert!(is_hard_skipped(Path::new("/Volumes"), &[]));
        assert!(!is_hard_skipped(Path::new("/Users/x/code/System"), &[]));
        assert!(!is_hard_skipped(Path::new("/Users/x/code/Library"), &[]));
    }

    #[test]
    fn an_explicit_root_beats_the_skip_list() {
        let roots = vec![PathBuf::from("/Volumes/work")];
        assert!(!is_hard_skipped(Path::new("/Volumes/work"), &roots));
        assert!(is_hard_skipped(Path::new("/Volumes/work/x/node_modules"), &roots));
    }

    #[test]
    fn git_dirs_are_not_repositories() {
        assert!(is_git_dir(Path::new("/a/b/.git")));
        assert!(!is_git_dir(Path::new("/a/b/bare.git")));
        assert!(!is_git_dir(Path::new("/a/b/src")));
    }

    #[test]
    fn dataless_placeholders() {
        assert!(looks_dataless(Path::new("/a/.big.psd.icloud")));
        assert!(looks_dataless(Path::new("/a/thing.icloud")));
        assert!(!looks_dataless(Path::new("/a/icloud-notes")));
        assert!(!looks_dataless(Path::new("/a/repo")));
    }
}
