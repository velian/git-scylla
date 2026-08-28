//! Who gets probed, when, and who is still owed one.
//!
//! Three rules, and every one of them was a bug before it was a rule:
//!
//! 1. **Exactly one re-probe.** A pull that writes a thousand files costs one
//!    `git status`, not a thousand. Sets rather than queues are what make that
//!    true however many times something asked.
//! 2. **Definite outranks observed.** A job finished, a repository was
//!    discovered, the user pressed Refresh — there is exactly one of each, and
//!    delaying the row that shows a job's result is the one thing a rate limit
//!    must never do. A watcher's report is the opposite: a directory being
//!    written to produces a fresh debounce window every 300 ms, for ever, and
//!    each one would otherwise be a subprocess.
//! 3. **One way out.** Nothing is started at the moment it is asked for.
//!    Everything is noted, and [`ProbeTraffic::take_ready`] is the only way a
//!    repository ever comes back out. The earlier version admitted some
//!    requests immediately and deferred the rest, and keeping those two paths'
//!    sets in step by hand is exactly where the redundant second probe came
//!    from.
//!
//! **No clock of its own.** `now` is an argument, the same discipline `policy`
//! keeps and for the same reason: it is what lets the rules be tested by
//! stating a time rather than by sleeping through one. No tokio either — this
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

/// The admission state for probing: who is owed one, who has one in flight, and
/// when each was last started.
///
/// Ordered collections rather than hashed ones. The sets are small and this is
/// not a hot path, and what it buys is a stable order out of
/// [`Self::take_ready`] — which makes the tests state a result rather than sort
/// one.
pub struct ProbeTraffic {
    /// The least time between two probes of one repository, for requests that
    /// came from watching rather than from knowing.
    interval: Duration,
    /// Repositories with a probe in flight, so two never race.
    probing: BTreeSet<RepoId>,
    /// Owed a re-probe because something is known to have changed.
    definite: BTreeSet<RepoId>,
    /// Owed one because a watcher saw something. Only these are rate-limited.
    observed: BTreeSet<RepoId>,
    /// When each repository was last *started*, for the rate limit.
    ///
    /// `Instant` rather than the snapshot's `probed_at`: this is about how
    /// often work is begun, which a wall clock that can move is the wrong tool
    /// for, and a probe still running has no `probed_at` yet.
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

    /// Record that this repository wants probing.
    ///
    /// Takes no `now`, because nothing here is decided yet: noting a request is
    /// not admitting it, and the only time-dependent question — has the
    /// interval elapsed — is asked by [`Self::take_ready`] against the moment
    /// it is asked.
    ///
    /// A repository asked about twice is owed one probe, not two. Asked about
    /// both ways, it is owed a *definite* one: the stronger claim wins, so a
    /// watcher's report can never downgrade a job's result into something the
    /// rate limit may sit on.
    ///
    /// **Only note a repository the caller will eventually be able to start.**
    /// What is owed is deferred, never dropped, and it holds [`Self::is_idle`]
    /// false — so noting one that can never start is how a caller ends up
    /// unable to wind down. Filtering that is the caller's job, because only it
    /// knows what it can act on.
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
    /// **The only exit.** A repository in the returned vector has been moved
    /// out of what is owed and into what is in flight, and the caller is
    /// obliged to start it — which is why `can_start` must answer for every
    /// reason a caller might fail to, not only for whether a job holds the
    /// repository. Nothing runs between the predicate answering and the caller
    /// acting: the actor is one task and its pump is synchronous.
    ///
    /// Definite requests first and without a rate limit, then what a watcher
    /// merely saw, at most once per `interval` per repository. Deferred rather
    /// than dropped: the caller's loop wakes on its other timers, and a
    /// repository held back here is taken on a later pass.
    pub fn take_ready(&mut self, now: Instant, can_start: impl Fn(&RepoId) -> bool) -> Vec<RepoId> {
        let (probing, last, interval) = (&self.probing, &self.last, self.interval);
        let free = |r: &RepoId| !probing.contains(r) && can_start(r);
        let due = |r: &RepoId| last.get(r).is_none_or(|at| now >= *at + interval);

        let mut ready: Vec<RepoId> = self.definite.iter().filter(|r| free(r)).cloned().collect();
        ready.extend(self.observed.iter().filter(|r| free(r) && due(r)).cloned());

        for repo in &ready {
            // Both sets: a probe starting now reads the state as it is, so it
            // satisfies every request owed to this repository however it was
            // asked for. Leaving the other entry is what fired a second,
            // redundant probe the moment the first one finished.
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

    /// This repository is gone. Forget everything about it, `last` included.
    ///
    /// The old version cleared what was owed and left the timestamp behind, so
    /// the map grew for the life of the process and a repository that came back
    /// under the same path found its first watcher-driven probe rate-limited by
    /// a reading from before it left.
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
        // The rule the watcher exists to survive: a pull writing a thousand
        // files must not cost a thousand `git status`.
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
        // A job finished. Delaying the row that shows its result is the one
        // thing the limit must not do, so the interval does not apply.
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
        // Both arrive while the repository is busy. The definite one must win,
        // or a job's result would sit behind a rate limit that exists for
        // watcher noise.
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("a"), Why::Observed);
        // Stamp a recent probe, so the rate limit would hold an observed one.
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
        // Two probes of one repository must never race, whatever asked for
        // them.
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
        // The leak, as behaviour rather than as memory: a repository that came
        // back under the same path used to find its first watcher-driven probe
        // held by a reading taken before it went away.
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
        // Deferred, never dropped — being dropped here is how a repository ends
        // up in neither the owed set nor in flight.
        //
        // It is also the reason `note` has a precondition. A caller that noted
        // something it can never start would hold `is_idle` false for ever, and
        // an engine that cannot go idle cannot shut down. Only the caller knows
        // the difference between "busy just now" and "not mine at all", so only
        // the caller can filter it.
        let mut t = traffic();
        let now = Instant::now();
        t.note(&repo("held"), Why::Definite);
        assert_eq!(t.take_ready(now, |_| false), Vec::<RepoId>::new());
        assert!(!t.is_idle());

        // Still owed, and taken as soon as the caller can act.
        assert_eq!(t.take_ready(now, anything), vec![repo("held")]);
    }
}
