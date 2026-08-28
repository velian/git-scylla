//! Who gets probed, when, and who is still owed one.
//!
//! Three rules:
//!
//! 1. **Exactly one re-probe.** A pull that writes a thousand files costs one
//!    `git status`, not a thousand — sets rather than queues, so repeated
//!    requests collapse.
//! 2. **Definite outranks observed.** A job finished, a repository was
//!    discovered, the user pressed Refresh: delaying the row that shows a
//!    job's result is the one thing the rate limit must never do. A watcher's
//!    report is noisier — a directory being written to re-triggers every
//!    debounce window — so only that path is rate-limited.
//! 3. **One way out.** Nothing starts the moment it's asked for; everything is
//!    noted, and [`ProbeTraffic::take_ready`] is the only way a repository
//!    comes back out.
//!
//! **No clock of its own.** `now` is an argument, so the rules can be tested
//! by stating a time rather than sleeping through one. No tokio either — this
//! decides, and the actor spawns.

use git_scylla_core::RepoId;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Where a probe request came from, which decides whether the rate limit
/// applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The engine knows something changed. Never rate-limited.
    Definite,
    /// A watcher saw filesystem activity. Rate-limited.
    Observed,
}

/// The admission state for probing: who is owed one, who has one in flight,
/// and when each was last started.
///
/// `BTreeSet`/`BTreeMap` rather than hashed: the sets are small, and this
/// buys a stable order out of [`Self::take_ready`] so tests can assert one.
pub struct ProbeTraffic {
    /// Least time between two probes of one repository, for requests that
    /// came from watching rather than from knowing.
    interval: Duration,
    /// Repositories with a probe in flight, so two never race.
    probing: BTreeSet<RepoId>,
    /// Owed a re-probe because something is known to have changed.
    definite: BTreeSet<RepoId>,
    /// Owed one because a watcher saw something. Only these are rate-limited.
    observed: BTreeSet<RepoId>,
    /// When each repository was last *started*, for the rate limit. An
    /// `Instant`, not the snapshot's `probed_at`: a probe still running has
    /// no `probed_at` yet.
    last: BTreeMap<RepoId, Instant>,
}

impl ProbeTraffic {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            probing: BTreeSet::new(),
            definite: BTreeSet::new(),
            observed: BTreeSet::new(),
            last: BTreeMap::new(),
        }
    }

    /// Record that this repository wants probing. Takes no `now`: noting a
    /// request doesn't admit it, and the interval is only checked later, by
    /// [`Self::take_ready`].
    ///
    /// Asked about twice, a repository is owed one probe, not two; asked
    /// about both ways, it's owed a *definite* one, so a watcher's report can
    /// never downgrade a job's result into something the rate limit may sit
    /// on.
    ///
    /// **Only note a repository the caller will eventually be able to
    /// start.** What is owed is deferred, never dropped, and holds
    /// [`Self::is_idle`] false — the caller alone knows what it can act on,
    /// so filtering is the caller's job.
    pub fn note(&mut self, repo: &RepoId, why: Why) {
        match why {
            Why::Definite => {
                self.observed.remove(repo);
                self.definite.insert(repo.clone());
            }
            // Already owed the stronger kind; leave it alone.
            Why::Observed if self.definite.contains(repo) => {}
            Why::Observed => {
                self.observed.insert(repo.clone());
            }
        }
    }

    /// Everything that may start right now, marked as started.
    ///
    /// **The only exit.** A returned repository moves from owed to in
    /// flight, and the caller must start it — so `can_start` needs to answer
    /// for every reason a caller might fail to, not just whether a job holds
    /// the repository.
    ///
    /// Definite requests first and without a rate limit, then what a watcher
    /// merely saw, at most once per `interval` per repository. A repository
    /// held back here is deferred, not dropped, and taken on a later pass.
    /// The actor is one task with a synchronous pump, so nothing runs
    /// between `can_start` answering and the caller acting on the result.
    pub fn take_ready(&mut self, now: Instant, can_start: impl Fn(&RepoId) -> bool) -> Vec<RepoId> {
        let (probing, last, interval) = (&self.probing, &self.last, self.interval);
        let free = |r: &RepoId| !probing.contains(r) && can_start(r);
        let due = |r: &RepoId| last.get(r).is_none_or(|at| now >= *at + interval);

        let mut ready: Vec<RepoId> = self.definite.iter().filter(|r| free(r)).cloned().collect();
        ready.extend(self.observed.iter().filter(|r| free(r) && due(r)).cloned());

        for repo in &ready {
            // Clear both sets: a probe starting now covers every request
            // owed, however it was asked for.
            self.definite.remove(repo);
            self.observed.remove(repo);
            self.probing.insert(repo.clone());
            self.last.insert(repo.clone(), now);
        }
        ready
    }

    /// A probe came back. A request that arrived while it was running is still
    /// owed, and will be taken on the next pass.
    pub fn finished(&mut self, repo: &RepoId) {
        self.probing.remove(repo);
    }

    /// This repository is gone. Forget everything about it, `last` included,
    /// so a repository that returns under the same path isn't rate-limited
    /// by a reading from before it left.
    pub fn forget(&mut self, repo: &RepoId) {
        self.probing.remove(repo);
        self.definite.remove(repo);
        self.observed.remove(repo);
        self.last.remove(repo);
    }

    /// Nothing owed and nothing in flight. The caller's shutdown gate.
    pub fn is_idle(&self) -> bool {
        self.probing.is_empty() && self.definite.is_empty() && self.observed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> RepoId {
        RepoId::from_canonical(format!("/r/{name}"))
    }

    /// Every repository may start, always.
    fn anything(_: &RepoId) -> bool {
        true
    }

    fn traffic() -> ProbeTraffic {
        ProbeTraffic::new(Duration::from_secs(1))
    }

    #[test]
    fn a_storm_of_requests_collapses_to_one_probe() {
        let mut t = traffic();
        let now = Instant::now();
        for _ in 0..1000 {
            t.note(&repo("a"), Why::Observed);
        }
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
        // ...and nothing is left owed by the other 999.
        assert_eq!(t.take_ready(now, anything), Vec::<RepoId>::new());
    }

    #[test]
    fn a_definite_request_is_never_rate_limited() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Definite);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
        t.finished(&repo("a"));

        // Immediately again, well inside the interval.
        t.note(&repo("a"), Why::Definite);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
    }

    #[test]
    fn an_observed_request_waits_for_the_interval_and_is_not_dropped() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Observed);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
        t.finished(&repo("a"));

        // Asked again straight away: held, not discarded.
        t.note(&repo("a"), Why::Observed);
        assert_eq!(t.take_ready(now, anything), Vec::<RepoId>::new());
        assert!(!t.is_idle(), "the request was dropped rather than deferred");

        // A moment before the interval elapses, still held.
        let almost = now + Duration::from_millis(999);
        assert_eq!(t.take_ready(almost, anything), Vec::<RepoId>::new());

        // And then it is taken, without being asked again.
        let later = now + Duration::from_secs(1);
        assert_eq!(t.take_ready(later, anything), vec![repo("a")]);
    }

    #[test]
    fn a_definite_request_outranks_an_observed_one_for_the_same_repository() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Observed);
        // Stamp a recent probe, so the rate limit would otherwise hold an observed one.
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
        t.finished(&repo("a"));

        t.note(&repo("a"), Why::Observed);
        t.note(&repo("a"), Why::Definite);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")], "the definite request was held");
    }

    #[test]
    fn a_busy_repository_is_held_and_taken_once_it_is_free() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Definite);
        t.note(&repo("b"), Why::Definite);

        let ready = t.take_ready(now, |r| r != &repo("a"));
        assert_eq!(ready, vec![repo("b")]);
        assert!(!t.is_idle());

        // `a` is still owed, and taken when it stops being busy.
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
    }

    #[test]
    fn a_probe_in_flight_blocks_a_second_one_for_the_same_repository() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Definite);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);

        // The request that arrives during the probe is remembered...
        t.note(&repo("a"), Why::Definite);
        assert_eq!(t.take_ready(now, anything), Vec::<RepoId>::new());

        // ...and honoured when the first one comes back.
        t.finished(&repo("a"));
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
    }

    #[test]
    fn forgetting_a_repository_forgets_when_it_was_last_probed() {
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Observed);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
        t.finished(&repo("a"));

        t.forget(&repo("a"));
        assert!(t.is_idle());

        // Rediscovered, and probed at once rather than a second later.
        t.note(&repo("a"), Why::Observed);
        assert_eq!(t.take_ready(now, anything), vec![repo("a")]);
    }

    #[test]
    fn idle_means_nothing_owed_and_nothing_in_flight() {
        let mut t = traffic();
        let now = Instant::now();
        assert!(t.is_idle());

        t.note(&repo("a"), Why::Definite);
        assert!(!t.is_idle(), "a request is owed");

        t.take_ready(now, anything);
        assert!(!t.is_idle(), "a probe is in flight");

        t.finished(&repo("a"));
        assert!(t.is_idle());
    }

    #[test]
    fn what_the_caller_refuses_stays_owed_and_holds_idle_open() {
        // This is why `note` has a precondition: noting something the caller
        // can never start would hold `is_idle` false forever.
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("held"), Why::Definite);
        assert_eq!(t.take_ready(now, |_| false), Vec::<RepoId>::new());
        assert!(!t.is_idle());

        // Still owed, and taken as soon as the caller can act.
        assert_eq!(t.take_ready(now, anything), vec![repo("held")]);
    }
}
