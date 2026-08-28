//! Which repository does a changed path belong to?
//!
//! Asked once per changed path, so it has to be cheap, and it is a **prefix**
//! question — which is why this is a sorted `Vec` and a binary search rather
//! than the `HashMap` that every other lookup in the project uses. A hash map
//! can answer "is this path a repository"; it cannot answer "is any ancestor of
//! this path a repository", and the second is the only question a watcher has.

use git_scylla_core::RepoId;
use std::path::{Path, PathBuf};

/// One repository the watcher can attribute a path to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watched {
    pub id: RepoId,
    /// The worktree root, or the repository itself when bare.
    pub path: PathBuf,
    /// A bare repository **is** its git directory, so the paths that matter sit
    /// at its root rather than under `.git/`. Carried because classification
    /// needs it and re-deriving it would mean a `stat` per event.
    pub bare: bool,
}

/// Path → repository, by longest prefix.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Sorted by path. Rebuilt whenever a scan settles rather than mutated,
    /// because the set it mirrors is rebuilt then too and an incrementally
    /// maintained copy is a second source of truth.
    entries: Vec<Watched>,
}

impl Index {
    pub fn new(repos: impl IntoIterator<Item = Watched>) -> Self {
        let mut entries: Vec<Watched> = repos.into_iter().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        Self { entries }
    }

    /// The repository owning `path`: the **longest** entry that is an ancestor
    /// of it, or the entry itself.
    ///
    /// Longest and not merely any, because repositories nest — a submodule, a
    /// linked worktree, or a checkout inside a vendor directory. Attributing a
    /// submodule's file to its superproject would re-probe the wrong row and
    /// leave the right one stale.
    pub fn owner(&self, path: &Path) -> Option<&Watched> {
        // Entries that are ancestors of `path` all sort at or before it, and
        // among them the longer ones sort later — an ancestor is a prefix of
        // its descendants, and a prefix sorts first. So the first ancestor
        // found scanning backwards is the longest one.
        let at = self.entries.partition_point(|e| e.path.as_path() <= path);
        self.entries[..at].iter().rev().find(|e| path.starts_with(&e.path))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every repository at or below `dir`.
    ///
    /// For a subtree that has gone: one `rm -rf` of a directory holding four
    /// checkouts is one event, and all four are gone.
    pub fn under<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = &'a Watched> {
        self.entries.iter().filter(move |e| e.path.starts_with(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(path: &str) -> Watched {
        Watched { id: RepoId::from_canonical(path), path: path.into(), bare: false }
    }

    fn index(paths: &[&str]) -> Index {
        Index::new(paths.iter().map(|p| watched(p)))
    }

    fn owner(index: &Index, path: &str) -> Option<String> {
        index.owner(Path::new(path)).map(|w| w.path.display().to_string())
    }

    #[test]
    fn a_path_belongs_to_the_repository_containing_it() {
        let index = index(&["/work/api", "/work/web"]);
        assert_eq!(owner(&index, "/work/api/src/main.rs").as_deref(), Some("/work/api"));
        assert_eq!(owner(&index, "/work/web/.git/HEAD").as_deref(), Some("/work/web"));
    }

    #[test]
    fn the_repository_root_itself_belongs_to_it() {
        let index = index(&["/work/api"]);
        assert_eq!(owner(&index, "/work/api").as_deref(), Some("/work/api"));
    }

    #[test]
    fn the_longest_prefix_wins() {
        // Repositories nest: a submodule, a linked worktree, a checkout in a
        // vendor directory. Attributing a submodule's file to its superproject
        // re-probes the wrong row and leaves the right one stale.
        let index = index(&["/work/super", "/work/super/vendor/inner"]);
        assert_eq!(owner(&index, "/work/super/a.txt").as_deref(), Some("/work/super"));
        assert_eq!(
            owner(&index, "/work/super/vendor/inner/a.txt").as_deref(),
            Some("/work/super/vendor/inner")
        );
    }

    #[test]
    fn a_sibling_sharing_a_string_prefix_is_not_an_ancestor() {
        // `/work/api-old` starts with the *string* `/work/api` but is not
        // inside it. This is why the test is `Path::starts_with` and not
        // `str::starts_with`.
        let index = index(&["/work/api"]);
        assert_eq!(owner(&index, "/work/api-old/src/main.rs"), None);
    }

    #[test]
    fn a_path_under_no_repository_has_no_owner() {
        let index = index(&["/work/api"]);
        assert_eq!(owner(&index, "/work/notes.txt"), None);
        assert_eq!(owner(&index, "/elsewhere/thing"), None);
        assert_eq!(owner(&Index::default(), "/work/api/a.txt"), None);
    }

    #[test]
    fn entries_that_sort_between_a_path_and_its_owner_are_stepped_over() {
        // `-` (0x2D) sorts before `/` (0x2F), so `/work/api-old` lands between
        // `/work/api` and `/work/api/src`. The backwards scan has to pass it.
        let index = index(&["/work/api", "/work/api-old", "/work/api-older"]);
        assert_eq!(owner(&index, "/work/api/src/main.rs").as_deref(), Some("/work/api"));
    }

    #[test]
    fn the_same_repository_twice_is_one_entry() {
        let index = Index::new([watched("/work/api"), watched("/work/api")]);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn everything_under_a_vanished_directory_is_reported() {
        // One `rm -rf` of a directory holding four checkouts is one event.
        let index = index(&["/work/a", "/work/b", "/other/c"]);
        let gone: Vec<_> =
            index.under(Path::new("/work")).map(|w| w.path.display().to_string()).collect();
        assert_eq!(gone, ["/work/a", "/work/b"]);
    }
}
