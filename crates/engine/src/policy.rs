//! Preconditions: is this action safe to run against this repository?
//!
//! Pure functions over a snapshot. No I/O, no clock of its own — `now` is an
//! argument — so the whole safety surface is exhaustively unit-testable, and a
//! table test renders it as something a reviewer reads rather than a set of
//! assertions a reviewer skims.
//!
//! Two rules govern the design:
//!
//! * **Default to refusing on any doubt.** A skip the user overrides is cheaper
//!   than a mutation they did not expect. Every skip carries an actionable
//!   reason, so refusing is not a dead end.
//! * **Do not over-restrict `Fetch`.** It is the one action that cannot touch a
//!   worktree or local history, and the background scheduler consults this same
//!   function. An over-restriction here silently degrades automatic fetching,
//!   where the symptom is a `behind` count that quietly stops advancing.

use git_scylla_core::{
    Action, FetchHealth, FetchSchedule, Head, PullMode, RepoId, RepoKind, RepoSnapshot, SkipReason,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

/// Whether an action may run against one repository.
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

/// The thresholds the rules consult.
///
/// Separate from the rules themselves: a threshold is the kind of thing to
/// revisit after real use rather than before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Beyond this age a snapshot is not trusted and every action skips with
    /// [`SkipReason::SnapshotStale`].
    ///
    /// Under a watcher the real age is milliseconds; this bound is for
    /// repositories the watcher does not cover, and for a UI left open
    /// overnight.
    #[serde(with = "git_scylla_core::serde_duration")]
    pub max_snapshot_age: Duration,
    /// How recently a fetch must have succeeded for `--force-with-lease` to
    /// mean anything. A lease against a stale remote-tracking ref is not a
    /// lease — it is a force push with extra steps.
    #[serde(with = "git_scylla_core::serde_duration")]
    pub max_lease_age: Duration,
    /// May a bare repository be fetched? Bare repositories are frequently local
    /// mirrors the user does not think of as part of the working set, so this
    /// is a switch rather than a rule.
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

/// Is `action` safe to run against `snap`?
///
/// The staleness rule needs both a clock and a bound, so `evaluate(action,
/// snap)` could not express it. Passing `now` in keeps the function pure, which
/// is the property the exhaustive table depends on.
pub fn evaluate(
    action: &Action,
    snap: &RepoSnapshot,
    now: SystemTime,
    policy: &Policy,
) -> Eligibility {
    // Universal, and first: acting on a snapshot you do not trust is how a bulk
    // tool corrupts a working set. This covers a failed or timed-out probe as
    // well as an old one — from the caller's side both mean "I do not know what
    // is in this repository", and the remedy for both is to refresh.
    //
    // **Except for `Fetch`**, which carries its own, narrower trust rule. See
    // [`fetch`]: blocking it on *age* would be the exact over-restriction this
    // module's header warns about, because a snapshot goes stale by sitting
    // still and a repository that sits still is one nothing will ever re-probe.
    // Automatic fetching would then stop after thirty seconds and the symptom
    // would be a `behind` count that quietly never advances again.
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
        Action::SyncDefault { .. } => sync_default(snap),
        Action::DevTag { push, .. } => dev_tag(snap, push.is_some()),
        Action::Custom { .. } => custom(snap),
    }
}

/// Is this snapshot too old, or the product of a probe that failed?
///
/// Both halves live on the snapshot as [`RepoSnapshot::is_stale`]: the grid
/// marks a stale row and this refuses to act on one, and those must be the same
/// question. A row shown as current that an action then skipped as stale would
/// be the tool contradicting itself on screen.
fn untrustworthy(snap: &RepoSnapshot, now: SystemTime, policy: &Policy) -> Option<SkipReason> {
    snap.is_stale(now, policy.max_snapshot_age).then_some(SkipReason::SnapshotStale)
}

/// The blockers common to every action that touches a worktree.
///
/// Ordered by how much they explain. A repository mid-rebase is also detached,
/// and "rebase in progress" is the fact the user needs; reporting "detached
/// HEAD" would be true and useless.
fn worktree_blockers(snap: &RepoSnapshot) -> Option<SkipReason> {
    if !snap.kind.has_worktree() {
        return Some(SkipReason::BareRepo);
    }
    if let Some(op) = snap.op {
        return Some(SkipReason::OperationInProgress(op));
    }
    None
}

/// Requires a real branch to act on.
fn branch_blockers(snap: &RepoSnapshot) -> Option<SkipReason> {
    match snap.head {
        Head::Branch(_) => None,
        Head::Detached(_) => Some(SkipReason::DetachedHead),
        Head::Unborn(_) => Some(SkipReason::UnbornBranch),
    }
}

/// Fetch is the permissive one, on purpose.
///
/// Safe on a dirty worktree, on a detached HEAD, mid-rebase, and on an unborn
/// branch: it advances `refs/remotes/**` and touches nothing else. The only
/// questions are whether there is anywhere to fetch from and whether the user
/// wants bare repositories included.
fn fetch(snap: &RepoSnapshot, policy: &Policy) -> Eligibility {
    // Its own trust rule, deliberately narrower than the universal one.
    //
    // A failed probe, or a row restored from the cache, means we do not know
    // this is a repository at all — there may be no directory there. Age means
    // only that nothing has looked recently, which says nothing about whether
    // advancing `refs/remotes/**` is safe, and is *precisely* when the `behind`
    // count most needs the fetch.
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
        // Configured but unresolvable. Not `NoUpstream`: the configuration is
        // right and the remote is wrong, which is a different fix.
        return Eligibility::Skip(SkipReason::UpstreamGone);
    };
    if sync.behind == 0 {
        // Nothing to pull in, whatever this branch is ahead by.
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    if mode == PullMode::FfOnly && sync.ahead > 0 {
        return Eligibility::Skip(SkipReason::Diverged);
    }
    if !snap.is_clean() {
        // Required by every mode, including rebase and merge. An autostash is a
        // policy decision and the default is no autostash — and the argv says
        // `--no-autostash` so a user's `rebase.autoStash = true` cannot quietly
        // overrule it.
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    Eligibility::Eligible
}

/// Worktree dirtiness is **irrelevant** to push; do not restrict on it.
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
        // No tracking ref yet. With a remote to set upstream to, there is by
        // definition something to publish; without one we cannot know.
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
        // A lease against a stale remote-tracking ref is not a lease. Reported
        // as stale rather than as a push problem, because refreshing is the fix.
        return Eligibility::Skip(SkipReason::SnapshotStale);
    }
    Eligibility::Eligible
}

/// Has anything fetched into this repository recently enough to lease against?
///
/// Reads both clocks: `Upstream::last_fetch` moves for a fetch by anyone,
/// including the user's own terminal, while `FetchHealth::last_success` is this
/// tool's scheduler. A repository quarantined for a day has an ancient lease
/// however healthy it looks.
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

/// The clean-worktree requirement is not negotiable: bulk checkout is genuinely
/// useful and genuinely dangerous on dirty trees.
fn checkout(snap: &RepoSnapshot, create: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    // `git checkout -b` works on an unborn branch; checking out an existing ref
    // does not, because there are none.
    if !create && matches!(snap.head, Head::Unborn(_)) {
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    if !snap.is_clean() {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    // `SkipReason::RefNotFound` cannot be decided here: whether a ref exists is
    // not in `RepoSnapshot` and deliberately must not be, because a ref list is
    // cold data and this path has to finish in under a second for a hundred
    // repositories. The planner resolves it from the filesystem instead.
    Eligibility::Eligible
}

/// Born or unborn are both fine — the first commit is still a commit.
fn commit(snap: &RepoSnapshot, stage_all: bool) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    let staged = snap.work.staged > 0;
    // `git add -A` picks up untracked files as well as modifications, so
    // untracked-only still counts as something to commit. It is also the case
    // the plan sheet has to warn about loudest, because `-A` commits whatever
    // is lying around.
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

/// "Clean enough" means no conflicts: popping onto merely-modified files
/// usually works and git says so clearly when it does not, but popping onto an
/// unresolved conflict cannot.
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

/// Cutting a tag. Deliberately permissive about the worktree, and deliberately
/// not about `HEAD`.
///
/// A tag names a commit. Whether the worktree is clean has nothing to do with
/// whether that commit exists, so requiring a clean tree would refuse work for
/// a reason that is not a reason — the same mistake `Checkout`'s rule would be
/// if it were applied to `Branch`. The *warning* on the plan carries the real
/// concern, which is that a tag on `HEAD` does not include what is still
/// uncommitted.
///
/// A detached `HEAD` is fine too, and this is the first action to say so: every
/// other one either moves a branch or promises to put the user back on one.
/// Tagging cares only that there is a commit.
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

/// Syncing the default branch. The preconditions are the *snapshot's* half;
/// which branch to visit is not on a snapshot and is answered by the engine.
///
/// Deliberately **not** requiring a clean worktree, which is the whole point of
/// the action: it stashes. And deliberately not judged on `ahead`/`behind`,
/// which describe the branch the user is standing on and say nothing about the
/// one being synced — refusing a repository because the *feature* branch is up
/// to date would refuse exactly the case this exists for.
///
/// It cannot check that the default branch has an upstream, for the same
/// reason: the snapshot describes one branch and this visits another. That
/// failure surfaces as a job with git's own message, which is honest, and is
/// the cost of keeping the ref list off the scan path.
fn sync_default(snap: &RepoSnapshot) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap).or_else(|| branch_blockers(snap)) {
        // A branch, not just a worktree: the action's promise is to put the
        // user back where they were, and "where they were" on a detached HEAD
        // is a commit that `checkout` would leave detached again with a
        // different warning. Refusing is the honest answer.
        return Eligibility::Skip(skip);
    }
    if snap.remotes.is_empty() {
        return Eligibility::Skip(SkipReason::NoRemote);
    }
    Eligibility::Eligible
}

/// Creating a branch does not touch the worktree, so the clean requirement that
/// governs `Checkout` deliberately does not apply here.
///
/// It does need somewhere to point: an unborn branch has no commit to start
/// from, and `git branch` on one fails.
fn branch(snap: &RepoSnapshot) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    if matches!(snap.head, Head::Unborn(_)) {
        return Eligibility::Skip(SkipReason::UnbornBranch);
    }
    Eligibility::Eligible
}

/// The guards on undo, and every one of them is mandatory.
///
/// `reset --hard` destroying real work is the risk undo carries, and refusing
/// loudly beats succeeding destructively. Three of the four guards are here;
/// the fourth — that `HEAD` is still where the job left it — needs the *job* as
/// well as the snapshot, and so lives in [`crate::plan::undo`].
///
/// The staleness guard is the universal one in [`evaluate`], which applies
/// because `Reset` is not `Fetch`. That is deliberate and not incidental: this
/// is the action that most needs to be told "refresh first".
fn reset(snap: &RepoSnapshot, to: &git_scylla_core::Oid) -> Eligibility {
    if let Some(skip) = worktree_blockers(snap) {
        return Eligibility::Skip(skip);
    }
    // A repository the user has since edited is one where `--hard` would throw
    // away work that has nothing to do with the batch being undone.
    if !snap.is_clean() {
        return Eligibility::Skip(SkipReason::DirtyWorktree);
    }
    // Already there. Undoing again would be a no-op that reads as a success.
    if snap.head_oid.as_ref() == Some(to) {
        return Eligibility::Skip(SkipReason::UpToDate);
    }
    Eligibility::Eligible
}

/// Only the universal rules. The engine cannot reason about an arbitrary
/// command and must not pretend to — including about whether a bare repository
/// is a valid target for it.
fn custom(snap: &RepoSnapshot) -> Eligibility {
    if let Some(op) = snap.op {
        return Eligibility::Skip(SkipReason::OperationInProgress(op));
    }
    Eligibility::Eligible
}

// ---- automatic fetching -------------------------------------------------
//
// What makes `behind` a fact rather than a guess with a timestamp. None of this
// is a new action; all of it is the decision of *when*.

/// The thresholds automatic fetching consults.
///
/// No `per_host` field: that cap is `Limits::per_host`, enforced by the
/// scheduler's semaphore. Two places to write down one number is how the
/// background and foreground paths end up disagreeing about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchPolicy {
    /// Per repository. Fifteen minutes: often enough that `behind` is worth
    /// believing, rare enough that a working set of a hundred is a few
    /// subprocesses a minute.
    #[serde(with = "git_scylla_core::serde_duration")]
    pub interval: Duration,
    /// How far either side of the interval a repository's slot may sit, as a
    /// percentage. What stops eighty repositories fetching in the same second.
    pub jitter_pct: u8,
    /// Waits after consecutive failures. Indexed by failure count, saturating
    /// at the last.
    #[serde(with = "backoff_serde")]
    pub backoff: [Duration; 4],
    /// Consecutive failures before a repository is quarantined and never
    /// retried automatically.
    pub quarantine_after: u32,
    /// The off switch, for somebody on a tethered connection.
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
            enabled: true,
        }
    }
}

impl FetchPolicy {
    /// The wait after `failures` consecutive failures.
    pub fn backoff_for(&self, failures: u32) -> Duration {
        self.backoff[(failures.saturating_sub(1) as usize).min(self.backoff.len() - 1)]
    }

    /// When this repository should next be attempted after a success.
    ///
    /// Symmetric about the interval: the window is
    /// `[interval - span, interval + span)`, so the *mean* stays the configured
    /// interval rather than drifting up by half the jitter. The jitter itself is
    /// **deterministic per repository** — see [`jitter`].
    pub fn next_due(&self, id: &RepoId, after: SystemTime) -> SystemTime {
        after
            + self.interval.saturating_sub(self.span())
            + jitter(id, self.interval, self.jitter_pct)
    }

    /// How far either side of the interval a slot may sit.
    pub fn span(&self) -> Duration {
        self.interval.mul_f64(f64::from(self.jitter_pct.min(100)) / 100.0)
    }
}

/// This repository's offset within the fetch cycle, in `[0, 2 × span)`.
///
/// Non-negative, and applied by [`FetchPolicy::next_due`] to
/// `interval - span` — so the window is symmetric about the interval while
/// every value a `Duration` has to hold stays positive. Returning a *signed*
/// offset and folding the negative half into the positive one, which is what
/// this did first, quietly halves the spread and pushes the mean interval up by
/// half the jitter.
///
/// Derived from a hash of the id rather than a fresh random, for two reasons.
/// The same repository then keeps its slot across restarts instead of
/// re-rolling into a new herd every launch — a random jitter re-rolled on each
/// start is a herd with extra steps. And it makes the schedule reproducible,
/// which is the only way any of this gets tested.
///
/// FNV-1a rather than `DefaultHasher`, whose output is explicitly not stable
/// across Rust releases: "keeps its slot across restarts" would otherwise stop
/// being true the next time the toolchain moved.
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

/// Which repositories are due a background fetch.
///
/// **Pure**: no I/O and no clock of its own, so the whole schedule is testable
/// by advancing a number — which is the only way backoff and quarantine get
/// tested at all, short of waiting two hours.
///
/// The engine is free to ignore any entry. A repository this returns may be
/// busy, mid-batch or already queued by the time the engine looks, and a tick
/// dropped for any of those reasons is **not a failure** and must not count
/// toward backoff.
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
        // The same `evaluate` a user fetch goes through, not a second copy of
        // it. Two eligibility rules for one action is how the background path
        // and the foreground path drift apart.
        .filter(|snap| evaluate(&ACTION, snap, now, policy).is_eligible())
        .map(|snap| snap.id.clone())
        .collect()
}

/// Has this repository's schedule come round?
fn ready(now: SystemTime, schedule: &FetchSchedule) -> bool {
    match schedule {
        FetchSchedule::Due(at) | FetchSchedule::BackingOff { until: at, .. } => now >= *at,
        // Never automatically. The tool has stopped trying and said why; only
        // the user asking restarts it.
        FetchSchedule::Quarantined { .. } => false,
        FetchSchedule::Disabled => false,
    }
}

/// What one fetch attempt did to a repository's schedule.
///
/// Pure, and separate from the engine for the same reason [`due`] is: this is
/// where backoff and quarantine actually live, and it has to be assertable by
/// advancing a clock rather than by failing a remote five times.
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
                    // Verbatim, and it is the whole value of the state:
                    // "quarantined" without "Permission denied (publickey)" is
                    // a dead end.
                    last_error: error.to_string(),
                }
            } else {
                FetchSchedule::BackingOff { until: now + fetch.backoff_for(failures), failures }
            };
            FetchHealth { last_attempt: Some(now), last_success: health.last_success, schedule }
        }
    }
}

/// How a fetch attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt<'a> {
    Ok,
    /// With the first line of git's own stderr, kept verbatim.
    Failed(&'a str),
}

/// The user asked. Clears backoff and quarantine, whatever the outcome.
///
/// The reset button, and the only one — so the UI has to say so on the
/// quarantine indicator itself, or a quarantined repository looks like a dead
/// end rather than one click from a retry.
pub fn manual_attempt(
    health: &FetchHealth,
    id: &RepoId,
    now: SystemTime,
    outcome: Attempt<'_>,
    fetch: &FetchPolicy,
) -> FetchHealth {
    // Cleared *first*, so a failure that follows starts a fresh backoff rather
    // than resuming the one the user was trying to escape.
    let cleared = FetchHealth {
        last_attempt: health.last_attempt,
        last_success: health.last_success,
        schedule: FetchSchedule::Due(now),
    };
    after_attempt(&cleared, id, now, outcome, fetch)
}

/// `[Duration; 4]` as four millisecond counts.
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

    /// Every `InProgress` value, so the tests below stay exhaustive when one is
    /// added. Not public: no caller outside these tests ever wanted it.
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

    /// A clean, tracked, in-sync repository. Every test below is a delta from
    /// this, so the thing under test is the only thing that differs.
    fn snap() -> RepoSnapshot {
        let mut s = RepoSnapshot::stub("/r");
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead: 0, behind: 0 }),
            last_fetch: Some(T0),
        });
        s.remotes = vec![Remote { name: "origin".into(), host: None }];
        s.fetch = FetchHealth::due_now(T0);
        s
    }

    fn sync(ahead: u32, behind: u32) -> RepoSnapshot {
        let mut s = snap();
        s.upstream.as_mut().unwrap().sync = Some(AheadBehind { ahead, behind });
        s
    }

    /// Evaluate at T0, so nothing is stale unless the test makes it so.
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

    // ---- the universal rule -------------------------------------------

    #[test]
    fn an_untrusted_snapshot_blocks_every_action_including_fetch() {
        // The rule that has to come first. A snapshot we cannot vouch for means
        // we do not know what is in the repository, and every remedy starts with
        // refreshing.
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
            // The bound is inclusive: exactly at it, the snapshot is still
            // trusted and the action is judged on its own merits.
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
        // A snapshot goes stale by sitting still, and a repository that sits
        // still is one nothing will ever re-probe — so blocking `Fetch` on age
        // stops automatic fetching after thirty seconds, and the symptom is a
        // `behind` count that quietly never advances again.
        let s = snap();
        assert!(evaluate(&FETCH, &s, at(86_400), &Policy::default()).is_eligible());
    }

    #[test]
    fn a_row_restored_from_the_cache_is_not_fetched_either() {
        // Age is not disqualifying; not having been seen by this process is.
        // There may be no directory there at all.
        let mut s = snap();
        s.from_cache = true;
        assert_eq!(skip(&FETCH, &s), Some(SkipReason::SnapshotStale));
    }

    #[test]
    fn a_snapshot_from_the_future_is_treated_as_fresh() {
        // A clock moved. Skipping every repository on the machine until it
        // settles is a worse failure than trusting a snapshot that is, if
        // anything, too new.
        let mut s = snap();
        s.probed_at = at(10_000);
        assert!(ev(&FETCH, &s).is_eligible());
    }

    // ---- fetch is deliberately permissive -----------------------------

    #[test]
    fn fetch_is_not_blocked_by_anything_about_the_worktree() {
        // Over-restricting here silently degrades automatic fetching, and the
        // symptom is a `behind` count that quietly stops advancing.
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

    // ---- pull ----------------------------------------------------------

    #[test]
    fn pull_requires_something_to_pull() {
        for mode in [FF, REBASE, MERGE] {
            assert_eq!(skip(&mode, &sync(0, 0)), Some(SkipReason::UpToDate), "{mode:?}");
            assert!(ev(&mode, &sync(0, 3)).is_eligible(), "{mode:?}");
        }
    }

    #[test]
    fn ff_only_refuses_a_branch_that_is_ahead_but_the_others_do_not() {
        // The one rule that differs between the modes.
        assert_eq!(skip(&FF, &sync(2, 3)), Some(SkipReason::Diverged));
        assert!(ev(&REBASE, &sync(2, 3)).is_eligible());
        assert!(ev(&MERGE, &sync(2, 3)).is_eligible());
    }

    #[test]
    fn ahead_only_is_up_to_date_rather_than_diverged() {
        // Nothing to pull in, so the honest reason is "already up to date" and
        // not "diverged" — even for ff-only, where being ahead would block a
        // pull that had anything to do.
        for mode in [FF, REBASE, MERGE] {
            assert_eq!(skip(&mode, &sync(3, 0)), Some(SkipReason::UpToDate), "{mode:?}");
        }
    }

    #[test]
    fn every_pull_mode_requires_a_clean_worktree() {
        // Including rebase and merge. An autostash is a policy decision and the
        // default is no autostash.
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
        // A stopped rebase is also a detached HEAD. "rebase in progress" is the
        // fact the user needs; "detached HEAD" would be true and useless.
        let mut s = sync(0, 3);
        s.op = Some(InProgress::Rebase);
        s.head = Head::Detached(Oid::parse("deadbeef").unwrap());
        assert_eq!(skip(&FF, &s), Some(SkipReason::OperationInProgress(InProgress::Rebase)));
    }

    // ---- push ----------------------------------------------------------

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
        // A repository quarantined for a day has an ancient lease however
        // healthy it looks.
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

        // The scheduler's own clock counts too, and the newer of the two wins.
        s.fetch.last_success = Some(at(100_000 - 100));
        assert!(evaluate(&leased, &s, at(100_000), &policy).is_eligible());

        // Never fetched at all is not a lease.
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

        // ...but only if there is a remote to set it to.
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
        // `git add -A` picks them up, so a repository with only untracked files
        // is still committable — and it is the case the plan sheet must warn
        // about loudest.
        let all = Action::Commit { message: "m".into(), stage_all: true, no_verify: false };
        let plain = Action::Commit { message: "m".into(), stage_all: false, no_verify: false };
        let mut s = snap();
        s.work.untracked = 3;
        assert!(ev(&all, &s).is_eligible());
        assert_eq!(skip(&plain, &s), Some(SkipReason::UpToDate));
    }

    #[test]
    fn commit_is_fine_on_an_unborn_branch() {
        // The first commit is still a commit.
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
        // The engine cannot reason about an arbitrary command and must not
        // pretend to — including about whether a bare repository is a target.
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

    // ---- cross-cutting ---------------------------------------------------

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

// ---- automatic fetching -------------------------------------------------

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
        s.remotes = vec![Remote { name: "origin".into(), host: Some("example.invalid".into()) }];
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
        // `Disabled`, not perpetually failing: a repository with no remote must
        // not enter backoff, or it quarantines for the wrong reason.
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
        // The tool has stopped trying and said why. Only the user restarts it.
        let mut s = repo("a");
        s.fetch.schedule = FetchSchedule::Quarantined { since: T0, last_error: "no key".into() };
        assert!(due_at(0, std::slice::from_ref(&s)).is_empty());
        assert!(due_at(86_400 * 30, &[s]).is_empty());
    }

    #[test]
    fn due_asks_the_same_precondition_a_user_fetch_asks() {
        // Two eligibility rules for one action is how the background path and
        // the foreground path drift apart.
        let mut untrusted = repo("a");
        untrusted.outcome = ProbeOutcome::Error("boom".into());
        assert!(due_at(0, &[untrusted]).is_empty());

        // ...and age alone does not disqualify, or automatic fetching would
        // stop thirty seconds after the last probe.
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
        // Asserted against a clock rather than by failing a remote five times
        // over two hours.
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
                // Verbatim: "quarantined" without the reason is a dead end.
                assert_eq!(last_error, "Permission denied");
            }
            other => panic!("{other:?}"),
        }
        // ...and the successful history is kept, so the UI can still say how
        // long it has been broken.
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
        // Cleared regardless of outcome: a manual fetch that fails starts a
        // *fresh* backoff rather than resuming the one being escaped.
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
        // A jitter re-rolled on each launch is a herd with extra steps.
        let p = FetchPolicy::default();
        let a = RepoId::from_canonical("/work/a");
        assert_eq!(jitter(&a, p.interval, p.jitter_pct), jitter(&a, p.interval, p.jitter_pct));
    }

    #[test]
    fn jitter_spreads_a_working_set_across_the_window() {
        // The whole point: a launch with a hundred repositories must not fetch
        // them in one second.
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
        // It rides in the engine's config, which the shell will persist.
        let p = FetchPolicy::default();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<FetchPolicy>(&json).unwrap(), p, "{json}");
    }
}
