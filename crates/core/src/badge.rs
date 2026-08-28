use crate::{ProbeOutcome, RepoSnapshot};
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// A single value summarising a snapshot, for display and sorting only.
///
/// Derived, never stored; declaration order is the sort priority, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Badge {
    Conflict,
    /// A merge, rebase, cherry-pick, revert or bisect left half-finished.
    InProgress,
    Diverged,
    Behind,
    Ahead,
    Dirty,
    Staged,
    Clean,
    /// The probe failed or timed out, or the upstream's tracking ref is gone.
    /// Never rendered as clean.
    Unknown,
}

impl std::fmt::Display for Badge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Badge::Conflict => "conflict",
            Badge::InProgress => "in-progress",
            Badge::Diverged => "diverged",
            Badge::Behind => "behind",
            Badge::Ahead => "ahead",
            Badge::Dirty => "dirty",
            Badge::Staged => "staged",
            Badge::Clean => "clean",
            Badge::Unknown => "unknown",
        })
    }
}

impl RepoSnapshot {
    /// Collapse the facts into one badge, worst-first.
    ///
    /// Precedence is the declaration order of [`Badge`], and nothing else
    /// breaks ties.
    pub fn badge(&self) -> Badge {
        if !matches!(self.outcome, ProbeOutcome::Ok) {
            return Badge::Unknown;
        }
        if self.work.conflicted > 0 {
            return Badge::Conflict;
        }
        if self.op.is_some() {
            return Badge::InProgress;
        }
        if let Some(up) = &self.upstream {
            match &up.sync {
                None => return Badge::Unknown,
                Some(ab) if ab.diverged() => return Badge::Diverged,
                Some(ab) if ab.behind > 0 => return Badge::Behind,
                Some(ab) if ab.ahead > 0 => return Badge::Ahead,
                Some(_) => {}
            }
        }
        if self.work.modified > 0 || self.work.untracked > 0 {
            return Badge::Dirty;
        }
        if self.work.staged > 0 {
            return Badge::Staged;
        }
        Badge::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AheadBehind, InProgress, Upstream};

    fn snap() -> RepoSnapshot {
        RepoSnapshot::stub("/r")
    }

    fn with_sync(ahead: u32, behind: u32) -> RepoSnapshot {
        let mut s = snap();
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead, behind }),
            last_fetch: None,
        });
        s
    }

    #[test]
    fn clean_is_clean() {
        assert_eq!(snap().badge(), Badge::Clean);
        assert_eq!(with_sync(0, 0).badge(), Badge::Clean);
    }

    #[test]
    fn upstream_position_beats_worktree_state() {
        let mut s = with_sync(3, 0);
        s.work.untracked = 4;
        assert_eq!(s.badge(), Badge::Ahead);

        let mut s = with_sync(0, 7);
        s.work.modified = 2;
        assert_eq!(s.badge(), Badge::Behind);

        let mut s = with_sync(3, 7);
        s.work.staged = 1;
        assert_eq!(s.badge(), Badge::Diverged);
    }

    #[test]
    fn conflict_and_in_progress_beat_everything() {
        let mut s = with_sync(3, 7);
        s.work.conflicted = 1;
        assert_eq!(s.badge(), Badge::Conflict);

        let mut s = with_sync(3, 7);
        s.op = Some(InProgress::Rebase);
        assert_eq!(s.badge(), Badge::InProgress);
    }

    #[test]
    fn an_operation_in_progress_is_not_called_a_conflict() {
        for op in [InProgress::Bisect, InProgress::Merge, InProgress::CherryPick] {
            let mut s = snap();
            s.op = Some(op);
            assert_eq!(s.badge(), Badge::InProgress, "{op}");
        }
        let mut s = snap();
        s.op = Some(InProgress::Merge);
        s.work.conflicted = 2;
        assert_eq!(s.badge(), Badge::Conflict);
    }

    #[test]
    fn dirty_beats_staged() {
        let mut s = snap();
        s.work.staged = 1;
        assert_eq!(s.badge(), Badge::Staged);
        s.work.modified = 1;
        assert_eq!(s.badge(), Badge::Dirty, "modified and staged together reads as dirty");
        s.work.modified = 0;
        s.work.untracked = 1;
        assert_eq!(s.badge(), Badge::Dirty, "untracked and staged together reads as dirty");
    }

    #[test]
    fn a_failed_probe_is_never_clean() {
        for outcome in [ProbeOutcome::Timeout, ProbeOutcome::Error("boom".into())] {
            let mut s = snap();
            s.outcome = outcome;
            assert_eq!(s.badge(), Badge::Unknown);
        }
    }

    #[test]
    fn a_deleted_tracking_ref_is_unknown_not_clean() {
        let mut s = snap();
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: None,
            last_fetch: None,
        });
        assert_eq!(s.badge(), Badge::Unknown);
    }

    #[test]
    fn no_upstream_is_not_in_sync() {
        assert_eq!(snap().badge(), Badge::Clean);
        assert!(snap().upstream.is_none());
        assert_eq!(with_sync(0, 0).badge(), Badge::Clean);
        assert!(with_sync(0, 0).upstream.is_some());
    }

    #[test]
    fn badge_order_is_the_sort_priority() {
        let mut all = vec![
            Badge::Clean,
            Badge::Unknown,
            Badge::Conflict,
            Badge::Ahead,
            Badge::Diverged,
            Badge::InProgress,
            Badge::Staged,
            Badge::Behind,
            Badge::Dirty,
        ];
        all.sort();
        assert_eq!(
            all,
            vec![
                Badge::Conflict,
                Badge::InProgress,
                Badge::Diverged,
                Badge::Behind,
                Badge::Ahead,
                Badge::Dirty,
                Badge::Staged,
                Badge::Clean,
                Badge::Unknown,
            ]
        );
    }
}
