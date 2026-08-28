use crate::{FetchHealth, FetchSchedule, FetchStatus, Oid, RepoId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Everything the tool knows about one repository at one instant.
///
/// A struct of orthogonal facts, not a state enum. A repository can be
/// untracked-dirty, staged, three ahead and seven behind at once; the
/// single-value summary is [`crate::Badge`], derived for display only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub id: RepoId,
    pub path: PathBuf,
    pub kind: RepoKind,
    pub head: Head,
    /// The commit `HEAD` resolves to.
    ///
    /// [`Head`] says *what* HEAD is; this says *where* it points, which for a
    /// branch the head itself does not carry. `None` for an unborn branch.
    ///
    /// Kept for undo: deciding whether somebody has committed on top of a job's
    /// result means comparing where HEAD is now against where the job left it.
    pub head_oid: Option<Oid>,
    pub upstream: Option<Upstream>,
    /// Fetchable targets. `Remote::host` is the per-host concurrency bucket key
    /// for automatic fetching.
    pub remotes: Vec<Remote>,
    pub work: WorkTree,
    pub op: Option<InProgress>,
    pub stashes: u32,
    /// Auto-fetch bookkeeping. Engine-maintained, never probed.
    pub fetch: FetchHealth,
    #[serde(with = "crate::serde_time")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub probed_at: SystemTime,
    pub outcome: ProbeOutcome,
    /// Restored from a previous run's cache and not yet re-read.
    ///
    /// Age cannot stand in for this. A cache written five seconds ago still
    /// describes a repository nothing watched while the application was closed,
    /// and "the user committed in a terminal, then relaunched" is exactly the
    /// case an age test lets through.
    #[serde(default)]
    pub from_cache: bool,
    /// A watcher is covering this repository.
    ///
    /// Never persisted: watching is a fact about the running process, and a
    /// cache claiming coverage would let a relaunch act on facts nothing had
    /// checked since the last run.
    ///
    /// It exists for [`Self::is_stale`]. Age alone means a repository nobody
    /// touches goes stale by sitting still, so thirty seconds after launch every
    /// action on a current row is refused with "refresh first".
    #[serde(default, skip_serializing)]
    pub watched: bool,
}

impl RepoSnapshot {
    /// No staged, modified, untracked or conflicted paths.
    ///
    /// A bare repository has no worktree, so it is vacuously clean. Any caller
    /// reading this as "safe to mutate" must also consult [`Self::op`].
    pub fn is_clean(&self) -> bool {
        self.work.is_clean()
    }

    /// Did the probe that produced this snapshot succeed?
    pub fn is_trustworthy(&self) -> bool {
        matches!(self.outcome, ProbeOutcome::Ok)
    }

    /// Is this older than `max_age`, or the product of a probe that failed?
    ///
    /// Derived from `probed_at` rather than stored as a flag, which is what
    /// makes the startup cache safe: a restored snapshot carries the previous
    /// run's timestamp, so it is stale by construction and the `SnapshotStale`
    /// precondition refuses it without anything remembering to set a bit.
    ///
    /// A snapshot from the future is treated as fresh: a clock moved, and
    /// refusing every action on the machine until it settles is the worse of
    /// the two failures.
    pub fn is_stale(&self, now: SystemTime, max_age: std::time::Duration) -> bool {
        if self.from_cache || !self.is_trustworthy() {
            return true;
        }
        // A watched repository is current whatever its age: the watcher would
        // have said if it had changed, which is a positive guarantee where a
        // timestamp is only the absence of one.
        if self.watched {
            return false;
        }
        now.duration_since(self.probed_at).is_ok_and(|age| age > max_age)
    }

    pub fn branch(&self) -> Option<&str> {
        match &self.head {
            Head::Branch(b) | Head::Unborn(b) => Some(b),
            Head::Detached(_) => None,
        }
    }

    /// The compact status column, e.g. `↑3 ↓7 ●2 +1 ?4`.
    ///
    /// Two distinctions are load-bearing, and both are the ones tools like this
    /// usually get wrong:
    ///
    /// * no upstream renders as `-`, never as `↑0 ↓0`
    /// * a deleted remote-tracking ref renders as `↑? ↓?`, never as in-sync
    ///
    /// In `core` rather than in a surface because the CLI's STATUS column and
    /// the grid's are the same column. They were two implementations that
    /// happened to agree, and the second had already lost the case where a bare
    /// repository carries an upstream.
    pub fn status_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.kind.has_worktree() {
            parts.push("bare".to_string());
        }
        match &self.upstream {
            // "-" says "no upstream configured", which is not a fact about a
            // repository with no branch checked out.
            None if self.kind.has_worktree() => parts.push("-".to_string()),
            None => {}
            Some(up) => match &up.sync {
                None => parts.push("\u{2191}? \u{2193}?".to_string()),
                Some(ab) => {
                    if ab.ahead > 0 {
                        parts.push(format!("\u{2191}{}", ab.ahead));
                    }
                    if ab.behind > 0 {
                        parts.push(format!("\u{2193}{}", ab.behind));
                    }
                }
            },
        }
        if self.work.modified > 0 {
            parts.push(format!("\u{25cf}{}", self.work.modified));
        }
        if self.work.staged > 0 {
            parts.push(format!("+{}", self.work.staged));
        }
        if self.work.untracked > 0 {
            parts.push(format!("?{}", self.work.untracked));
        }
        if self.work.conflicted > 0 {
            parts.push(format!("\u{00d7}{}", self.work.conflicted));
        }
        if self.stashes > 0 {
            parts.push(format!("\u{2691}{}", self.stashes));
        }
        if let Some(op) = self.op {
            parts.push(format!("[{op}]"));
        }
        if matches!(self.outcome, ProbeOutcome::Timeout) {
            parts.push("[timeout]".to_string());
        }
        parts.join(" ")
    }

    /// Fetch **health**, not a fetch timestamp.
    ///
    /// With fetching automatic, an old timestamp is not a caveat on the
    /// `behind` count — it is a fault, and the column has to name which. The
    /// phrasing is left to each surface, because a table cell and a cell with a
    /// tooltip behind it have different room; the *decision* is here so they
    /// cannot disagree about what they are describing.
    pub fn fetch_status(&self) -> FetchStatus {
        match &self.fetch.schedule {
            FetchSchedule::Disabled => {
                if self.remotes.is_empty() {
                    FetchStatus::NoRemote
                } else {
                    FetchStatus::Off
                }
            }
            FetchSchedule::Quarantined { last_error, .. } => {
                FetchStatus::Quarantined { reason: last_error.clone() }
            }
            FetchSchedule::BackingOff { until, failures } => {
                FetchStatus::BackingOff { until: *until, failures: *failures }
            }
            // The newest fetch by anyone, and only then this tool's own last
            // success. The grid consulted both and the CLI only the first, so a
            // repository the scheduler had fetched but whose `FETCH_HEAD` was
            // missing read as "never" in one surface and as fetched in the
            // other.
            FetchSchedule::Due(_) => {
                match self.upstream.as_ref().and_then(|u| u.last_fetch).or(self.fetch.last_success)
                {
                    Some(at) => FetchStatus::Fetched { at },
                    None => FetchStatus::Never,
                }
            }
        }
    }

    /// A clean repository at `path`, on `main`, tracking nothing.
    ///
    /// For tests. Seven modules across three crates each wrote these fourteen
    /// fields out by hand, so every field added to `RepoSnapshot` meant editing
    /// seven fixtures with no opinion about it — and each of those fixtures
    /// silently decided things its tests were not about. Override what the test
    /// *is* about and leave the rest.
    ///
    /// `probed_at` is the epoch: a fixture must not be accidentally fresh, and
    /// anything that cares about age passes its own `now`.
    #[cfg(any(test, feature = "testkit"))]
    pub fn stub(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            id: RepoId::from_canonical(path.clone()),
            path,
            kind: RepoKind::Normal,
            head: Head::Branch("main".into()),
            head_oid: None,
            upstream: None,
            remotes: Vec::new(),
            work: WorkTree::default(),
            op: None,
            stashes: 0,
            fetch: FetchHealth::disabled(),
            probed_at: SystemTime::UNIX_EPOCH,
            outcome: ProbeOutcome::Ok,
            from_cache: false,
            watched: false,
        }
    }

    /// A snapshot for a repository we could not probe.
    ///
    /// Never a silently empty "clean" snapshot: the outcome carries the
    /// failure, [`Self::badge`] returns `Unknown`, and [`Self::is_trustworthy`]
    /// is false.
    ///
    /// `head` gets a placeholder, because an "unknown" variant would put a case
    /// in every match arm for a value only this constructor produces. Callers
    /// must gate on `is_trustworthy()` before reading `head`.
    pub fn failed(id: RepoId, kind: RepoKind, at: SystemTime, outcome: ProbeOutcome) -> Self {
        Self {
            path: id.path().to_path_buf(),
            id,
            kind,
            head: Head::Detached(Oid::parse("0000000").expect("static oid")),
            head_oid: None,
            upstream: None,
            remotes: Vec::new(),
            work: WorkTree::default(),
            op: None,
            stashes: 0,
            fetch: FetchHealth::disabled(),
            probed_at: at,
            outcome,
            from_cache: false,
            watched: false,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Adjacent tagging (`{"type": ..., "value": ...}`) rather than internal, which
/// cannot represent a newtype variant wrapping a string. One uniform shape
/// across every enum is worth more to the TypeScript generator than a flatter
/// one for the struct variants. Enums whose variants are all unit-valued keep
/// serde's default plain-string form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum RepoKind {
    Normal,
    /// No worktree, so working-tree state is meaningless rather than clean.
    Bare,
    /// `.git` is a file pointing into the main repository's `worktrees/`.
    Worktree {
        main: RepoId,
    },
    /// `.git` is a file pointing into the parent's `modules/`.
    Submodule {
        parent: RepoId,
    },
}

impl RepoKind {
    /// Does this kind have a working tree whose state is meaningful?
    pub fn has_worktree(&self) -> bool {
        !matches!(self, RepoKind::Bare)
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Head {
    Branch(String),
    Detached(Oid),
    /// A fresh `git init`: `HEAD` names a branch that does not exist yet. The
    /// string is that branch name, which is real information even though no
    /// commit carries it.
    Unborn(String),
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Where the current branch stands against its configured upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// The remote's name — `"origin"`. What a targeted fetch fetches.
    pub remote: String,
    /// The short tracking ref — `"origin/main"`.
    pub remote_ref: String,
    /// `None` when the upstream is configured but its remote-tracking ref does
    /// not exist. Deliberately not `ahead: 0, behind: 0`: "the branch it tracks
    /// was deleted" and "in sync" are opposite facts, and an `Option` makes
    /// reading one as the other a compile error rather than a quiet
    /// reassurance.
    pub sync: Option<AheadBehind>,
    /// Mtime of `FETCH_HEAD`: somebody fetched, including the user in their own
    /// terminal. Distinct from [`FetchHealth::last_success`], which only this
    /// tool's scheduler writes.
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_fetch: Option<SystemTime>,
}

impl Upstream {
    /// The upstream is configured but its remote-tracking ref is gone.
    pub fn is_gone(&self) -> bool {
        self.sync.is_none()
    }

    /// Commits on this branch and not upstream — "committed but not pushed".
    /// `None` when the tracking ref is gone.
    pub fn ahead(&self) -> Option<u32> {
        self.sync.as_ref().map(|s| s.ahead)
    }

    /// Commits upstream and not here, **as of the last fetch**.
    pub fn behind(&self) -> Option<u32> {
        self.sync.as_ref().map(|s| s.behind)
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

impl AheadBehind {
    pub fn diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// `"origin"`.
    pub name: String,
    /// Host parsed from the configured URL. `None` for a path remote or an
    /// unparseable URL — this is a concurrency bucket key and never a
    /// correctness input, so a miss costs throughput and nothing else.
    pub host: Option<String>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Path counts, by which side of the index they differ on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTree {
    /// Index vs `HEAD`.
    pub staged: u32,
    /// Worktree vs index.
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
}

impl WorkTree {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.modified == 0 && self.untracked == 0 && self.conflicted == 0
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// A multi-step git operation the user left half-finished.
///
/// Every one of these makes most mutating actions unsafe, which is why it is a
/// first-class fact rather than something inferred from a dirty worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl std::fmt::Display for InProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
            InProgress::Bisect => "bisect",
        })
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum ProbeOutcome {
    Ok,
    /// The probe hit its deadline. One slow volume must not stall the grid, so
    /// this is an ordinary outcome rather than an error.
    Timeout,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FetchSchedule;
    use std::time::Duration;

    fn snap() -> RepoSnapshot {
        RepoSnapshot::stub("/r/a")
    }

    fn tracked(sync: Option<AheadBehind>) -> RepoSnapshot {
        let mut s = snap();
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync,
            last_fetch: None,
        });
        s
    }

    #[test]
    fn no_upstream_is_not_rendered_as_in_sync() {
        assert_eq!(snap().status_line(), "-");
        assert_eq!(tracked(Some(AheadBehind { ahead: 0, behind: 0 })).status_line(), "");
    }

    #[test]
    fn a_gone_tracking_ref_is_rendered_as_unknown() {
        assert_eq!(tracked(None).status_line(), "\u{2191}? \u{2193}?");
    }

    #[test]
    fn counts_render_in_legend_order() {
        let mut s = tracked(Some(AheadBehind { ahead: 3, behind: 7 }));
        s.work = WorkTree { staged: 1, modified: 2, untracked: 4, conflicted: 1 };
        s.stashes = 2;
        assert_eq!(s.status_line(), "\u{2191}3 \u{2193}7 \u{25cf}2 +1 ?4 \u{00d7}1 \u{2691}2");
    }

    #[test]
    fn bare_repositories_do_not_claim_a_missing_upstream() {
        let mut s = snap();
        s.kind = RepoKind::Bare;
        // "-" would say "no upstream configured", which is not a fact about a
        // repository that has no branch checked out.
        assert_eq!(s.status_line(), "bare");
    }

    #[test]
    fn a_bare_repository_that_does_track_something_still_reports_it() {
        // The case the grid's own copy of this had lost: it stopped at "bare"
        // and dropped the counts entirely. Nothing produces this today —
        // `probe_bare` leaves `upstream` unset — which is exactly why a second
        // implementation could disagree here for as long as it liked.
        let mut s = tracked(Some(AheadBehind { ahead: 3, behind: 0 }));
        s.kind = RepoKind::Bare;
        assert_eq!(s.status_line(), "bare \u{2191}3");
    }

    #[test]
    fn timeout_and_in_progress_are_visible() {
        let mut s = snap();
        s.outcome = ProbeOutcome::Timeout;
        assert!(s.status_line().contains("[timeout]"));
        let mut s = snap();
        s.op = Some(InProgress::Rebase);
        assert!(s.status_line().contains("[rebase]"));
    }

    #[test]
    fn no_remote_and_opted_out_are_different_states() {
        // "off" for a repository with nothing to fetch from would be a setting
        // the user did not choose.
        assert_eq!(snap().fetch_status(), FetchStatus::NoRemote);
        let mut s = snap();
        s.remotes = vec![Remote { name: "origin".into(), host: None }];
        assert_eq!(s.fetch_status(), FetchStatus::Off);
    }

    #[test]
    fn a_healthy_schedule_reports_the_newest_fetch_by_anyone() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        let mut s = tracked(Some(AheadBehind { ahead: 0, behind: 0 }));
        s.fetch.schedule = FetchSchedule::Due(SystemTime::UNIX_EPOCH);
        assert_eq!(s.fetch_status(), FetchStatus::Never);

        // The scheduler's own success counts when `FETCH_HEAD` says nothing.
        // The CLI read only `last_fetch` and the grid read both, so one of them
        // called this repository unfetched.
        s.fetch.last_success = Some(at);
        assert_eq!(s.fetch_status(), FetchStatus::Fetched { at });

        // Anyone's fetch outranks it: the user's own `git fetch` in a terminal
        // is what the behind count is as current as.
        let newer = at + Duration::from_secs(60);
        s.upstream.as_mut().unwrap().last_fetch = Some(newer);
        assert_eq!(s.fetch_status(), FetchStatus::Fetched { at: newer });
    }

    #[test]
    fn only_the_states_a_user_can_act_on_are_problems() {
        assert!(!FetchStatus::NoRemote.is_problem());
        assert!(!FetchStatus::Off.is_problem());
        assert!(!FetchStatus::Never.is_problem());
        assert!(!FetchStatus::Fetched { at: SystemTime::UNIX_EPOCH }.is_problem());
        assert!(FetchStatus::Quarantined { reason: "boom".into() }.is_problem());
        assert!(FetchStatus::BackingOff { until: SystemTime::UNIX_EPOCH, failures: 2 }.is_problem());
    }
}
