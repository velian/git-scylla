use git_scylla_core::{
    AheadBehind, FetchSchedule, Head, InProgress, ProbeOutcome, RepoKind, RepoSnapshot, WorkTree,
};
use serde_json::{json, Value};

/// What a fixture's snapshot must look like.
///
/// Not a bare `RepoSnapshot`, for one reason: three of a snapshot's fields
/// cannot be predicted by a generator — the probe time, the mtime behind
/// `last_fetch`, and the clock inside `FetchSchedule::Due`. Every other field is
/// asserted exactly. See [`normalize`], which is what makes the comparison a
/// JSON equality rather than a hand-written field-by-field walk.
#[derive(Debug, Clone)]
pub struct Expect {
    pub kind: RepoKind,
    pub head: Head,
    pub upstream: UpstreamExpect,
    /// Remote names, in config order. Hosts are asserted separately because
    /// every fixture remote is a local path and so has no host.
    pub remotes: Vec<String>,
    pub work: WorkTree,
    pub op: Option<InProgress>,
    pub stashes: u32,
    pub fetch: FetchExpect,
    pub outcome: ProbeOutcome,
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            kind: RepoKind::Normal,
            head: Head::Branch("main".into()),
            upstream: UpstreamExpect::None,
            remotes: Vec::new(),
            work: WorkTree::default(),
            op: None,
            stashes: 0,
            fetch: FetchExpect::Disabled,
            outcome: ProbeOutcome::Ok,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamExpect {
    /// No upstream configured. Distinct from `Sync(0, 0)` — see the note on
    /// [`git_scylla_core::Upstream::sync`].
    None,
    /// Configured, tracking ref resolvable.
    Sync { remote: &'static str, remote_ref: String, ahead: u32, behind: u32 },
    /// Configured, tracking ref deleted.
    Gone { remote: &'static str, remote_ref: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchExpect {
    /// No remote to fetch from.
    Disabled,
    /// A remote exists, so the fetch scheduler picks it up.
    Due,
}

impl Expect {
    /// Render the expectation in the same shape [`normalize`] produces.
    pub fn to_json(&self, name: &str) -> Value {
        json!({
            "name": name,
            "kind": self.kind,
            "head": self.head,
            "upstream": match &self.upstream {
                UpstreamExpect::None => Value::Null,
                UpstreamExpect::Sync { remote, remote_ref, ahead, behind } => json!({
                    "remote": remote,
                    "remote_ref": remote_ref,
                    "sync": AheadBehind { ahead: *ahead, behind: *behind },
                }),
                UpstreamExpect::Gone { remote, remote_ref } => json!({
                    "remote": remote,
                    "remote_ref": remote_ref,
                    "sync": Value::Null,
                }),
            },
            "remotes": self.remotes,
            "work": self.work,
            "op": self.op,
            "stashes": self.stashes,
            "fetch": match self.fetch {
                FetchExpect::Disabled => "disabled",
                FetchExpect::Due => "due",
            },
            "outcome": self.outcome,
        })
    }
}

/// Project a real snapshot into the comparable shape.
///
/// Everything volatile is dropped rather than approximated: `probed_at` is
/// gone, `last_fetch` is gone (its *presence* is not asserted, because
/// `FETCH_HEAD` exists or not depending on whether git chose to write it during
/// a clone, which is not our business), and `FetchSchedule::Due(t)` collapses to
/// `"due"`. What remains is asserted exactly, including remote *order*.
pub fn normalize(name: &str, s: &RepoSnapshot) -> Value {
    json!({
        "name": name,
        "kind": s.kind,
        "head": s.head,
        "upstream": s.upstream.as_ref().map(|u| json!({
            "remote": u.remote,
            "remote_ref": u.remote_ref,
            "sync": u.sync,
        })),
        "remotes": s.remotes.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
        "work": s.work,
        "op": s.op,
        "stashes": s.stashes,
        "fetch": match s.fetch.schedule {
            FetchSchedule::Disabled => "disabled",
            FetchSchedule::Due(_) => "due",
            FetchSchedule::BackingOff { .. } => "backing-off",
            FetchSchedule::Quarantined { .. } => "quarantined",
        },
        "outcome": s.outcome,
    })
}
