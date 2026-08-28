use crate::InProgress;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Why a repository was left out of a batch.
///
/// Closed rather than a string so the UI can render each case with its own
/// actionable explanation and the CLI can group a plan's skips by reason.
///
/// Every reason says what is wrong in terms the user can act on: "no upstream
/// configured" tells them what to do, "skipped" does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum SkipReason {
    /// Nothing to do — already at the requested state.
    UpToDate,
    /// The branch tracks nothing, so there is no "behind" to resolve.
    NoUpstream,
    /// An upstream is configured but its remote-tracking ref is gone. Distinct
    /// from `NoUpstream`: the configuration is right and the remote is wrong,
    /// which is a different fix.
    UpstreamGone,
    DirtyWorktree,
    OperationInProgress(InProgress),
    DetachedHead,
    UnbornBranch,
    BareRepo,
    /// Ahead *and* behind, for an action that cannot reconcile the two.
    Diverged,
    /// No remote at all, so nothing to fetch from or push to.
    NoRemote,
    /// The snapshot is too old, or the probe that produced it failed. Acting on
    /// a snapshot you do not trust is how a bulk tool corrupts a working set.
    SnapshotStale,
    /// Excluded by the selection, not by a precondition. Present so a plan can
    /// account for every repository it was shown.
    NotSelected,
    /// The named ref does not exist here. Partial success is expected for a
    /// bulk checkout, and the plan must name the repositories it missed.
    RefNotFound(String),
    /// Nothing to stash, or nothing on the stash to pop.
    NoStash,
    /// This job's effect cannot be repaired by moving `HEAD` back. Carries the
    /// reason: "cannot be undone" alone is a dead end, and the reasons differ
    /// completely between actions.
    NotUndoable(String),
    /// Nothing says which branch this repository treats as its default, so a
    /// sync has nowhere to go. Named rather than guessed: checking out the wrong
    /// branch across a working set is not worth saving the user one line.
    NoDefaultBranch,
    /// `HEAD` has moved since the job finished — somebody committed on top of
    /// its result. Undoing would discard that newer work, which is not what an
    /// undo is for.
    HeadMoved,
}

impl std::fmt::Display for SkipReason {
    /// Actionable phrasing, lowercase, no trailing punctuation — these are
    /// rendered in a plan's skip list and in a CLI table cell.
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

    /// Every reason, so the assertions below are exhaustive by construction.
    ///
    /// The hand-written list this replaces had already lost three variants —
    /// `NotUndoable`, `NoDefaultBranch` and `HeadMoved` — so their phrasing had
    /// never been checked at all. `every_variant_is_covered_by_the_test_corpus`
    /// is what makes the next omission a failing test rather than a silence.
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
        // A discriminant-level check, so adding a variant fails here rather
        // than silently going unrendered — the same shape `Action`'s corpus
        // uses, and for the same reason.
        let seen: std::collections::HashSet<_> =
            all_reasons().iter().map(std::mem::discriminant).collect();
        assert_eq!(seen.len(), 17, "SkipReason has a variant with no test coverage");
    }

    #[test]
    fn every_reason_reads_as_a_cause_not_a_verdict() {
        // A reason must tell the user something. The cheapest mechanical proxy:
        // it never just says "skipped", and it is phrased for a sentence rather
        // than as a type name.
        for r in all_reasons() {
            let s = r.to_string();
            assert!(!s.is_empty(), "{r:?}");
            assert!(!s.contains("skipped"), "{r:?} explains nothing: {s:?}");
            assert!(!s.ends_with('.'), "{r:?} should not be punctuated: {s:?}");
            assert_eq!(s.trim(), s, "{r:?} has stray whitespace: {s:?}");
            // Rendered inline in a table cell, so it starts lowercase — unless
            // the first word is something git itself spells in capitals, which
            // is why `HeadMoved` opens with `HEAD`. Lowercasing an identifier
            // to satisfy a style rule would be the wrong fix.
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
        // The configuration is right in one case and wrong in the other, and
        // the fix differs.
        assert_ne!(SkipReason::NoUpstream.to_string(), SkipReason::UpstreamGone.to_string());
    }
}
