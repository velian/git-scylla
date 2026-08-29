//! Facts that live in the git directory rather than in `git status` output.

use git_scylla_core::InProgress;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub fn resolve_common_dir(per_worktree_dir: &Path) -> PathBuf {
    let marker = per_worktree_dir.join("commondir");
    let Ok(contents) = std::fs::read_to_string(&marker) else {
        return per_worktree_dir.to_path_buf();
    };
    let target = contents.trim();
    if target.is_empty() {
        return per_worktree_dir.to_path_buf();
    }
    let raw = Path::new(target);
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { per_worktree_dir.join(raw) };
    joined.canonicalize().unwrap_or(joined)
}

pub fn detect_in_progress(per_worktree_dir: &Path) -> Option<InProgress> {
    let d = per_worktree_dir;
    if d.join("rebase-merge").is_dir() || d.join("rebase-apply").is_dir() {
        return Some(InProgress::Rebase);
    }
    if d.join("MERGE_HEAD").exists() {
        return Some(InProgress::Merge);
    }
    if d.join("CHERRY_PICK_HEAD").exists() {
        return Some(InProgress::CherryPick);
    }
    if d.join("REVERT_HEAD").exists() {
        return Some(InProgress::Revert);
    }
    if d.join("BISECT_LOG").exists() {
        return Some(InProgress::Bisect);
    }
    None
}

pub fn last_fetch(common_dir: &Path) -> Option<SystemTime> {
    if let Ok(md) = std::fs::metadata(common_dir.join("FETCH_HEAD")) {
        if let Ok(t) = md.modified() {
            return Some(t);
        }
    }
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
