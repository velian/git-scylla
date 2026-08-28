//! Does a changed path mean anything to the snapshot?
//!
//! Pure and separate from the notify integration, so the classification
//! rules are unit tested without a real filesystem.

use std::path::{Component, Path, PathBuf};

/// What a changed path is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The repository may have moved. Re-probe it.
    Reprobe,
    /// Nothing the snapshot depends on.
    Ignore,
}

/// The kinds of change this crate distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Created or modified.
    Touched,
    Removed,
    /// The backend could not say. Common with FSEvents.
    Unknown,
}

/// Judge a path already known to belong to a repository.
///
/// `rel` is relative to the repository root; `bare` means that root is
/// itself the git directory.
pub fn verdict(rel: &Path, bare: bool, change: Change) -> Verdict {
    let Some(inside_git) = git_relative(rel, bare) else {
        // Worktree path. Only git can say whether it matters.
        return Verdict::Reprobe;
    };

    // A fetch or `gc` writes thousands of loose objects; none is news until
    // a ref references it, and that ref write is a separate reported event.
    if starts_with_component(inside_git, "objects") {
        return Verdict::Ignore;
    }

    if inside_git == Path::new("index.lock") {
        // `index.lock` existing means an operation is mid-flight; probing
        // then reads a torn state.
        return match change {
            Change::Removed => Verdict::Reprobe,
            // `Unknown` is treated as a create; the operation's real end is
            // reported by the ref/index/HEAD write that follows.
            Change::Touched | Change::Unknown => Verdict::Ignore,
        };
    }

    Verdict::Reprobe
}

/// The part of `rel` inside the git directory, if it is inside one.
///
/// `None` means the worktree. A bare repository's root is the git
/// directory, so everything is inside it.
fn git_relative(rel: &Path, bare: bool) -> Option<&Path> {
    if bare {
        return Some(rel);
    }
    rel.strip_prefix(".git").ok()
}

/// Is `first` the first component of `path`?
///
/// Component-wise, so `objects-cache/x` is not inside `objects/`.
fn starts_with_component(path: &Path, first: &str) -> bool {
    matches!(path.components().next(), Some(Component::Normal(c)) if c == first)
}

/// Does this path look like a repository appearing where none was known?
///
/// Only `.git` counts; a bare repository's appearance is not detected here.
pub fn repository_appearing(path: &Path) -> Option<PathBuf> {
    // Owned rather than borrowed: a `Path` slice cannot be taken at an
    // arbitrary component boundary.
    let mut parent = PathBuf::new();
    for comp in path.components() {
        if comp.as_os_str() == ".git" {
            return (!parent.as_os_str().is_empty()).then_some(parent);
        }
        parent.push(comp);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(rel: &str, change: Change) -> Verdict {
        verdict(Path::new(rel), false, change)
    }

    #[test]
    fn worktree_changes_are_news() {
        assert_eq!(v("src/main.rs", Change::Touched), Verdict::Reprobe);
        assert_eq!(v("README.md", Change::Removed), Verdict::Reprobe);
        // Build output is untracked content; the probe reports how much.
        assert_eq!(v("target/debug/thing", Change::Touched), Verdict::Reprobe);
    }

    #[test]
    fn the_fields_of_a_snapshot_are_news() {
        for path in [
            ".git/HEAD",
            ".git/index",
            ".git/FETCH_HEAD",
            ".git/MERGE_HEAD",
            ".git/packed-refs",
            ".git/refs/heads/main",
            ".git/refs/remotes/origin/main",
            ".git/rebase-merge/done",
        ] {
            assert_eq!(v(path, Change::Touched), Verdict::Reprobe, "{path}");
        }
    }

    #[test]
    fn loose_objects_are_not_news() {
        for path in [".git/objects/ab/cdef", ".git/objects/pack/pack-1.pack", ".git/objects"] {
            assert_eq!(v(path, Change::Touched), Verdict::Ignore, "{path}");
        }
    }

    #[test]
    fn a_directory_merely_named_like_objects_is_still_news() {
        // Component-wise, so a worktree file cannot be silenced by its name.
        assert_eq!(v(".git/objects-backup/x", Change::Touched), Verdict::Reprobe);
        assert_eq!(v("objects/x", Change::Touched), Verdict::Reprobe);
    }

    #[test]
    fn an_index_lock_is_interesting_only_when_it_goes() {
        assert_eq!(v(".git/index.lock", Change::Touched), Verdict::Ignore);
        assert_eq!(v(".git/index.lock", Change::Unknown), Verdict::Ignore);
        assert_eq!(v(".git/index.lock", Change::Removed), Verdict::Reprobe);
    }

    #[test]
    fn a_bare_repository_has_its_git_directory_at_the_root() {
        let bare = |rel: &str| verdict(Path::new(rel), true, Change::Touched);
        assert_eq!(bare("objects/ab/cdef"), Verdict::Ignore);
        assert_eq!(bare("refs/heads/main"), Verdict::Reprobe);
        assert_eq!(bare("HEAD"), Verdict::Reprobe);
        // The same path in a normal repository is worktree content.
        assert_eq!(v("objects/ab/cdef", Change::Touched), Verdict::Reprobe);
    }

    #[test]
    fn a_git_directory_in_a_path_names_the_repository_above_it() {
        let appearing =
            |p: &str| repository_appearing(Path::new(p)).map(|q| q.display().to_string());
        assert_eq!(appearing("/work/new/.git"), Some("/work/new".into()));
        assert_eq!(appearing("/work/new/.git/HEAD"), Some("/work/new".into()));
        assert_eq!(appearing("/work/a/b/.git/refs/heads/main"), Some("/work/a/b".into()));
    }

    #[test]
    fn a_path_with_no_git_component_is_not_a_repository_appearing() {
        assert_eq!(repository_appearing(Path::new("/work/notes.txt")), None);
        assert_eq!(repository_appearing(Path::new("/work/gitignore")), None);
        assert_eq!(repository_appearing(Path::new("/work/x.git")), None);
        // A `.git` with nothing above it names no repository.
        assert_eq!(repository_appearing(Path::new(".git")), None);
    }
}
