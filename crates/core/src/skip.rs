use crate::InProgress;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Why a repository was left out of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum SkipReason {
    UpToDate,
    NoUpstream,
    /// An upstream is configured but its remote-tracking ref is gone.
    UpstreamGone,
    DirtyWorktree,
    OperationInProgress(InProgress),
    DetachedHead,
    UnbornBranch,
    BareRepo,
    Diverged,
    NoRemote,
    SnapshotStale,
    NotSelected,
    RefNotFound(String),
    NoStash,
    NotUndoable(String),
    NoDefaultBranch,
    HeadMoved,
}

impl std::fmt::Display for SkipReason {
    /// Actionable phrasing: lowercase, no trailing punctuation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::UpToDate => f.write_str("already up to date"),
            SkipReason::NoUpstream => f.write_str("no upstream configured"),
            SkipReason::UpstreamGone => {
                f.write_str("upstream configured but its remote branch is gone")
            }
            SkipReason::DirtyWorktree => f.write_str("uncommitted changes"),
            SkipReason::OperationInProgress(op) => write!(f, "{op} in progress"),
            SkipReason::DetachedHead => f.write_str("detached HEAD"),
            SkipReason::UnbornBranch => f.write_str("no commits yet"),
            SkipReason::BareRepo => f.write_str("bare repository"),
            SkipReason::Diverged => f.write_str("diverged from upstream"),
            SkipReason::NoRemote => f.write_str("no remote configured"),
            SkipReason::SnapshotStale => f.write_str("status is out of date; refresh first"),
            SkipReason::NotSelected => f.write_str("not selected"),
            SkipReason::RefNotFound(r) => write!(f, "no such ref: {r}"),
            SkipReason::NoStash => f.write_str("nothing stashed"),
            SkipReason::NotUndoable(why) => write!(f, "cannot be undone: {why}"),
            SkipReason::NoDefaultBranch => {
                f.write_str("no default branch: no origin/HEAD, and no main or master")
            }
            SkipReason::HeadMoved => {
                f.write_str("HEAD moved after the job; undoing would discard newer work")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reason, once each.
    fn all_reasons() -> Vec<SkipReason> {
        vec![
            SkipReason::UpToDate,
            SkipReason::NoUpstream,
            SkipReason::UpstreamGone,
            SkipReason::DirtyWorktree,
            SkipReason::OperationInProgress(InProgress::Rebase),
            SkipReason::DetachedHead,
            SkipReason::UnbornBranch,
            SkipReason::BareRepo,
            SkipReason::Diverged,
            SkipReason::NoRemote,
            SkipReason::SnapshotStale,
            SkipReason::NotSelected,
            SkipReason::RefNotFound("refs/heads/nope".into()),
            SkipReason::NoStash,
            SkipReason::NotUndoable("a sync cannot be reversed".into()),
            SkipReason::NoDefaultBranch,
            SkipReason::HeadMoved,
        ]
    }

    #[test]
    fn every_variant_is_covered_by_the_test_corpus() {
        let seen: std::collections::HashSet<_> =
            all_reasons().iter().map(std::mem::discriminant).collect();
        assert_eq!(seen.len(), 17, "SkipReason has a variant with no test coverage");
    }

    #[test]
    fn every_reason_reads_as_a_cause_not_a_verdict() {
        for r in all_reasons() {
            let s = r.to_string();
            assert!(!s.is_empty(), "{r:?}");
            assert!(!s.contains("skipped"), "{r:?} explains nothing: {s:?}");
            assert!(!s.ends_with('.'), "{r:?} should not be punctuated: {s:?}");
            assert_eq!(s.trim(), s, "{r:?} has stray whitespace: {s:?}");
            // Allows an uppercase first word too, since `HeadMoved` opens with `HEAD`.
            let first = s.split_whitespace().next().unwrap();
            assert!(
                first.chars().next().unwrap().is_lowercase() || first == first.to_uppercase(),
                "{r:?} should start lowercase or with an identifier: {s:?}"
            );
        }
    }

    #[test]
    fn in_progress_reasons_name_the_operation() {
        assert_eq!(
            SkipReason::OperationInProgress(InProgress::CherryPick).to_string(),
            "cherry-pick in progress"
        );
    }

    #[test]
    fn no_upstream_and_upstream_gone_read_differently() {
        assert_ne!(SkipReason::NoUpstream.to_string(), SkipReason::UpstreamGone.to_string());
    }
}
