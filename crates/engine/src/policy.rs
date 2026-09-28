//! Preconditions: is this action safe to run against this repository?
//!
//! Pure functions over a snapshot. No I/O, no clock of its own — `now` is an
//! argument — so the whole safety surface is exhaustively unit-testable.
//!
//! Two rules govern the design:
//!
//! * **Default to refusing on any doubt.** A skip the user overrides is
//!   cheaper than a mutation they did not expect. Every skip carries an
//!   actionable reason.
//! * **Do not over-restrict `Fetch`.** It cannot touch a worktree or local
//!   history, and the background scheduler consults this same function.

use git_scylla_core::{
    Action, FetchHealth, FetchSchedule, Head, Network, Outage, PullMode, RepoId, RepoKind,
    RepoSnapshot, SkipReason, SyncPlan,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eligibility {
    Eligible,
    Skip(SkipReason),
}

impl Eligibility {
    pub fn is_eligible(&self) -> bool {
        matches!(self, Eligibility::Eligible)
    }

    pub fn skip_reason(&self) -> Option<&SkipReason> {
        match self {
            Eligibility::Eligible => None,
            Eligibility::Skip(r) => Some(r),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(with = "git_scylla_core::serde_duration")]
    pub max_snapshot_age: Duration,
    #[serde(with = "git_scylla_core::serde_duration")]
    pub max_lease_age: Duration,
    pub fetch_bare: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_snapshot_age: Duration::from_secs(30),
            max_lease_age: Duration::from_secs(600),
            fetch_bare: true,
        }
    }
}

pub fn evaluate(
    action: &Action,
    snap: &RepoSnapshot,
    now: SystemTime,
    policy: &Policy,
) -> Eligibility {
    if !matches!(action, Action::Fetch { .. }) {
        if let Some(skip) = untrustworthy(snap, now, policy) {
            return Eligibility::Skip(skip);
        }
    }

    match action {
        Action::Fetch { .. } => fetch(snap, policy),
        Action::Pull { mode } => pull(snap, *mode),
        Action::Push { set_upstream, force_with_lease } => {
            push(snap, set_upstream.as_deref(), *force_with_lease, now, policy)
        }
        Action::Checkout { create, .. } => checkout(snap, *create),
        Action::Commit { stage_all, .. } => commit(snap, *stage_all),
        Action::Stash { include_untracked } => stash(snap, *include_untracked),
        Action::StashPop => stash_pop(snap),
        Action::Branch { .. } => branch(snap),
        Action::Reset { to, .. } => reset(snap, to),
        Action::SyncDefault { plan, .. } => sync_default(snap, plan.as_ref()),
        Action::DevTag { push, .. } => dev_tag(snap, push.is_some()),
        Action::Custom { .. } => custom(snap),
    }
}

fn untrustworthy(snap: &RepoSnapshot, now: SystemTime, policy: &Policy) -> Option<SkipReason> {
    snap.is_stale(now, policy.max_snapshot_age).then_some(SkipReason::SnapshotStale)
}

fn worktree_blockers(snap: &RepoSnapshot) -> Option<SkipReason> {
    if !snap.kind.has_worktree() {
        return Some(SkipReason::BareRepo);
    }
    if let Some(op) = snap.op {
        return Some(SkipReason::OperationInProgress(op));
    }
    None
}

fn branch_blockers(snap: &RepoSnapshot) -> Option<SkipReason> {
    match snap.head {
        Head::Branch(_) => None,
        Head::Detached(_) => Some(SkipReason::DetachedHead),
        Head::Unborn(_) => Some(SkipReason::UnbornBranch),
    }
}

fn fetch(snap: &RepoSnapshot, policy: &Policy) -> Eligibility {
    if !snap.is_trustworthy() || snap.from_cache {
        return Eligibility::Skip(SkipReason::SnapshotStale);
    }
    if matches!(snap.kind, RepoKind::Bare) && !policy.fetch_bare {
        return Eligibility::Skip(SkipReason::BareRepo);
    }
    if snap.remotes.is_empty() {
        return Eligibility::Skip(SkipReason::NoRemote);
    }
    Eligibility::Eligible
}

fn pull(snap: &RepoSnapshot, mode: PullMode) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap).or_else(|| branch_blockers(snap)) {
        return Eligibility::Skip(skip);
    }
    let Some(upstream) = &snap.upstream else {
        return Eligibility::Skip(SkipReason::NoUpstream);
    };
    let Some(sync) = upstream.sync else {
        return Eligibility::Skip(SkipReason::UpstreamGone);
    };
    if sync.behind == 0 {
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    if mode == PullMode::FfOnly && sync.ahead > 0 {
        return Eligibility::Skip(SkipReason::Diverged);
    }
    if !snap.is_clean() {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    Eligibility::Eligible
}

fn push(
    snap: &RepoSnapshot,
    set_upstream: Option<&str>,
    force_with_lease: bool,
    now: SystemTime,
    policy: &Policy,
) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap).or_else(|| branch_blockers(snap)) {
        return Eligibility::Skip(skip);
    }
    match (&snap.upstream, set_upstream) {
        (None, None) => return Eligibility::Skip(SkipReason::NoUpstream),
        (None, Some(_)) if snap.remotes.is_empty() => {
            return Eligibility::Skip(SkipReason::NoRemote)
        }
        _ => {}
    }
    match snap.upstream.as_ref().and_then(|u| u.sync) {
        None if set_upstream.is_some() => {}
        None => return Eligibility::Skip(SkipReason::UpstreamGone),
        Some(sync) => {
            if sync.ahead == 0 {
                return Eligibility::Skip(SkipReason::UpToDate);
            }
            if sync.behind > 0 && !force_with_lease {
                return Eligibility::Skip(SkipReason::Diverged);
            }
        }
    }
    if force_with_lease && !lease_is_fresh(snap, now, policy) {
        return Eligibility::Skip(SkipReason::SnapshotStale);
    }
    Eligibility::Eligible
}

fn lease_is_fresh(snap: &RepoSnapshot, now: SystemTime, policy: &Policy) -> bool {
    let newest = [snap.upstream.as_ref().and_then(|u| u.last_fetch), snap.fetch.last_success]
        .into_iter()
        .flatten()
        .max();
    match newest {
        Some(t) => now.duration_since(t).map(|age| age <= policy.max_lease_age).unwrap_or(true),
        None => false,
    }
}

fn checkout(snap: &RepoSnapshot, create: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if !create && matches!(snap.head, Head::Unborn(_)) {
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    if !snap.is_clean() {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    Eligibility::Eligible
}

fn commit(snap: &RepoSnapshot, stage_all: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    let staged = snap.work.staged > 0;
    let stageable = stage_all && (snap.work.modified > 0 || snap.work.untracked > 0);
    if !staged && !stageable {
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    Eligibility::Eligible
}

fn stash(snap: &RepoSnapshot, include_untracked: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if matches!(snap.head, Head::Unborn(_)) {
        // `git stash` needs a commit to record against.
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    let has_tracked_changes = snap.work.staged > 0 || snap.work.modified > 0;
    let has_untracked = include_untracked && snap.work.untracked > 0;
    if !has_tracked_changes && !has_untracked {
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    Eligibility::Eligible
}

fn stash_pop(snap: &RepoSnapshot) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if snap.stashes == 0 {
        return Eligibility::Skip(SkipReason::NoStash);
    }
    if snap.work.conflicted > 0 {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    Eligibility::Eligible
}

fn dev_tag(snap: &RepoSnapshot, push: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if matches!(snap.head, Head::Unborn(_)) {
        // Nothing to tag. `git tag` on an unborn branch fails.
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    if push && snap.remotes.is_empty() {
        return Eligibility::Skip(SkipReason::NoRemote);
    }
    Eligibility::Eligible
}

fn sync_default(snap: &RepoSnapshot, plan: Option<&SyncPlan>) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap).or_else(|| branch_blockers(snap)) {
        return Eligibility::Skip(skip);
    }
    if snap.remotes.is_empty() {
        return Eligibility::Skip(SkipReason::NoRemote);
    }
    if let Some(plan) = plan {
        if plan.default == plan.back_to && !snap.is_clean() {
            return Eligibility::Skip(SkipReason::DirtyWorktree);
        }
    }
    Eligibility::Eligible
}
fn branch(snap: &RepoSnapshot) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if matches!(snap.head, Head::Unborn(_)) {
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    Eligibility::Eligible
}

fn reset(snap: &RepoSnapshot, to: &git_scylla_core::Oid) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if !snap.is_clean() {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    if snap.head_oid.as_ref() == Some(to) {
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    Eligibility::Eligible
}

fn custom(snap: &RepoSnapshot) -> Eligibility {
    if let Some(op) = snap.op {
        return Eligibility::Skip(SkipReason::OperationInProgress(op));
    }
    Eligibility::Eligible
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchPolicy {
    #[serde(with = "git_scylla_core::serde_duration")]
    pub interval: Duration,
    pub jitter_pct: u8,
    #[serde(with = "backoff_serde")]
    pub backoff: [Duration; 4],
    pub quarantine_after: u32,
    /// Consecutive fetches that failed because the machine could not get out,
    /// with nothing succeeding in between, before automatic fetching is held.
    ///
    /// Two rather than one: a single remote whose host does not resolve looks
    /// the same as no network, and in a working set of any size it is
    /// interrupted by everything that still works.
    pub offline_after: u32,
    /// How often, while held, one repository is asked whether the machine is
    /// back.
    #[serde(with = "git_scylla_core::serde_duration")]
    pub recheck: Duration,
    pub enabled: bool,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15 * 60),
            jitter_pct: 20,
            backoff: [
                Duration::from_secs(60),
                Duration::from_secs(5 * 60),
                Duration::from_secs(30 * 60),
                Duration::from_secs(2 * 60 * 60),
            ],
            quarantine_after: 5,
            offline_after: 2,
            recheck: Duration::from_secs(60),
            enabled: true,
        }
    }
}

impl FetchPolicy {
    pub fn backoff_for(&self, failures: u32) -> Duration {
        self.backoff[(failures.saturating_sub(1) as usize).min(self.backoff.len() - 1)]
    }
    pub fn next_due(&self, id: &RepoId, after: SystemTime) -> SystemTime {
        after
            + self.interval.saturating_sub(self.span())
            + jitter(id, self.interval, self.jitter_pct)
    }

    pub fn span(&self) -> Duration {
        self.interval.mul_f64(f64::from(self.jitter_pct.min(100)) / 100.0)
    }
}
pub fn jitter(id: &RepoId, interval: Duration, pct: u8) -> Duration {
    let span = interval.mul_f64(f64::from(pct.min(100)) / 100.0);
    let window = (span.as_millis() as u64).saturating_mul(2);
    if window == 0 {
        return Duration::ZERO;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.path().as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Duration::from_millis(hash % window)
}

pub fn due(
    now: SystemTime,
    snaps: &[RepoSnapshot],
    fetch: &FetchPolicy,
    policy: &Policy,
) -> Vec<RepoId> {
    if !fetch.enabled {
        return Vec::new();
    }
    const ACTION: Action = Action::Fetch { prune: true, tags: false };
    snaps
        .iter()
        .filter(|snap| ready(now, &snap.fetch.schedule))
        // The same `evaluate` a user fetch goes through, not a second copy.
        .filter(|snap| evaluate(&ACTION, snap, now, policy).is_eligible())
        .map(|snap| snap.id.clone())
        .collect()
}

fn ready(now: SystemTime, schedule: &FetchSchedule) -> bool {
    match schedule {
        FetchSchedule::Due(at) | FetchSchedule::BackingOff { until: at, .. } => now >= *at,
        // Never automatically. Only the user asking restarts it.
        FetchSchedule::Quarantined { .. } => false,
        FetchSchedule::Disabled => false,
    }
}

pub fn after_attempt(
    health: &FetchHealth,
    id: &RepoId,
    now: SystemTime,
    outcome: Attempt<'_>,
    fetch: &FetchPolicy,
) -> FetchHealth {
    let failures = match &health.schedule {
        FetchSchedule::BackingOff { failures, .. } => *failures,
        _ => 0,
    };
    match outcome {
        Attempt::Ok => FetchHealth {
            last_attempt: Some(now),
            last_success: Some(now),
            schedule: FetchSchedule::Due(fetch.next_due(id, now)),
        },
        Attempt::Failed(error) => {
            let failures = failures + 1;
            let schedule = if failures >= fetch.quarantine_after {
                FetchSchedule::Quarantined {
                    since: now,
                    // Verbatim: "quarantined" without the error is a dead end.
                    last_error: error.to_string(),
                }
            } else {
                FetchSchedule::BackingOff { until: now + fetch.backoff_for(failures), failures }
            };
            FetchHealth { last_attempt: Some(now), last_success: health.last_success, schedule }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt<'a> {
    Ok,
    Failed(&'a str),
}

pub fn manual_attempt(
    health: &FetchHealth,
    id: &RepoId,
    now: SystemTime,
    outcome: Attempt<'_>,
    fetch: &FetchPolicy,
) -> FetchHealth {
    let cleared = FetchHealth {
        last_attempt: health.last_attempt,
        last_success: health.last_success,
        schedule: FetchSchedule::Due(now),
    };
    after_attempt(&cleared, id, now, outcome, fetch)
}

/// The verdict on the machine's own network.
///
/// Two independent facts, and the verdict is down while either holds:
///
/// * **No route off the machine** ([`crate::route`]) — proved locally, in a
///   fifth of a millisecond, and unproved the same way the moment a route is
///   back. Nothing has to be run to find out.
/// * **Nothing reachable** — learned from fetches that already ran, which is
///   the only evidence that can catch a network with a route and no service:
///   dead DNS, a captive portal, a VPN half up. Only a fetch that reaches
///   something can clear it, because only a fetch could have found it.
///
/// Each fact is set and cleared only by its own evidence, which is what stops
/// a returning route from declaring a captive portal solved.
///
/// Only fetches from a remote *host* are evidence. A path remote succeeds with
/// the wifi off, and letting it vote would lift a verdict it knows nothing
/// about.
#[derive(Debug, Clone, Default)]
pub struct Reachability {
    no_route_since: Option<SystemTime>,
    unreachable: Option<Held>,
    /// Hosted fetches that looked like no network, since the last that did not.
    misses: u32,
}

/// The "nothing reachable" fact, while it holds.
#[derive(Debug, Clone, Copy)]
struct Held {
    since: SystemTime,
    /// When a repository was last asked whether the machine is back. Starts at
    /// `since`: the fetches that established the verdict were the first ask.
    asked: SystemTime,
}

/// One finished fetch from a remote host, as the verdict sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    /// It worked, or it failed in a way only a reachable remote can fail.
    Yes,
    /// It failed saying the machine could not get out. See
    /// [`git_scylla_core::looks_offline`].
    No,
}

impl Reachability {
    pub fn verdict(&self) -> Network {
        match (self.no_route_since, self.unreachable) {
            (None, None) => Network::Up,
            // No route is the stronger statement: nothing is tried at all, so
            // there is no recheck to tell anyone about.
            (Some(t), held) => {
                Network::Down { since: held.map_or(t, |h| h.since.min(t)), cause: Outage::NoRoute }
            }
            (None, Some(h)) => Network::Down { since: h.since, cause: Outage::Unreachable },
        }
    }

    pub fn is_down(&self) -> bool {
        self.verdict().is_down()
    }

    /// Whether the routing table had a way off this machine when last asked.
    pub fn routable(&self) -> bool {
        self.no_route_since.is_none()
    }

    /// What the routing table says now. Conclusive both ways for its own
    /// fact, and silent on the other.
    pub fn saw_route(&mut self, now: SystemTime, exists: bool) {
        self.no_route_since = if exists { None } else { self.no_route_since.or(Some(now)) };
    }

    /// Fold in one finished fetch from a remote host.
    pub fn saw_fetch(&mut self, now: SystemTime, reached: Reached, fetch: &FetchPolicy) {
        match reached {
            Reached::Yes => {
                self.misses = 0;
                self.unreachable = None;
            }
            // A missing route already explains it, and counting it would
            // leave a verdict behind that outlives the route coming back.
            Reached::No if self.no_route_since.is_some() => {}
            Reached::No => {
                self.misses = self.misses.saturating_add(1);
                if self.misses >= fetch.offline_after.max(1) && self.unreachable.is_none() {
                    self.unreachable = Some(Held { since: now, asked: now });
                }
            }
        }
    }

    /// Which of the due repositories may start a background fetch now.
    ///
    /// A repository whose remotes are all paths always may: it never needed a
    /// network. Of the rest, all may when the verdict is up and none may with
    /// no route. With a route and nothing reachable, one may per recheck
    /// interval — holding every one would be a stop rather than a pause,
    /// since nothing else tells the app that DNS came back.
    pub fn admit(
        &mut self,
        now: SystemTime,
        due: &[&RepoSnapshot],
        fetch: &FetchPolicy,
    ) -> Vec<RepoId> {
        let recheck = match (self.no_route_since, &mut self.unreachable) {
            (None, None) => return due.iter().map(|s| s.id.clone()).collect(),
            (Some(_), _) => None,
            (None, Some(held)) => {
                let waited = now.duration_since(held.asked).unwrap_or_default() >= fetch.recheck;
                let pick = if waited { longest_unasked(due) } else { None };
                if pick.is_some() {
                    held.asked = now;
                }
                pick
            }
        };
        due.iter()
            .filter(|s| !s.has_remote_host() || Some(&s.id) == recheck.as_ref())
            .map(|s| s.id.clone())
            .collect()
    }
}

/// Which repository answers the recheck: the hosted one asked longest ago.
///
/// It rotates rather than picking a favourite because the question is about
/// the *machine*, and any repository can answer it — while a repository whose
/// own remote is broken would answer "no" for ever and hold the rest behind
/// it. A repository never yet attempted goes first: `None` sorts first.
fn longest_unasked(due: &[&RepoSnapshot]) -> Option<RepoId> {
    due.iter()
        .filter(|s| s.has_remote_host())
        .min_by_key(|s| s.fetch.last_attempt)
        .map(|s| s.id.clone())
}

/// A fetch that failed while the machine itself could not get out.
///
/// Nothing was asked of the repository, so nothing is concluded about it: the
/// schedule and the failure count are left exactly as they were, and only the
/// attempt is recorded. Quarantine is for a repository that keeps refusing —
/// spending its budget on a closed laptop lid would silence it for reasons it
/// had no part in, and leave the user to restart each one by hand.
pub fn offline_attempt(health: &FetchHealth, now: SystemTime) -> FetchHealth {
    FetchHealth { last_attempt: Some(now), ..health.clone() }
}

mod backoff_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(v: &[Duration; 4], s: S) -> Result<S::Ok, S::Error> {
        v.map(|d| d.as_millis() as u64).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[Duration; 4], D::Error> {
        Ok(<[u64; 4]>::deserialize(d)?.map(Duration::from_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_scylla_core::{InProgress, ProbeOutcome};

    const ALL_IN_PROGRESS: &[InProgress] = &[
        InProgress::Merge,
        InProgress::Rebase,
        InProgress::CherryPick,
        InProgress::Revert,
        InProgress::Bisect,
    ];
    use git_scylla_core::{AheadBehind, FetchHealth, Oid, Remote, Upstream, WorkTree};

    const T0: SystemTime = SystemTime::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        T0 + Duration::from_secs(secs)
    }

    fn snap() -> RepoSnapshot {
        let mut s = RepoSnapshot::stub("/r");
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead: 0, behind: 0 }),
            last_fetch: Some(T0),
        });
        s.remotes = vec![Remote { name: "origin".into(), host: None, web: None }];
        s.fetch = FetchHealth::due_now(T0);
        s
    }

    fn sync(ahead: u32, behind: u32) -> RepoSnapshot {
        let mut s = snap();
        s.upstream.as_mut().unwrap().sync = Some(AheadBehind { ahead, behind });
        s
    }

    fn ev(action: &Action, s: &RepoSnapshot) -> Eligibility {
        evaluate(action, s, T0, &Policy::default())
    }

    fn skip(action: &Action, s: &RepoSnapshot) -> Option<SkipReason> {
        ev(action, s).skip_reason().cloned()
    }

    const FETCH: Action = Action::Fetch { prune: true, tags: false };
    const FF: Action = Action::Pull { mode: PullMode::FfOnly };
    const REBASE: Action = Action::Pull { mode: PullMode::Rebase };
    const MERGE: Action = Action::Pull { mode: PullMode::Merge };

    fn all_actions() -> Vec<Action> {
        vec![
            FETCH,
            FF,
            REBASE,
            MERGE,
            Action::Push { set_upstream: None, force_with_lease: false },
            Action::Checkout { rev: "main".into(), create: false },
            Action::Commit { message: "m".into(), stage_all: false, no_verify: false },
            Action::Stash { include_untracked: true },
            Action::StashPop,
            Action::Custom { args: vec!["gc".into()], network: true, mutating: true },
        ]
    }

    #[test]
    fn an_untrusted_snapshot_blocks_every_action_including_fetch() {
        for outcome in [ProbeOutcome::Timeout, ProbeOutcome::Error("boom".into())] {
            let mut s = snap();
            s.outcome = outcome.clone();
            for action in all_actions() {
                assert_eq!(
                    skip(&action, &s),
                    Some(SkipReason::SnapshotStale),
                    "{action:?} ran against a {outcome:?} snapshot"
                );
            }
        }
    }

    #[test]
    fn a_snapshot_older_than_the_bound_blocks_every_action_but_fetch() {
        let s = snap(); // probed at T0
        let policy = Policy { max_snapshot_age: Duration::from_secs(30), ..Default::default() };
        for action in all_actions().into_iter().filter(|a| !matches!(a, Action::Fetch { .. })) {
            assert_ne!(
                evaluate(&action, &s, at(30), &policy).skip_reason(),
                Some(&SkipReason::SnapshotStale),
                "{action:?} was stale exactly at the bound"
            );
            assert_eq!(
                evaluate(&action, &s, at(31), &policy).skip_reason(),
                Some(&SkipReason::SnapshotStale),
                "{action:?} past the bound"
            );
        }
    }

    #[test]
    fn age_alone_never_blocks_a_fetch() {
        let s = snap();
        assert!(evaluate(&FETCH, &s, at(86_400), &Policy::default()).is_eligible());
    }

    #[test]
    fn a_row_restored_from_the_cache_is_not_fetched_either() {
        let mut s = snap();
        s.from_cache = true;
        assert_eq!(skip(&FETCH, &s), Some(SkipReason::SnapshotStale));
    }

    #[test]
    fn a_snapshot_from_the_future_is_treated_as_fresh() {
        let mut s = snap();
        s.probed_at = at(10_000);
        assert!(ev(&FETCH, &s).is_eligible());
    }

    #[test]
    fn fetch_is_not_blocked_by_anything_about_the_worktree() {
        let cases: Vec<(&str, RepoSnapshot)> = vec![
            ("dirty", {
                let mut s = snap();
                s.work = WorkTree { staged: 1, modified: 2, untracked: 3, conflicted: 1 };
                s
            }),
            ("detached", {
                let mut s = snap();
                s.head = Head::Detached(Oid::parse("deadbeef").unwrap());
                s
            }),
            ("unborn", {
                let mut s = snap();
                s.head = Head::Unborn("main".into());
                s.upstream = None;
                s
            }),
            ("no upstream", {
                let mut s = snap();
                s.upstream = None;
                s
            }),
            ("upstream gone", {
                let mut s = snap();
                s.upstream.as_mut().unwrap().sync = None;
                s
            }),
            ("diverged", sync(3, 7)),
        ];
        for (name, s) in cases {
            assert!(ev(&FETCH, &s).is_eligible(), "fetch was blocked by: {name}");
        }
        for op in ALL_IN_PROGRESS {
            let mut s = snap();
            s.op = Some(*op);
            assert!(ev(&FETCH, &s).is_eligible(), "fetch was blocked mid-{op}");
        }
    }

    #[test]
    fn fetch_needs_somewhere_to_fetch_from() {
        let mut s = snap();
        s.remotes.clear();
        assert_eq!(skip(&FETCH, &s), Some(SkipReason::NoRemote));
    }

    #[test]
    fn bare_repositories_are_fetchable_unless_configured_out() {
        let mut s = snap();
        s.kind = RepoKind::Bare;
        assert!(ev(&FETCH, &s).is_eligible());
        let off = Policy { fetch_bare: false, ..Default::default() };
        assert_eq!(evaluate(&FETCH, &s, T0, &off).skip_reason(), Some(&SkipReason::BareRepo));
    }

    #[test]
    fn pull_requires_something_to_pull() {
        for mode in [FF, REBASE, MERGE] {
            assert_eq!(skip(&mode, &sync(0, 0)), Some(SkipReason::UpToDate), "{mode:?}");
            assert!(ev(&mode, &sync(0, 3)).is_eligible(), "{mode:?}");
        }
    }

    #[test]
    fn ff_only_refuses_a_branch_that_is_ahead_but_the_others_do_not() {
        assert_eq!(skip(&FF, &sync(2, 3)), Some(SkipReason::Diverged));
        assert!(ev(&REBASE, &sync(2, 3)).is_eligible());
        assert!(ev(&MERGE, &sync(2, 3)).is_eligible());
    }

    #[test]
    fn ahead_only_is_up_to_date_rather_than_diverged() {
        for mode in [FF, REBASE, MERGE] {
            assert_eq!(skip(&mode, &sync(3, 0)), Some(SkipReason::UpToDate), "{mode:?}");
        }
    }

    #[test]
    fn every_pull_mode_requires_a_clean_worktree() {
        for mode in [FF, REBASE, MERGE] {
            for (what, work) in [
                ("modified", WorkTree { modified: 1, ..Default::default() }),
                ("staged", WorkTree { staged: 1, ..Default::default() }),
                ("untracked", WorkTree { untracked: 1, ..Default::default() }),
            ] {
                let mut s = sync(0, 3);
                s.work = work;
                assert_eq!(
                    skip(&mode, &s),
                    Some(SkipReason::DirtyWorktree),
                    "{mode:?} with {what}"
                );
            }
        }
    }

    #[test]
    fn pull_distinguishes_no_upstream_from_a_deleted_one() {
        let mut none = snap();
        none.upstream = None;
        assert_eq!(skip(&FF, &none), Some(SkipReason::NoUpstream));

        let mut gone = snap();
        gone.upstream.as_mut().unwrap().sync = None;
        assert_eq!(skip(&FF, &gone), Some(SkipReason::UpstreamGone));
    }

    #[test]
    fn an_operation_in_progress_outranks_the_detached_head_it_causes() {
        let mut s = sync(0, 3);
        s.op = Some(InProgress::Rebase);
        s.head = Head::Detached(Oid::parse("deadbeef").unwrap());
        assert_eq!(skip(&FF, &s), Some(SkipReason::OperationInProgress(InProgress::Rebase)));
    }

    #[test]
    fn push_ignores_worktree_dirtiness() {
        let mut s = sync(2, 0);
        s.work = WorkTree { staged: 1, modified: 2, untracked: 3, conflicted: 0 };
        let push = Action::Push { set_upstream: None, force_with_lease: false };
        assert!(ev(&push, &s).is_eligible(), "dirtiness is irrelevant to push");
    }

    #[test]
    fn push_needs_something_to_publish() {
        let push = Action::Push { set_upstream: None, force_with_lease: false };
        assert_eq!(skip(&push, &sync(0, 0)), Some(SkipReason::UpToDate));
        assert!(ev(&push, &sync(2, 0)).is_eligible());
    }

    #[test]
    fn push_refuses_a_diverged_branch_unless_leased() {
        let plain = Action::Push { set_upstream: None, force_with_lease: false };
        let leased = Action::Push { set_upstream: None, force_with_lease: true };
        assert_eq!(skip(&plain, &sync(2, 3)), Some(SkipReason::Diverged));
        assert!(ev(&leased, &sync(2, 3)).is_eligible());
    }

    #[test]
    fn a_lease_against_a_stale_tracking_ref_is_refused() {
        let leased = Action::Push { set_upstream: None, force_with_lease: true };
        let policy = Policy { max_lease_age: Duration::from_secs(600), ..Default::default() };

        let mut s = sync(2, 3);
        s.probed_at = at(100_000);
        s.upstream.as_mut().unwrap().last_fetch = Some(at(100_000 - 500));
        s.fetch.last_success = None;
        assert!(evaluate(&leased, &s, at(100_000), &policy).is_eligible(), "fetched 500s ago");

        s.upstream.as_mut().unwrap().last_fetch = Some(at(100_000 - 5000));
        assert_eq!(
            evaluate(&leased, &s, at(100_000), &policy).skip_reason(),
            Some(&SkipReason::SnapshotStale),
            "fetched 5000s ago"
        );
        s.fetch.last_success = Some(at(100_000 - 100));
        assert!(evaluate(&leased, &s, at(100_000), &policy).is_eligible());
        s.upstream.as_mut().unwrap().last_fetch = None;
        s.fetch.last_success = None;
        assert_eq!(
            evaluate(&leased, &s, at(100_000), &policy).skip_reason(),
            Some(&SkipReason::SnapshotStale)
        );
    }

    #[test]
    fn push_can_set_an_upstream_where_none_exists() {
        let mut s = snap();
        s.upstream = None;
        let with = Action::Push { set_upstream: Some("origin".into()), force_with_lease: false };
        let without = Action::Push { set_upstream: None, force_with_lease: false };
        assert!(ev(&with, &s).is_eligible());
        assert_eq!(skip(&without, &s), Some(SkipReason::NoUpstream));

        s.remotes.clear();
        assert_eq!(skip(&with, &s), Some(SkipReason::NoRemote));
    }

    // ---- the authoring actions -------------------------------------------

    #[test]
    fn checkout_requires_a_clean_worktree() {
        let co = Action::Checkout { rev: "main".into(), create: false };
        let mut s = snap();
        s.work.modified = 1;
        assert_eq!(skip(&co, &s), Some(SkipReason::DirtyWorktree));
    }

    #[test]
    fn checkout_dash_b_works_on_an_unborn_branch_but_plain_checkout_does_not() {
        let mut s = snap();
        s.head = Head::Unborn("main".into());
        s.upstream = None;
        assert_eq!(
            skip(&Action::Checkout { rev: "x".into(), create: false }, &s),
            Some(SkipReason::UnbornBranch)
        );
        assert!(ev(&Action::Checkout { rev: "x".into(), create: true }, &s).is_eligible());
    }

    #[test]
    fn commit_needs_something_to_commit() {
        let plain = Action::Commit { message: "m".into(), stage_all: false, no_verify: false };
        let all = Action::Commit { message: "m".into(), stage_all: true, no_verify: false };

        assert_eq!(skip(&plain, &snap()), Some(SkipReason::UpToDate));

        let mut staged = snap();
        staged.work.staged = 1;
        assert!(ev(&plain, &staged).is_eligible());

        let mut modified = snap();
        modified.work.modified = 1;
        assert_eq!(skip(&plain, &modified), Some(SkipReason::UpToDate), "nothing staged");
        assert!(ev(&all, &modified).is_eligible());
    }

    #[test]
    fn stage_all_counts_untracked_files_as_something_to_commit() {
        let all = Action::Commit { message: "m".into(), stage_all: true, no_verify: false };
        let plain = Action::Commit { message: "m".into(), stage_all: false, no_verify: false };
        let mut s = snap();
        s.work.untracked = 3;
        assert!(ev(&all, &s).is_eligible());
        assert_eq!(skip(&plain, &s), Some(SkipReason::UpToDate));
    }

    #[test]
    fn commit_is_fine_on_an_unborn_branch() {
        let all = Action::Commit { message: "m".into(), stage_all: true, no_verify: false };
        let mut s = snap();
        s.head = Head::Unborn("main".into());
        s.upstream = None;
        s.work.untracked = 1;
        assert!(ev(&all, &s).is_eligible());
    }

    #[test]
    fn stash_needs_something_to_stash_and_respects_include_untracked() {
        let tracked = Action::Stash { include_untracked: false };
        let all = Action::Stash { include_untracked: true };

        assert_eq!(skip(&all, &snap()), Some(SkipReason::UpToDate));

        let mut untracked_only = snap();
        untracked_only.work.untracked = 2;
        assert_eq!(
            skip(&tracked, &untracked_only),
            Some(SkipReason::UpToDate),
            "untracked files are not stashed without --include-untracked"
        );
        assert!(ev(&all, &untracked_only).is_eligible());

        let mut modified = snap();
        modified.work.modified = 1;
        assert!(ev(&tracked, &modified).is_eligible());
    }

    #[test]
    fn stash_pop_needs_a_stash_and_no_conflicts() {
        let pop = Action::StashPop;
        assert_eq!(skip(&pop, &snap()), Some(SkipReason::NoStash));

        let mut s = snap();
        s.stashes = 2;
        assert!(ev(&pop, &s).is_eligible());

        s.work.modified = 1;
        assert!(ev(&pop, &s).is_eligible(), "popping onto modified files usually works");

        s.work.conflicted = 1;
        assert_eq!(skip(&pop, &s), Some(SkipReason::DirtyWorktree));
    }

    #[test]
    fn custom_is_restricted_only_by_the_universal_rules() {
        let custom = Action::Custom { args: vec!["gc".into()], network: true, mutating: true };
        let mut bare = snap();
        bare.kind = RepoKind::Bare;
        assert!(ev(&custom, &bare).is_eligible());

        let mut dirty = snap();
        dirty.work = WorkTree { staged: 1, modified: 1, untracked: 1, conflicted: 0 };
        assert!(ev(&custom, &dirty).is_eligible());

        let mut mid_rebase = snap();
        mid_rebase.op = Some(InProgress::Rebase);
        assert_eq!(
            skip(&custom, &mid_rebase),
            Some(SkipReason::OperationInProgress(InProgress::Rebase))
        );
    }

    #[test]
    fn a_bare_repository_admits_only_fetch_and_custom() {
        let mut s = snap();
        s.kind = RepoKind::Bare;
        for action in all_actions() {
            let eligible = ev(&action, &s).is_eligible();
            let expected = matches!(action, Action::Fetch { .. } | Action::Custom { .. });
            assert_eq!(eligible, expected, "{action:?} against a bare repository");
        }
    }

    #[test]
    fn every_operation_in_progress_blocks_every_action_except_fetch() {
        for op in ALL_IN_PROGRESS {
            let mut s = sync(0, 3);
            s.op = Some(*op);
            for action in all_actions() {
                let e = ev(&action, &s);
                if matches!(action, Action::Fetch { .. }) {
                    assert!(e.is_eligible(), "fetch blocked mid-{op}");
                } else {
                    assert_eq!(
                        e.skip_reason(),
                        Some(&SkipReason::OperationInProgress(*op)),
                        "{action:?} mid-{op}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_skip_reason_a_rule_can_produce_is_renderable() {
        // A reason with no words is a reason the user cannot act on.
        let mut produced = std::collections::BTreeSet::new();
        let variants: Vec<RepoSnapshot> = vec![
            snap(),
            sync(3, 0),
            sync(0, 3),
            sync(3, 7),
            {
                let mut s = snap();
                s.upstream = None;
                s
            },
            {
                let mut s = snap();
                s.upstream.as_mut().unwrap().sync = None;
                s
            },
            {
                let mut s = snap();
                s.kind = RepoKind::Bare;
                s
            },
            {
                let mut s = snap();
                s.head = Head::Unborn("main".into());
                s
            },
            {
                let mut s = snap();
                s.head = Head::Detached(Oid::parse("deadbeef").unwrap());
                s
            },
            {
                let mut s = sync(0, 3);
                s.work.modified = 1;
                s
            },
            {
                let mut s = snap();
                s.remotes.clear();
                s
            },
            {
                let mut s = snap();
                s.op = Some(InProgress::Merge);
                s
            },
            {
                let mut s = snap();
                s.outcome = ProbeOutcome::Timeout;
                s
            },
        ];
        for s in &variants {
            for action in all_actions() {
                if let Some(r) = skip(&action, s) {
                    produced.insert(r.to_string());
                }
            }
        }
        assert!(produced.len() >= 9, "corpus is too thin: {produced:?}");
        for text in &produced {
            assert!(!text.is_empty());
            assert!(!text.contains("skipped"), "{text:?} explains nothing");
        }
    }
}

#[cfg(test)]
mod fetch_schedule {
    use super::*;
    use git_scylla_core::{FetchHealth, FetchSchedule, ProbeOutcome, Remote, RepoId, Upstream};

    const T0: SystemTime = SystemTime::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        T0 + Duration::from_secs(secs)
    }

    fn repo(name: &str) -> RepoSnapshot {
        let mut s = RepoSnapshot::stub(format!("/work/{name}"));
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(git_scylla_core::AheadBehind { ahead: 0, behind: 0 }),
            last_fetch: Some(T0),
        });
        s.remotes =
            vec![Remote { name: "origin".into(), host: Some("example.invalid".into()), web: None }];
        s.fetch = FetchHealth::due_now(T0);
        s
    }

    fn due_at(now: u64, snaps: &[RepoSnapshot]) -> Vec<String> {
        due(at(now), snaps, &FetchPolicy::default(), &Policy::default())
            .iter()
            .map(|id| id.name().to_string())
            .collect()
    }

    #[test]
    fn a_repository_whose_slot_has_come_round_is_due() {
        assert_eq!(due_at(0, &[repo("a")]), ["a"]);
    }

    #[test]
    fn the_off_switch_stops_everything() {
        let off = FetchPolicy { enabled: false, ..Default::default() };
        assert!(due(at(0), &[repo("a")], &off, &Policy::default()).is_empty());
    }

    #[test]
    fn a_repository_with_nothing_to_fetch_from_is_never_due() {
        let mut s = repo("a");
        s.remotes.clear();
        s.fetch = FetchHealth::disabled();
        assert!(due_at(0, &[s]).is_empty());
    }

    #[test]
    fn a_repository_backing_off_waits_and_then_goes_again() {
        let mut s = repo("a");
        s.fetch.schedule = FetchSchedule::BackingOff { until: at(60), failures: 1 };
        assert!(due_at(59, std::slice::from_ref(&s)).is_empty());
        assert_eq!(due_at(60, &[s]), ["a"], "the bound is inclusive");
    }

    #[test]
    fn a_quarantined_repository_is_never_due_however_long_it_waits() {
        let mut s = repo("a");
        s.fetch.schedule = FetchSchedule::Quarantined { since: T0, last_error: "no key".into() };
        assert!(due_at(0, std::slice::from_ref(&s)).is_empty());
        assert!(due_at(86_400 * 30, &[s]).is_empty());
    }

    #[test]
    fn due_asks_the_same_precondition_a_user_fetch_asks() {
        let mut untrusted = repo("a");
        untrusted.outcome = ProbeOutcome::Error("boom".into());
        assert!(due_at(0, &[untrusted]).is_empty());
        assert_eq!(due_at(86_400, &[repo("a")]), ["a"]);
    }

    #[test]
    fn success_schedules_the_next_attempt_about_an_interval_out() {
        let p = FetchPolicy::default();
        let id = RepoId::from_canonical("/work/a");
        let next = after_attempt(&FetchHealth::due_now(T0), &id, at(1_000), Attempt::Ok, &p);
        assert_eq!(next.last_success, Some(at(1_000)));
        let FetchSchedule::Due(when) = next.schedule else { panic!("{next:?}") };
        let delta = when.duration_since(at(1_000)).unwrap();
        assert!(
            delta >= p.interval - p.span() && delta <= p.interval + p.span(),
            "{delta:?} is outside one interval either side of {:?}",
            p.interval
        );
    }

    #[test]
    fn failures_back_off_along_the_configured_ladder_and_then_quarantine() {
        let p = FetchPolicy::default();
        let id = RepoId::from_canonical("/work/a");
        let mut health = FetchHealth::due_now(T0);
        let mut waits = Vec::new();
        for attempt in 1..=4 {
            health = after_attempt(&health, &id, T0, Attempt::Failed("nope"), &p);
            match health.schedule {
                FetchSchedule::BackingOff { until, failures } => {
                    assert_eq!(failures, attempt);
                    waits.push(until.duration_since(T0).unwrap());
                }
                ref other => panic!("attempt {attempt}: {other:?}"),
            }
        }
        assert_eq!(waits, p.backoff, "1 m, 5 m, 30 m, 2 h");

        health = after_attempt(&health, &id, T0, Attempt::Failed("Permission denied"), &p);
        match health.schedule {
            FetchSchedule::Quarantined { last_error, .. } => {
                assert_eq!(last_error, "Permission denied");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(health.last_success, None);
    }

    #[test]
    fn a_success_after_failures_clears_them() {
        let p = FetchPolicy::default();
        let id = RepoId::from_canonical("/work/a");
        let failed = after_attempt(&FetchHealth::due_now(T0), &id, T0, Attempt::Failed("x"), &p);
        let ok = after_attempt(&failed, &id, at(60), Attempt::Ok, &p);
        assert!(matches!(ok.schedule, FetchSchedule::Due(_)));
    }

    #[test]
    fn the_user_asking_is_the_reset_button() {
        let p = FetchPolicy::default();
        let id = RepoId::from_canonical("/work/a");
        let quarantined = FetchHealth {
            last_attempt: Some(T0),
            last_success: None,
            schedule: FetchSchedule::Quarantined { since: T0, last_error: "no key".into() },
        };
        let after = manual_attempt(&quarantined, &id, at(10), Attempt::Failed("still no"), &p);
        assert!(
            matches!(after.schedule, FetchSchedule::BackingOff { failures: 1, .. }),
            "{after:?}"
        );
        let after = manual_attempt(&quarantined, &id, at(10), Attempt::Ok, &p);
        assert!(matches!(after.schedule, FetchSchedule::Due(_)), "{after:?}");
    }

    #[test]
    fn jitter_is_the_same_every_time_for_one_repository() {
        let p = FetchPolicy::default();
        let a = RepoId::from_canonical("/work/a");
        assert_eq!(jitter(&a, p.interval, p.jitter_pct), jitter(&a, p.interval, p.jitter_pct));
    }

    #[test]
    fn jitter_spreads_a_working_set_across_the_window() {
        let p = FetchPolicy::default();
        let mut per_second: std::collections::BTreeMap<u64, usize> = Default::default();
        for i in 0..100 {
            let id = RepoId::from_canonical(format!("/work/r{i:02}"));
            let slot = jitter(&id, p.interval, p.jitter_pct);
            assert!(slot < p.span() * 2, "{slot:?} escaped the window");
            *per_second.entry(slot.as_secs()).or_default() += 1;
        }
        let worst = per_second.values().copied().max().unwrap_or(0);
        assert!(worst <= 3, "{worst} repositories share one second — that is a herd");
        assert!(per_second.len() > 60, "only {} distinct slots", per_second.len());
    }

    #[test]
    fn no_jitter_configured_means_no_jitter() {
        assert_eq!(
            jitter(&RepoId::from_canonical("/work/a"), Duration::from_secs(900), 0),
            Duration::ZERO
        );
    }

    #[test]
    fn the_policy_round_trips_through_json() {
        let p = FetchPolicy::default();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<FetchPolicy>(&json).unwrap(), p, "{json}");
    }
}

#[cfg(test)]
mod reachability {
    use super::*;
    use git_scylla_core::{FetchHealth, FetchSchedule, Remote, Upstream};

    const T0: SystemTime = SystemTime::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        T0 + Duration::from_secs(secs)
    }

    fn repo(name: &str, last_attempt: Option<SystemTime>) -> RepoSnapshot {
        let mut s = RepoSnapshot::stub(format!("/work/{name}"));
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: None,
            last_fetch: None,
        });
        s.remotes =
            vec![Remote { name: "origin".into(), host: Some("example.invalid".into()), web: None }];
        s.fetch =
            FetchHealth { last_attempt, last_success: None, schedule: FetchSchedule::Due(T0) };
        s
    }

    /// A repository whose only remote is a path on this disk.
    fn on_disk(name: &str, last_attempt: Option<SystemTime>) -> RepoSnapshot {
        let mut s = repo(name, last_attempt);
        s.remotes[0].host = None;
        s
    }

    fn down(since: SystemTime, cause: Outage) -> Network {
        Network::Down { since, cause }
    }

    /// Two hosted fetches that could not get out: enough, by default, to put
    /// the verdict down as `Unreachable` at `t`.
    fn unreachable_at(t: SystemTime) -> Reachability {
        let p = FetchPolicy::default();
        let mut net = Reachability::default();
        net.saw_fetch(t, Reached::No, &p);
        net.saw_fetch(t, Reached::No, &p);
        assert_eq!(net.verdict(), down(t, Outage::Unreachable));
        net
    }

    fn names(ids: &[RepoId]) -> Vec<&str> {
        ids.iter().map(|id| id.name()).collect()
    }

    #[test]
    fn one_unreachable_fetch_is_not_yet_a_verdict_about_the_machine() {
        // A single remote whose host does not resolve looks exactly like a
        // closed lid, and is far more common.
        let mut net = Reachability::default();
        net.saw_fetch(at(0), Reached::No, &FetchPolicy::default());
        assert_eq!(net.verdict(), Network::Up);
    }

    #[test]
    fn nothing_reachable_twice_over_holds_fetching() {
        let p = FetchPolicy::default();
        let mut net = Reachability::default();
        net.saw_fetch(at(0), Reached::No, &p);
        net.saw_fetch(at(10), Reached::No, &p);
        assert_eq!(net.verdict(), down(at(10), Outage::Unreachable));
        // Still down, and still down *since* the same moment.
        net.saw_fetch(at(20), Reached::No, &p);
        assert_eq!(net.verdict(), down(at(10), Outage::Unreachable));
    }

    #[test]
    fn reaching_anything_at_all_releases_it() {
        let mut net = unreachable_at(at(0));
        net.saw_fetch(at(20), Reached::Yes, &FetchPolicy::default());
        assert_eq!(net.verdict(), Network::Up);
    }

    #[test]
    fn a_success_in_between_means_the_run_never_happened() {
        let p = FetchPolicy::default();
        let mut net = Reachability::default();
        net.saw_fetch(at(0), Reached::No, &p);
        net.saw_fetch(at(1), Reached::Yes, &p);
        net.saw_fetch(at(2), Reached::No, &p);
        assert_eq!(net.verdict(), Network::Up, "one bad remote among good ones is not an outage");
    }

    #[test]
    fn a_missing_route_is_conclusive_on_its_own() {
        // Nothing was run to find it out, so there is no second opinion to
        // wait for and no failure to attribute to anybody.
        let mut net = Reachability::default();
        net.saw_route(at(0), false);
        assert_eq!(net.verdict(), down(at(0), Outage::NoRoute));
        net.saw_route(at(5), false);
        assert_eq!(net.verdict(), down(at(0), Outage::NoRoute), "saying it twice is not news");
        assert!(!net.routable());
    }

    #[test]
    fn a_route_coming_back_lifts_what_a_missing_route_put_down() {
        let mut net = Reachability::default();
        net.saw_route(at(0), false);
        net.saw_route(at(1), true);
        assert_eq!(net.verdict(), Network::Up);
    }

    #[test]
    fn a_route_coming_back_does_not_lift_what_git_found() {
        // A captive portal has a perfectly good route. Believing the route
        // meant it was fixed would send every repository at it again.
        let mut net = unreachable_at(at(0));
        net.saw_route(at(1), true);
        assert_eq!(net.verdict(), down(at(0), Outage::Unreachable));

        // Only a fetch that reached something can clear what a fetch found.
        net.saw_fetch(at(2), Reached::Yes, &FetchPolicy::default());
        assert_eq!(net.verdict(), Network::Up);
    }

    #[test]
    fn with_both_facts_down_no_route_is_what_the_user_is_told() {
        let mut net = unreachable_at(at(0));
        net.saw_route(at(5), false);
        assert_eq!(net.verdict(), down(at(0), Outage::NoRoute), "down since the earlier fact");
        net.saw_route(at(9), true);
        assert_eq!(net.verdict(), down(at(0), Outage::Unreachable));
    }

    #[test]
    fn failures_with_no_route_are_already_explained() {
        // Fetches in flight when the route went away fail offline. Counting
        // them would leave an `Unreachable` behind that outlives the route.
        let p = FetchPolicy::default();
        let mut net = Reachability::default();
        net.saw_route(at(0), false);
        for t in 1..5 {
            net.saw_fetch(at(t), Reached::No, &p);
        }
        net.saw_route(at(9), true);
        assert_eq!(net.verdict(), Network::Up);
    }

    #[test]
    fn up_admits_everything_that_is_due() {
        let snaps = [repo("a", None), on_disk("b", None)];
        let due: Vec<&RepoSnapshot> = snaps.iter().collect();
        let mut net = Reachability::default();
        assert_eq!(names(&net.admit(at(0), &due, &FetchPolicy::default())), ["a", "b"]);
    }

    #[test]
    fn with_no_route_only_a_path_remote_is_admitted() {
        // A clone from this disk is as reachable with the wifi off as on.
        let snaps = [repo("a", None), on_disk("b", None), repo("c", None)];
        let due: Vec<&RepoSnapshot> = snaps.iter().collect();
        let mut net = Reachability::default();
        net.saw_route(at(0), false);
        for t in [0, 60, 600] {
            assert_eq!(names(&net.admit(at(t), &due, &FetchPolicy::default())), ["b"]);
        }
    }

    #[test]
    fn unreachable_admits_path_remotes_and_one_recheck_per_interval() {
        let p = FetchPolicy { recheck: Duration::from_secs(60), ..Default::default() };
        let snaps = [repo("a", None), on_disk("b", None)];
        let due: Vec<&RepoSnapshot> = snaps.iter().collect();
        let mut net = unreachable_at(at(0));

        // The fetches that put it down were the first ask; the next waits.
        assert_eq!(names(&net.admit(at(30), &due, &p)), ["b"]);
        assert_eq!(names(&net.admit(at(60), &due, &p)), ["a", "b"]);
        assert_eq!(names(&net.admit(at(90), &due, &p)), ["b"]);
        assert_eq!(names(&net.admit(at(120), &due, &p)), ["a", "b"]);
    }

    #[test]
    fn a_recheck_with_nothing_hosted_due_is_not_spent() {
        let p = FetchPolicy { recheck: Duration::from_secs(60), ..Default::default() };
        let hosted = repo("a", None);
        let mut net = unreachable_at(at(0));
        assert!(net.admit(at(60), &[], &p).is_empty());
        // The interval is still open, so the first hosted repository to come
        // due answers it at once.
        assert_eq!(names(&net.admit(at(61), &[&hosted], &p)), ["a"]);
    }

    #[test]
    fn the_recheck_rotates_through_hosted_repositories() {
        // Whichever was asked longest ago, so a repository whose own remote is
        // broken cannot answer "no" for ever on everyone else's behalf.
        let p = FetchPolicy { recheck: Duration::ZERO, ..Default::default() };
        let pick = |snaps: &[RepoSnapshot]| {
            let due: Vec<&RepoSnapshot> = snaps.iter().collect();
            let mut net = unreachable_at(at(0));
            let admitted = net.admit(at(100), &due, &p);
            let hosted: Vec<&str> = snaps
                .iter()
                .filter(|s| s.has_remote_host() && admitted.contains(&s.id))
                .map(|s| s.id.name())
                .collect();
            assert_eq!(hosted.len(), 1, "{admitted:?}");
            hosted[0].to_string()
        };

        assert_eq!(
            pick(&[repo("a", Some(at(30))), repo("b", Some(at(10))), repo("c", Some(at(20)))]),
            "b"
        );
        assert_eq!(
            pick(&[repo("a", Some(at(30))), repo("b", None)]),
            "b",
            "never asked goes first"
        );
        // A path remote can say nothing about the network, however long ago
        // it was asked.
        assert_eq!(pick(&[on_disk("a", None), repo("b", Some(at(50)))]), "b");
    }

    #[test]
    fn an_offline_attempt_leaves_the_repository_exactly_as_it_was() {
        // The whole point: nothing was asked of this repository, so its
        // failure count does not move and it cannot reach quarantine.
        let health = FetchHealth {
            last_attempt: Some(at(0)),
            last_success: Some(at(0)),
            schedule: FetchSchedule::BackingOff { until: at(90), failures: 3 },
        };
        let after = offline_attempt(&health, at(60));
        assert_eq!(after.schedule, health.schedule);
        assert_eq!(after.last_success, health.last_success);
        assert_eq!(after.last_attempt, Some(at(60)), "the attempt itself is still recorded");
    }

    #[test]
    fn an_outage_of_any_length_never_quarantines_anything() {
        // The failure the verdict was drawn from is the only one that counts,
        // and one is a long way from `quarantine_after`.
        let p = FetchPolicy::default();
        let mut net = Reachability::default();
        let mut health = FetchHealth::due_now(T0);
        let id = repo("a", None).id;

        let mut counted = 0;
        for minute in 0..(24 * 60) {
            let now = at(minute * 60);
            net.saw_fetch(now, Reached::No, &p);
            health = if net.is_down() {
                offline_attempt(&health, now)
            } else {
                counted += 1;
                after_attempt(&health, &id, now, Attempt::Failed("Could not resolve host"), &p)
            };
        }
        assert_eq!(counted, 1, "only the fetch that established the verdict counted");
        assert!(
            !matches!(health.schedule, FetchSchedule::Quarantined { .. }),
            "a day offline quarantined a repository: {health:?}"
        );
    }
}
