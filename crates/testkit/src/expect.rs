use git_scylla_core::{
    AheadBehind, FetchSchedule, Head, InProgress, ProbeOutcome, RepoKind, RepoSnapshot, WorkTree,
};
use serde_json::{json, Value};

/// What a fixture's snapshot must look like.
///
/// Omits `probed_at`, `last_fetch`, and the clock inside `FetchSchedule::Due`.
/// Every other field is asserted exactly. See [`normalize`].
#[derive(Debug, Clone)]
pub struct Expect {
    pub kind: RepoKind,
    pub head: Head,
    pub upstream: UpstreamExpect,
    /// Remote names, in config order. Every fixture remote is a local path,
    /// so hosts are not asserted here.
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
    /// No upstream configured. Distinct from `Sync(0, 0)`.
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
    /// A remote exists.
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

/// Project a real snapshot into the shape [`Expect::to_json`] produces.
///
/// Drops `probed_at` and `last_fetch`; collapses `FetchSchedule::Due(t)` to
/// `"due"`. Remote order is preserved and asserted.
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
