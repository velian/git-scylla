//! Does this changed path mean anything to the snapshot?
//!
//! The whole point of the watcher is to notice that a repository moved. The
//! whole *cost* of the watcher is noticing things that are not that. A fetch
//! writes thousands of loose object files and none of them is news; a build
//! writes a directory the snapshot already counts as one untracked entry.
//!
//! Pure, and separate from the notify integration, because this is the part
//! that has to be right and the part that would otherwise only be exercisable
//! by producing real filesystem events on a real volume.

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
///
/// Narrower than `notify::EventKind` on purpose: only one rule anywhere depends
/// on which kind it is (`index.lock`), so carrying the full taxonomy through
/// would be inviting rules that depend on distinctions the backends do not
/// report consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Created or modified.
    Touched,
    Removed,
    /// The backend would not say. FSEvents coalesces, so this is common.
    Unknown,
}

/// Judge a path already known to belong to a repository.
///
/// `rel` is relative to the repository root. `bare` says whether that root
/// *is* the git directory, which moves every path below one level up.
pub fn verdict(rel: &Path, bare: bool, change: Change) -> Verdict {
    let Some(inside_git) = git_relative(rel, bare) else {
        // Somewhere in the worktree. Anything there can change what
        // `git status` reports, and nothing cheap distinguishes a source file
        // from a build artefact — `.gitignore` is git's to interpret, and the
        // probe is what asks it.
        return Verdict::Reprobe;
    };

    // A fetch or a `gc` writes thousands of these and not one is news: an
    // object is only reachable once a ref points at it, and that ref write is a
    // separate event this does report. Without this rule a single fetch is a
    // probe storm, which is the failure the watcher is most likely to cause.
    if starts_with_component(inside_git, "objects") {
        return Verdict::Ignore;
    }

    // The user's own git holds `index.lock` for the duration of an operation.
    // Its *creation* says something is about to happen and the repository is
    // mid-flight; probing then races the operation and reads a torn state. Its
    // *removal* is the interesting half — that is the operation finishing.
    if inside_git == Path::new("index.lock") {
        return match change {
            Change::Removed => Verdict::Reprobe,
            // `Unknown` counts as the create: FSEvents coalesces a create and a
            // remove into one event, and the removal will be reported again by
            // whatever the operation touched on its way out — a ref, the index,
            // `HEAD`. Guessing "finished" here would probe mid-operation.
            Change::Touched | Change::Unknown => Verdict::Ignore,
        };
    }

    // Everything else in the git directory: `HEAD`, `refs/**`, `packed-refs`,
    // `index`, `FETCH_HEAD`, `MERGE_HEAD`, `rebase-merge/**`. Every one of them
    // is a field of the snapshot.
    Verdict::Reprobe
}

/// The part of `rel` inside the git directory, if it is inside one.
///
/// `None` means the worktree. For a bare repository the root *is* the git
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
/// Only `.git` itself counts. A watcher that treated every unattributable path
/// as a possible repository would run a discovery pass every time somebody
/// saved a note in a root directory.
///
/// A **bare** repository appearing is not detected: it has no `.git`, and the
/// only way to recognise one is to look for `HEAD` + `objects/` + `refs/`,
/// which is a classification the walker already owns and which no single event
/// can answer. Cloning a bare repository still needs a Refresh.
pub fn repository_appearing(path: &Path) -> Option<PathBuf> {
    // The same walk `discovery::owner_of` does over a gitdir path: accumulate
    // components until `.git`, and what came before it is the repository.
    // Owned rather than borrowed because a borrowed slice of a `Path` cannot be
    // taken on a component boundary without going through the encoded bytes,
    // and a path this crate handles is never large enough for that to matter.
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
        // A build directory is untracked content, and how much of it is
        // untracked is exactly what the probe reports. `.gitignore` is git's to
        // interpret, not ours.
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
        // One fetch writes thousands of these. An object nobody references
        // changes nothing a snapshot reports, and the ref write that makes it
        // reachable is a separate event this does report.
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
        // Its creation says an operation is starting; probing then races it and
        // reads a torn state. Its removal is the operation finishing.
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
        // ...and the same path in a normal repository is worktree content.
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
        // Otherwise every saved note in a root directory runs a discovery pass.
        assert_eq!(repository_appearing(Path::new("/work/notes.txt")), None);
        assert_eq!(repository_appearing(Path::new("/work/gitignore")), None);
        assert_eq!(repository_appearing(Path::new("/work/x.git")), None);
        // A `.git` with nothing above it names no repository.
        assert_eq!(repository_appearing(Path::new(".git")), None);
    }
}
