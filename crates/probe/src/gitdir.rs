//! Facts that live in the git directory rather than in `git status` output.

use git_scylla_core::InProgress;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Resolve the *common* git directory.
///
/// A linked worktree's git dir contains a `commondir` file pointing at the main
/// repository's `.git`. The split matters, and getting it backwards produces
/// bugs that look random:
///
/// * per-worktree, in `git_dir`: `MERGE_HEAD`, `rebase-merge/`, `HEAD`
/// * shared, in the common dir: `config`, `FETCH_HEAD`, `refs/remotes/`
///
/// So a linked worktree mid-rebase must be read from its own git dir, while its
/// remotes and last-fetch time come from the main repository's.
pub fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let marker = git_dir.join("commondir");
    let Ok(contents) = std::fs::read_to_string(&marker) else {
        return git_dir.to_path_buf();
    };
    let target = contents.trim();
    if target.is_empty() {
        return git_dir.to_path_buf();
    }
    let raw = Path::new(target);
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { git_dir.join(raw) };
    joined.canonicalize().unwrap_or(joined)
}

/// Which multi-step operation, if any, is half-finished.
///
/// Marker files in the *per-worktree* git dir. Checked in a fixed order so that
/// a repository somehow carrying two markers reports deterministically; the
/// order runs most-obstructive first.
pub fn detect_in_progress(git_dir: &Path) -> Option<InProgress> {
    // `git rebase --continue` leaves rebase-merge/ (interactive and merge
    // backends) or rebase-apply/ (the am backend, and `git am` itself).
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        return Some(InProgress::Rebase);
    }
    if git_dir.join("MERGE_HEAD").exists() {
        return Some(InProgress::Merge);
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Some(InProgress::CherryPick);
    }
    if git_dir.join("REVERT_HEAD").exists() {
        return Some(InProgress::Revert);
    }
    if git_dir.join("BISECT_LOG").exists() {
        return Some(InProgress::Bisect);
    }
    None
}

/// When anything last fetched into this repository.
///
/// `FETCH_HEAD`'s mtime is the primary signal and moves for any fetch by
/// anyone, including the user in their own terminal. The fallback matters for a
/// repository cloned but never fetched since: `refs/remotes/` exists and carries
/// the clone time, where `FETCH_HEAD` may not exist at all.
pub fn last_fetch(common_dir: &Path) -> Option<SystemTime> {
    if let Ok(md) = std::fs::metadata(common_dir.join("FETCH_HEAD")) {
        if let Ok(t) = md.modified() {
            return Some(t);
        }
    }
    // Newest mtime among the per-remote directories, not the parent's: the
    // parent's mtime only moves when a remote is added or removed.
    let entries = std::fs::read_dir(common_dir.join("refs/remotes")).ok()?;
    entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()?.modified().ok()).max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_each_operation() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        assert_eq!(detect_in_progress(g), None);

        fs::write(g.join("MERGE_HEAD"), "x").unwrap();
        assert_eq!(detect_in_progress(g), Some(InProgress::Merge));
        fs::remove_file(g.join("MERGE_HEAD")).unwrap();

        for (name, want) in [
            ("CHERRY_PICK_HEAD", InProgress::CherryPick),
            ("REVERT_HEAD", InProgress::Revert),
            ("BISECT_LOG", InProgress::Bisect),
        ] {
            fs::write(g.join(name), "x").unwrap();
            assert_eq!(detect_in_progress(g), Some(want));
            fs::remove_file(g.join(name)).unwrap();
        }

        for dir in ["rebase-merge", "rebase-apply"] {
            fs::create_dir(g.join(dir)).unwrap();
            assert_eq!(detect_in_progress(g), Some(InProgress::Rebase));
            fs::remove_dir(g.join(dir)).unwrap();
        }
    }

    #[test]
    fn rebase_wins_over_a_stray_merge_head() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("rebase-merge")).unwrap();
        fs::write(tmp.path().join("MERGE_HEAD"), "x").unwrap();
        assert_eq!(detect_in_progress(tmp.path()), Some(InProgress::Rebase));
    }

    #[test]
    fn common_dir_follows_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main_git = root.join("main/.git");
        let wt_git = main_git.join("worktrees/wt");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        assert_eq!(resolve_common_dir(&wt_git), main_git);
    }

    #[test]
    fn common_dir_of_a_plain_repo_is_itself() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_common_dir(tmp.path()), tmp.path());
    }
}
