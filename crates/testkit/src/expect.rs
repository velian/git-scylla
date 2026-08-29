use git_scylla_core::{
    AheadBehind, FetchSchedule, Head, InProgress, ProbeOutcome, RepoKind, RepoSnapshot, WorkTree,
};
use serde_json::{json, Value};

/// What a fixture's snapshot must look like.
#[derive(Debug, Clone)]
pub struct Expect {
    pub kind: RepoKind,
    pub head: Head,
    pub upstream: UpstreamExpect,
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
    None,
    Sync { remote: &'static str, remote_ref: String, ahead: u32, behind: u32 },
    Gone { remote: &'static str, remote_ref: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchExpect {
    Disabled,
    Due,
}

impl Expect {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefExpect {
    pub default_branch: Option<String>,
    pub tags: Vec<String>,
    pub exists: Vec<(String, Option<bool>)>,
}

impl Default for RefExpect {
    fn default() -> Self {
        Self { default_branch: Some("main".into()), tags: Vec::new(), exists: Vec::new() }
    }
}

impl RefExpect {
    pub fn no_default_branch(mut self) -> Self {
        self.default_branch = None;
        self
    }

    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|t| (*t).to_string()).collect();
        self
    }

    pub fn exists(mut self, probes: &[(&str, Option<bool>)]) -> Self {
        self.exists = probes.iter().map(|(r, a)| ((*r).to_string(), *a)).collect();
        self
    }
}
