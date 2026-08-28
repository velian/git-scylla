use std::path::Path;

/// Directory names never descended into.
///
/// Two different reasons, mixed because the action is the same: dependency and
/// build directories holding thousands of files and the occasional vendored
/// `.git` nobody wants, and system trees where the walk can only lose time or
/// trip over TCC.
pub const HARD_SKIP_NAMES: &[&str] = &[
    "node_modules",
    "target",
    ".build",
    "Pods",
    ".Trash",
    "Library",
    "System",
    "Volumes",
    // Caches whose contents are never a repository the user means.
    ".cache",
    ".gradle",
    "DerivedData",
];

/// Should this directory be skipped outright?
///
/// `roots` are exempt: naming `/Volumes/work` as a root is an explicit request
/// that must beat the generic skip list.
pub fn is_hard_skipped(dir: &Path, roots: &[std::path::PathBuf]) -> bool {
    if roots.iter().any(|r| r == dir) {
        return false;
    }
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // `Library` and `System` are only meaningful at the places macOS puts them;
    // a project directory called `System` is a project directory. Matching them
    // by bare name everywhere would silently drop real repositories.
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

/// Is this a `.git` directory?
///
/// Never descended into, and never classified. A `.git` directory contains
/// `HEAD`, `objects/` and `refs/`, so the bare-repository test matches it
/// exactly — and with `--nested` that turns every repository into two, plus one
/// more for every submodule under `.git/modules/`. A `.git` directory is
/// machinery, not a repository, and nothing inside one is either.
///
/// A *bare* repository is unaffected: it is conventionally named `<name>.git`,
/// not `.git`.
pub fn is_git_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == ".git")
}

/// Does this look like an iCloud Drive dataless placeholder?
///
/// Traversing into one asks the file provider to materialise it, turning a local
/// scan into a download. The `.icloud` sidecar naming is a cheap, reliable
/// signal; statting the entry to ask more precisely is itself what triggers the
/// download.
pub fn looks_dataless(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // `~/Documents/foo.pdf` evicted from local storage becomes `.foo.pdf.icloud`.
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
        // A project that happens to be called System is not the system.
        assert!(!is_hard_skipped(Path::new("/Users/x/code/System"), &[]));
        assert!(!is_hard_skipped(Path::new("/Users/x/code/Library"), &[]));
    }

    #[test]
    fn an_explicit_root_beats_the_skip_list() {
        let roots = vec![PathBuf::from("/Volumes/work")];
        assert!(!is_hard_skipped(Path::new("/Volumes/work"), &roots));
        // ...but only that exact directory, not everything named alike below it.
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
