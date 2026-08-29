//! Turning "pull everything dirty" into a list the user can read and confirm.

use crate::policy::{evaluate, Eligibility, Policy};
use crate::Selection;
use git_scylla_core::{template, Action, PullMode, RepoId, RepoSnapshot, ResetMode, SkipReason};
use git_scylla_core::{undoability, Job, JobState, Undoable};
use git_scylla_probe::{RefAnswer, RefError, RefQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

pub type RefAnswers = HashMap<RepoId, Result<RefAnswer, RefError>>;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub action: Action,
    pub eligible: Vec<(RepoId, Action)>,
    pub skipped: Vec<(RepoId, SkipReason)>,
    pub considered: usize,
    pub warning: Option<String>,
}

impl<'de> Deserialize<'de> for Plan {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            action: Action,
            eligible: Vec<(RepoId, Action)>,
            skipped: Vec<(RepoId, SkipReason)>,
            considered: usize,
            warning: Option<String>,
        }
        let w = Wire::deserialize(d)?;
        if let Some((_, action)) = w.eligible.iter().find(|(_, a)| !a.is_resolved()) {
            return Err(serde::de::Error::custom(format!(
                "the plan carries an unresolved action ({action}); it would run no commands"
            )));
        }
        Ok(Plan {
            action: w.action,
            eligible: w.eligible,
            skipped: w.skipped,
            considered: w.considered,
            warning: w.warning,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PlanTemplate {
    plan: Plan,
    now: SystemTime,
    policy: Policy,
}

impl PlanTemplate {
    pub(crate) fn eligible(&self) -> &[(RepoId, Action)] {
        &self.plan.eligible
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipGroup {
    pub reason: SkipReason,
    pub repos: Vec<RepoId>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionVariant {
    pub action: Action,
    pub repos: Vec<RepoId>,
}

pub fn plan(
    action: &Action,
    snaps: &[RepoSnapshot],
    sel: &Selection,
    now: SystemTime,
    policy: &Policy,
) -> PlanTemplate {
    let now_for_template = now;
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();

    for snap in snaps {
        if !sel.contains(snap) {
            continue;
        }
        match evaluate(action, snap, now, policy) {
            Eligibility::Eligible => {
                eligible.push((snap.id.clone(), resolve_template(action, snap, now_for_template)))
            }
            Eligibility::Skip(why) => skipped.push((snap.id.clone(), why)),
        }
    }

    let warning = warn_about(action, snaps, &eligible);
    PlanTemplate {
        plan: Plan { action: action.clone(), eligible, skipped, considered: snaps.len(), warning },
        now,
        policy: policy.clone(),
    }
}

fn warn_about(
    action: &Action,
    snaps: &[RepoSnapshot],
    eligible: &[(RepoId, Action)],
) -> Option<String> {
    let chosen: std::collections::HashSet<&RepoId> = eligible.iter().map(|(id, _)| id).collect();
    let mine = || snaps.iter().filter(|s| chosen.contains(&s.id));
    match action {
        Action::Commit { stage_all: true, .. } => {
            let untracked: u32 = mine().map(|s| s.work.untracked).sum();
            (untracked > 0).then(|| {
                let file = if untracked == 1 { "file" } else { "files" };
                format!(
                    "`git add -A` will include {untracked} untracked {file}. \
                     `git commit -a` would not; this does."
                )
            })
        }
        Action::DevTag { .. } => {
            let dirty = mine().filter(|s| !s.is_clean()).count();
            (dirty > 0).then(|| {
                let has = if dirty == 1 { "has" } else { "have" };
                format!(
                    "{dirty} of these {has} uncommitted changes. A tag marks the \
                     commit at HEAD, not what is on disk."
                )
            })
        }
        _ => None,
    }
}

fn resolve_template(template: &Action, snap: &RepoSnapshot, now: SystemTime) -> Action {
    match template {
        Action::Push { set_upstream: Some(wanted), force_with_lease } => Action::Push {
            set_upstream: Some(preferred_remote(snap, wanted)),
            force_with_lease: *force_with_lease,
        },
        // Rendered here so the plan shows the actual messages, not the template.
        Action::Commit { message, stage_all, no_verify } => Action::Commit {
            message: template::render(message, snap, now),
            stage_all: *stage_all,
            no_verify: *no_verify,
        },
        Action::Checkout { rev, create } => {
            Action::Checkout { rev: template::render(rev, snap, now), create: *create }
        }
        Action::Branch { name, from } => {
            Action::Branch { name: template::render(name, snap, now), from: from.clone() }
        }
        Action::DevTag { channel, bump, name, push: Some(wanted) } => Action::DevTag {
            channel: channel.clone(),
            bump: *bump,
            name: name.clone(),
            push: Some(preferred_remote(snap, wanted)),
        },
        Action::Fetch { .. }
        | Action::Pull { .. }
        | Action::Push { set_upstream: None, .. }
        | Action::Stash { .. }
        | Action::StashPop
        | Action::Reset { .. }
        | Action::SyncDefault { .. }
        | Action::DevTag { push: None, .. }
        | Action::Custom { .. } => template.clone(),
    }
}

fn preferred_remote(snap: &RepoSnapshot, wanted: &str) -> String {
    let has = |name: &str| snap.remotes.iter().any(|r| r.name == name);
    if has(wanted) {
        return wanted.to_string();
    }
    if has("origin") {
        return "origin".to_string();
    }
    match snap.remotes.first() {
        Some(r) => r.name.clone(),
        None => wanted.to_string(),
    }
}

impl Plan {
    pub fn selected(&self) -> usize {
        self.eligible.len() + self.skipped.len()
    }

    pub fn is_empty(&self) -> bool {
        self.eligible.is_empty()
    }

    pub fn skip_groups(&self) -> Vec<SkipGroup> {
        let mut groups: Vec<SkipGroup> = Vec::new();
        for (id, reason) in &self.skipped {
            match groups.iter_mut().find(|g| &g.reason == reason) {
                Some(g) => g.repos.push(id.clone()),
                None => groups.push(SkipGroup { reason: reason.clone(), repos: vec![id.clone()] }),
            }
        }
        groups.sort_by(|a, b| {
            b.repos
                .len()
                .cmp(&a.repos.len())
                .then_with(|| a.reason.to_string().cmp(&b.reason.to_string()))
        });
        groups
    }

    pub fn action_variants(&self) -> Vec<ActionVariant> {
        let mut variants: Vec<ActionVariant> = Vec::new();
        for (id, action) in &self.eligible {
            match variants.iter_mut().find(|v| &v.action == action) {
                Some(v) => v.repos.push(id.clone()),
                None => {
                    variants.push(ActionVariant { action: action.clone(), repos: vec![id.clone()] })
                }
            }
        }
        variants.sort_by(|a, b| {
            b.repos
                .len()
                .cmp(&a.repos.len())
                .then_with(|| a.action.to_string().cmp(&b.action.to_string()))
        });
        variants
    }

    pub fn view(&self) -> PlanView {
        let w = words(&self.action);
        let eligible = (!self.eligible.is_empty()).then(|| PlanRow {
            count: self.eligible.len(),
            phrase: w.will.to_string(),
            detail: w.rationale.to_string(),
            repos: self.eligible.iter().map(|(id, _)| id.clone()).collect(),
        });
        let skips = self
            .skip_groups()
            .into_iter()
            .map(|g| PlanRow {
                count: g.repos.len(),
                phrase: "skipped".to_string(),
                detail: g.reason.to_string(),
                repos: g.repos,
            })
            .collect();

        let variants = match self.action_variants() {
            v if v.len() < 2 && !w.show_commands => Vec::new(),
            v => v
                .into_iter()
                .map(|v| PlanVariant {
                    command: v.action.to_string(),
                    label: match v.repos.as_slice() {
                        [only] => only.name().to_string(),
                        many => many.len().to_string(),
                    },
                    repos: v.repos,
                })
                .collect(),
        };

        PlanView {
            headline: header(&w, self.selected()),
            selection_note: (self.selected() < self.considered)
                .then(|| format!("{} of {} selected", self.selected(), self.considered)),
            variants_note: match variants.len() {
                0 => None,
                1 => Some("resolved to one command:".to_string()),
                n => Some(format!("resolved to {n} different commands:")),
            },
            eligible,
            skips,
            variants,
            confirm_label: (!self.is_empty()).then(|| header(&w, self.eligible.len())),
            confirm_guard: (!self.is_empty())
                .then(|| guard(&self.action, self.eligible.len()))
                .flatten(),
            warning: self.warning.clone(),
            empty_note: match (self.selected(), self.is_empty()) {
                (0, _) => Some("Nothing selected.".to_string()),
                (_, true) => {
                    Some("Nothing to do: no repository in the selection is eligible.".to_string())
                }
                (_, false) => None,
            },
        }
    }

    pub fn render(&self) -> String {
        self.view().render()
    }
}

pub fn undo(
    jobs: &[Job],
    snaps: &[RepoSnapshot],
    now: SystemTime,
    policy: &Policy,
) -> PlanTemplate {
    let by_id: std::collections::HashMap<&RepoId, &RepoSnapshot> =
        snaps.iter().map(|s| (&s.id, s)).collect();
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();

    for job in jobs {
        let repo = job.repo.clone();
        let how = undoability(&job.action);
        if let Undoable::No(why) = how {
            skipped.push((repo, SkipReason::NotUndoable(why.to_string())));
            continue;
        }
        if job.state != JobState::Ok {
            skipped.push((repo, SkipReason::NotUndoable("the job did not run".into())));
            continue;
        }
        let repair = match how {
            Undoable::Switch => match &job.branch_before {
                Some(branch) => Some(Action::Checkout { rev: branch.clone(), create: false }),
                None => {
                    skipped.push((
                        repo.clone(),
                        SkipReason::NotUndoable("it was on a detached HEAD, not a branch".into()),
                    ));
                    continue;
                }
            },
            Undoable::Reset(mode) => job.head_before.clone().map(|to| Action::Reset { to, mode }),
            Undoable::No(_) => unreachable!("refused above"),
        };
        let Some(action) = repair else {
            skipped.push((repo, SkipReason::NotUndoable("no recorded commit to return to".into())));
            continue;
        };
        let Some(snap) = by_id.get(&job.repo) else {
            skipped.push((repo, SkipReason::SnapshotStale));
            continue;
        };
        if job.head_after.is_some() && snap.head_oid != job.head_after {
            skipped.push((repo, SkipReason::HeadMoved));
            continue;
        }
        match evaluate(&action, snap, now, policy) {
            Eligibility::Eligible => eligible.push((repo, action)),
            Eligibility::Skip(why) => skipped.push((repo, why)),
        }
    }

    let considered = jobs.len();
    let template = eligible.iter().map(|(_, a)| a.clone()).next().unwrap_or(Action::Reset {
        to: git_scylla_core::Oid::parse("0000000").expect("static oid"),
        mode: ResetMode::Hard,
    });
    PlanTemplate {
        plan: Plan { action: template, eligible, skipped, considered, warning: None },
        now,
        policy: policy.clone(),
    }
}

pub(crate) fn no_undo(now: SystemTime, policy: &Policy) -> PlanTemplate {
    PlanTemplate {
        plan: Plan {
            action: Action::Reset {
                to: git_scylla_core::Oid::parse("0000000").expect("static oid"),
                mode: ResetMode::Hard,
            },
            eligible: Vec::new(),
            skipped: Vec::new(),
            considered: 0,
            warning: None,
        },
        now,
        policy: policy.clone(),
    }
}

pub(crate) fn queries_for(t: &PlanTemplate) -> Vec<(RefQuery, Vec<RepoId>)> {
    let mut groups: Vec<(RefQuery, Vec<RepoId>)> = Vec::new();
    for (id, action) in t.eligible() {
        let Some(query) = query_of(action) else { continue };
        match groups.iter_mut().find(|(q, _)| q == &query) {
            Some((_, ids)) => ids.push(id.clone()),
            None => groups.push((query, vec![id.clone()])),
        }
    }
    groups
}

fn query_of(action: &Action) -> Option<RefQuery> {
    match action {
        // `create: true` makes the ref rather than requiring it.
        Action::Checkout { rev, create: false } => Some(RefQuery::Exists { rev: rev.clone() }),
        Action::SyncDefault { .. } => Some(RefQuery::DefaultBranch),
        Action::DevTag { .. } => Some(RefQuery::Tags),
        Action::Checkout { create: true, .. }
        | Action::Fetch { .. }
        | Action::Pull { .. }
        | Action::Push { .. }
        | Action::Commit { .. }
        | Action::Stash { .. }
        | Action::StashPop
        | Action::Branch { .. }
        | Action::Reset { .. }
        | Action::Custom { .. } => None,
    }
}

pub fn resolve(t: PlanTemplate, snaps: &[RepoSnapshot], answers: &RefAnswers) -> Plan {
    let by_id: HashMap<&RepoId, &RepoSnapshot> = snaps.iter().map(|s| (&s.id, s)).collect();
    let PlanTemplate { plan: mut p, now, policy } = t;
    let mut kept = Vec::with_capacity(p.eligible.len());
    for (id, action) in std::mem::take(&mut p.eligible) {
        let action = match finish(&id, action, &by_id, answers) {
            Ok(action) => action,
            Err(why) => {
                p.skipped.push((id, why));
                continue;
            }
        };
        let Some(snap) = by_id.get(&id) else {
            p.skipped.push((id, SkipReason::SnapshotStale));
            continue;
        };
        match evaluate(&action, snap, now, &policy) {
            Eligibility::Eligible => kept.push((id, action)),
            Eligibility::Skip(why) => p.skipped.push((id, why)),
        }
    }
    p.eligible = kept;
    p
}

fn finish(
    id: &RepoId,
    action: Action,
    by_id: &HashMap<&RepoId, &RepoSnapshot>,
    answers: &RefAnswers,
) -> Result<Action, SkipReason> {
    match action {
        Action::Checkout { ref rev, create: false } => match answers.get(id) {
            Some(Ok(RefAnswer::Exists(Some(false)))) => Err(SkipReason::RefNotFound(rev.clone())),
            None => Err(SkipReason::SnapshotStale),
            _ => Ok(action),
        },

        Action::SyncDefault { mode, plan: None } => {
            let Some(snap) = by_id.get(id) else { return Err(SkipReason::SnapshotStale) };
            let default = match answers.get(id) {
                Some(Ok(RefAnswer::DefaultBranch(Some(name)))) => name.clone(),
                Some(Ok(RefAnswer::DefaultBranch(None))) => {
                    return Err(SkipReason::NoDefaultBranch)
                }
                _ => return Err(SkipReason::SnapshotStale),
            };
            let Some(back_to) = snap.branch().map(str::to_string) else {
                return Err(SkipReason::DetachedHead);
            };
            let sync = git_scylla_core::SyncPlan {
                default,
                back_to,
                stash: snap.work.staged > 0 || snap.work.modified > 0,
            };
            Ok(Action::SyncDefault { mode, plan: Some(sync) })
        }

        Action::DevTag { channel, bump, name: None, push } => {
            let Some(Ok(RefAnswer::Tags(have))) = answers.get(id) else {
                return Err(SkipReason::SnapshotStale);
            };
            let name = git_scylla_core::version::next_dev_tag(have, &channel, bump);
            Ok(Action::DevTag { channel, bump, name: Some(name), push })
        }
        _ => Ok(action),
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    pub headline: String,
    pub selection_note: Option<String>,
    pub variants_note: Option<String>,
    pub eligible: Option<PlanRow>,
    pub skips: Vec<PlanRow>,
    pub variants: Vec<PlanVariant>,
    pub confirm_label: Option<String>,
    pub confirm_guard: Option<ConfirmGuard>,
    pub empty_note: Option<String>,
    pub warning: Option<String>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum ConfirmGuard {
    TypeCount(usize),
    Acknowledge(String),
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRow {
    pub count: usize,
    pub phrase: String,
    pub detail: String,
    pub repos: Vec<RepoId>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanVariant {
    pub command: String,
    pub label: String,
    pub repos: Vec<RepoId>,
}

impl PlanView {
    pub fn render(&self) -> String {
        let rows: Vec<(char, &PlanRow)> = self
            .eligible
            .iter()
            .map(|r| ('\u{2713}', r))
            .chain(self.skips.iter().map(|r| ('\u{23ed}', r)))
            .collect();

        let count_w = rows.iter().map(|(_, r)| r.count.to_string().len()).max().unwrap_or(1);
        const PHRASE_MIN: usize = 15;
        let phrase_w = rows.iter().map(|(_, r)| r.phrase.len()).max().unwrap_or(0).max(PHRASE_MIN);

        let mut out = String::new();
        out.push_str(&self.headline);
        if let Some(note) = &self.selection_note {
            out.push_str(&format!(" \u{2014} {note}"));
        }
        out.push('\n');

        if rows.is_empty() {
            out.push_str("  nothing selected\n");
            return out;
        }
        for (marker, row) in rows {
            let (count, phrase, detail) = (row.count, &row.phrase, &row.detail);
            out.push_str(&format!("  {marker} {count:>count_w$} {phrase:<phrase_w$}  {detail}\n"));
        }
        if let Some(note) = &self.variants_note {
            out.push_str(&format!("\n  {note}\n"));
            // Right-align so commands line up regardless of label width.
            let w = self.variants.iter().map(|v| v.label.len()).max().unwrap_or(4).max(4);
            for v in &self.variants {
                out.push_str(&format!("    {:>w$}  {}\n", v.label, v.command));
            }
        }
        if let Some(warning) = &self.warning {
            out.push_str(&format!("\n  ! {warning}\n"));
        }
        if let Some(note) = &self.empty_note {
            // The other empty case: repositories were selected, none qualified.
            out.push_str(&format!("\n{note}\n"));
        }
        out
    }
}

struct Words {
    verb: &'static str,
    qualifier: Option<String>,
    will: &'static str,
    rationale: &'static str,
    show_commands: bool,
}

fn words(action: &Action) -> Words {
    match action {
        Action::Fetch { prune, tags } => Words {
            verb: "Fetch",
            qualifier: match (prune, tags) {
                (true, true) => Some("prune, tags".into()),
                (true, false) => Some("prune".into()),
                (false, true) => Some("tags".into()),
                (false, false) => None,
            },
            will: "will fetch",
            rationale: "has a remote",
            show_commands: false,
        },
        Action::Pull { mode } => Words {
            verb: "Pull",
            qualifier: Some(mode.to_string()),
            will: "will pull",
            rationale: match mode {
                PullMode::FfOnly => "behind, not ahead, clean, upstream present",
                PullMode::Rebase | PullMode::Merge => "behind, clean, upstream present",
            },
            show_commands: false,
        },
        Action::Push { set_upstream, force_with_lease } => Words {
            verb: "Push",
            qualifier: match (set_upstream, force_with_lease) {
                (_, true) => Some("force-with-lease".into()),
                (Some(r), false) => Some(format!("set upstream to {r}")),
                (None, false) => None,
            },
            will: "will push",
            rationale: match force_with_lease {
                true => "ahead, recent fetch",
                false => "ahead, upstream present",
            },
            show_commands: false,
        },
        Action::Checkout { rev, create } => Words {
            verb: match create {
                true => "Create branch on",
                false => "Check out on",
            },
            qualifier: Some(rev.clone()),
            will: "will check out",
            rationale: "clean worktree",
            show_commands: false,
        },
        Action::Commit { stage_all, no_verify, .. } => Words {
            verb: "Commit in",
            qualifier: match (stage_all, no_verify) {
                (true, true) => Some("stage all, including untracked; no hooks".into()),
                (true, false) => Some("stage all, including untracked".into()),
                (false, true) => Some("no hooks".into()),
                (false, false) => None,
            },
            will: "will commit",
            rationale: "something to commit",
            show_commands: false,
        },
        Action::Stash { include_untracked } => Words {
            verb: "Stash in",
            qualifier: include_untracked.then(|| "including untracked".to_string()),
            will: "will stash",
            rationale: "something to stash",
            show_commands: false,
        },
        Action::StashPop => Words {
            verb: "Pop stash in",
            qualifier: None,
            will: "will pop",
            rationale: "has a stash, no conflicts",
            show_commands: false,
        },
        Action::Branch { name, from } => Words {
            verb: "Branch in",
            qualifier: Some(match from {
                Some(from) => format!("{name} from {from}"),
                None => name.clone(),
            }),
            will: "will branch",
            rationale: "has a commit to branch from",
            show_commands: false,
        },
        Action::Reset { to, mode } => Words {
            verb: "Undo in",
            qualifier: Some(match mode {
                ResetMode::Hard => "reset --hard; discards uncommitted work".into(),
                ResetMode::Soft => format!("reset --soft to {}", to.short()),
            }),
            will: "will reset",
            rationale: "clean, and HEAD is still where the job left it",
            show_commands: false,
        },
        Action::SyncDefault { mode, .. } => Words {
            verb: "Sync default branch in",
            qualifier: Some(mode.to_string()),
            will: "will sync",
            rationale: "on a branch, has a remote; work in the way is stashed and put back",
            show_commands: true,
        },
        Action::DevTag { channel, bump, push, .. } => Words {
            verb: "Cut a tag in",
            qualifier: Some(match push {
                Some(remote) => format!("{channel}, {bump} bump, push to {remote}"),
                None => format!("{channel}, {bump} bump, local only"),
            }),
            will: "will be tagged",
            show_commands: true,
            rationale: "has a commit to tag",
        },
        Action::Custom { args, .. } => Words {
            verb: "Run in",
            qualifier: Some(format!("git {}", args.join(" "))),
            will: "will run",
            // Honest: no precondition can be checked for an arbitrary command.
            rationale: "no preconditions apply",
            show_commands: false,
        },
    }
}

fn guard(action: &Action, count: usize) -> Option<ConfirmGuard> {
    match action {
        Action::Push { force_with_lease: true, .. } => Some(ConfirmGuard::TypeCount(count)),
        Action::Custom { .. } => Some(ConfirmGuard::Acknowledge(
            "preconditions and undo do not apply to a custom command".into(),
        )),
        _ => None,
    }
}

fn header(w: &Words, count: usize) -> String {
    let repos = if count == 1 { "repo" } else { "repos" };
    match &w.qualifier {
        Some(q) => format!("{} {count} {repos} ({q})", w.verb),
        None => format!("{} {count} {repos}", w.verb),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_scylla_core::{AheadBehind, FetchHealth, Head, InProgress, Oid, Remote, Upstream};
    use std::time::{Duration, UNIX_EPOCH};

    const NOW: SystemTime = UNIX_EPOCH;

    fn base(name: &str) -> RepoSnapshot {
        let mut s = RepoSnapshot::stub(format!("/r/{name}"));
        s.remotes = vec![Remote { name: "origin".into(), host: None }];
        s.fetch = FetchHealth::due_now(NOW);
        s
    }

    fn tracked(name: &str, ahead: u32, behind: u32) -> RepoSnapshot {
        let mut s = base(name);
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead, behind }),
            last_fetch: Some(NOW),
        });
        s
    }

    fn plan_all(action: &Action, snaps: &[RepoSnapshot]) -> Plan {
        plan(action, snaps, &Selection::All, NOW, &Policy::default()).plan
    }

    fn answered(id: &RepoId, a: Option<Result<RefAnswer, RefError>>) -> RefAnswers {
        let mut m = RefAnswers::new();
        if let Some(a) = a {
            m.insert(id.clone(), a);
        }
        m
    }

    fn one(
        action: Action,
        snap: RepoSnapshot,
        answer: Option<Result<RefAnswer, RefError>>,
    ) -> Result<Action, SkipReason> {
        let snaps = vec![snap];
        let t = plan(&action, &snaps, &Selection::All, NOW, &Policy::default());
        assert_eq!(t.eligible().len(), 1, "the fixture must survive the first gate");
        let answers = answered(&snaps[0].id, answer);
        let p = resolve(t, &snaps, &answers);
        match (p.eligible.first(), p.skipped.first()) {
            (Some((_, a)), None) => Ok(a.clone()),
            (None, Some((_, why))) => Err(why.clone()),
            other => panic!("expected exactly one outcome, got {other:?}"),
        }
    }

    fn unreadable() -> RefError {
        RefError::Unreadable {
            path: "/r/r/.git".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        }
    }

    fn on_branch(name: &str, branch: &str) -> RepoSnapshot {
        let mut s = tracked(name, 0, 0);
        s.head = Head::Branch(branch.into());
        s
    }

    #[test]
    fn a_checkout_is_refused_only_by_a_definite_no() {
        let action = || Action::Checkout { rev: "release".into(), create: false };
        let snap = || on_branch("r", "main");

        assert_eq!(
            one(action(), snap(), Some(Ok(RefAnswer::Exists(Some(false))))),
            Err(SkipReason::RefNotFound("release".into()))
        );
        assert!(one(action(), snap(), Some(Ok(RefAnswer::Exists(Some(true))))).is_ok());
        assert!(one(action(), snap(), Some(Ok(RefAnswer::Exists(None)))).is_ok());
        assert!(one(action(), snap(), Some(Err(unreadable()))).is_ok());
    }

    #[test]
    fn a_row_that_needed_an_answer_and_got_none_is_stale_not_refused() {
        assert_eq!(
            one(
                Action::Checkout { rev: "release".into(), create: false },
                on_branch("r", "main"),
                None
            ),
            Err(SkipReason::SnapshotStale)
        );
        assert_eq!(
            one(
                Action::SyncDefault { mode: PullMode::FfOnly, plan: None },
                on_branch("r", "wip"),
                None
            ),
            Err(SkipReason::SnapshotStale)
        );
        assert_eq!(
            one(dev_tag_template(), on_branch("r", "main"), None),
            Err(SkipReason::SnapshotStale)
        );
    }

    fn dev_tag_template() -> Action {
        Action::DevTag {
            channel: "dev".into(),
            bump: git_scylla_core::version::Bump::Minor,
            name: None,
            push: None,
        }
    }

    #[test]
    fn no_trunk_and_an_unreadable_repository_are_different_skips() {
        let sync = || Action::SyncDefault { mode: PullMode::FfOnly, plan: None };
        assert_eq!(
            one(sync(), on_branch("r", "wip"), Some(Ok(RefAnswer::DefaultBranch(None)))),
            Err(SkipReason::NoDefaultBranch)
        );
        assert_eq!(
            one(sync(), on_branch("r", "wip"), Some(Err(unreadable()))),
            Err(SkipReason::SnapshotStale)
        );
    }

    #[test]
    fn a_sync_off_trunk_stashes_only_tracked_work_and_comes_back() {
        let mut snap = on_branch("r", "wip");
        snap.work.modified = 2;
        snap.work.untracked = 5;
        let resolved = one(
            Action::SyncDefault { mode: PullMode::FfOnly, plan: None },
            snap,
            Some(Ok(RefAnswer::DefaultBranch(Some("main".into())))),
        )
        .expect("off trunk, so the action stashes rather than refusing");
        let Action::SyncDefault { plan: Some(p), .. } = resolved else { panic!("unresolved") };
        assert_eq!(p.default, "main");
        assert_eq!(p.back_to, "wip");
        assert!(p.stash, "modified files are in the way of the switch");
    }

    #[test]
    fn a_sync_already_on_trunk_with_a_dirty_tree_is_a_plain_pull_and_refused() {
        let mut snap = on_branch("r", "main");
        snap.work.modified = 1;
        assert_eq!(
            one(
                Action::SyncDefault { mode: PullMode::FfOnly, plan: None },
                snap,
                Some(Ok(RefAnswer::DefaultBranch(Some("main".into())))),
            ),
            Err(SkipReason::DirtyWorktree)
        );
    }

    #[test]
    fn a_dev_tag_name_comes_from_this_repositorys_own_tags() {
        let resolved = one(
            dev_tag_template(),
            on_branch("r", "main"),
            Some(Ok(RefAnswer::Tags(vec!["v1.2.0".into(), "v1.3.0-dev.1".into()]))),
        )
        .expect("tags answered");
        let Action::DevTag { name: Some(name), .. } = resolved else { panic!("unresolved") };
        assert_eq!(name, "v1.3.0-dev.2");
    }

    #[test]
    fn an_unreadable_repository_is_never_given_the_first_tag_in_a_series() {
        assert_eq!(
            one(dev_tag_template(), on_branch("r", "main"), Some(Err(unreadable()))),
            Err(SkipReason::SnapshotStale)
        );
    }

    #[test]
    fn an_action_needing_no_cold_facts_asks_nothing() {
        let snaps = vec![tracked("a", 0, 3), tracked("b", 0, 3)];
        let t = plan(
            &Action::Pull { mode: PullMode::Rebase },
            &snaps,
            &Selection::All,
            NOW,
            &Policy::default(),
        );
        assert!(queries_for(&t).is_empty());
    }

    #[test]
    fn one_question_of_everyone_is_one_group() {
        let snaps = vec![on_branch("a", "main"), on_branch("b", "main")];
        let t = plan(
            &Action::Checkout { rev: "release".into(), create: false },
            &snaps,
            &Selection::All,
            NOW,
            &Policy::default(),
        );
        let groups = queries_for(&t);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, RefQuery::Exists { rev: "release".into() });
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn a_placeholder_rev_asks_each_repository_about_its_own_branch() {
        let snaps = vec![on_branch("a", "main"), on_branch("b", "main")];
        let t = plan(
            &Action::Checkout { rev: "release/{repo}".into(), create: false },
            &snaps,
            &Selection::All,
            NOW,
            &Policy::default(),
        );
        let mut asked: Vec<String> = queries_for(&t)
            .into_iter()
            .map(|(q, ids)| {
                assert_eq!(ids.len(), 1, "each repository asks about itself");
                match q {
                    RefQuery::Exists { rev } => rev,
                    other => panic!("{other:?}"),
                }
            })
            .collect();
        asked.sort();
        assert_eq!(asked, vec!["release/a".to_string(), "release/b".to_string()]);
    }

    #[test]
    fn a_plan_carrying_a_template_is_refused_at_the_wire() {
        let snaps = vec![on_branch("r", "wip")];
        let t = plan(
            &Action::SyncDefault { mode: PullMode::FfOnly, plan: None },
            &snaps,
            &Selection::All,
            NOW,
            &Policy::default(),
        );
        let json = serde_json::to_string(&t.plan).unwrap();
        let err = serde_json::from_str::<Plan>(&json).unwrap_err().to_string();
        assert!(err.contains("unresolved"), "{err}");

        let answers =
            answered(&snaps[0].id, Some(Ok(RefAnswer::DefaultBranch(Some("main".into())))));
        let p = resolve(t, &snaps, &answers);
        let back: Plan = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    fn worked_example() -> Vec<RepoSnapshot> {
        let mut snaps = Vec::new();
        for i in 0..31 {
            snaps.push(tracked(&format!("behind{i}"), 0, 3));
        }
        for i in 0..9 {
            snaps.push(tracked(&format!("insync{i}"), 0, 0));
        }
        for i in 0..4 {
            snaps.push(base(&format!("noup{i}")));
        }
        for i in 0..2 {
            let mut s = tracked(&format!("dirty{i}"), 0, 3);
            s.work.modified = 1;
            snaps.push(s);
        }
        let mut r = tracked("rebasing", 0, 3);
        r.op = Some(InProgress::Rebase);
        r.head = Head::Detached(Oid::parse("deadbeef").unwrap());
        snaps.push(r);
        snaps
    }

    #[test]
    fn the_worked_example_accounts_for_every_repository() {
        let p = plan_all(&Action::Pull { mode: PullMode::Rebase }, &worked_example());
        assert_eq!(p.selected(), 47);
        assert_eq!(p.eligible.len(), 31);
        assert_eq!(p.eligible.len() + p.skipped.len(), 47);

        let groups = p.skip_groups();
        let counted: Vec<(usize, String)> =
            groups.iter().map(|g| (g.repos.len(), g.reason.to_string())).collect();
        assert_eq!(
            counted,
            vec![
                (9, "already up to date".to_string()),
                (4, "no upstream configured".to_string()),
                (2, "uncommitted changes".to_string()),
                (1, "rebase in progress".to_string()),
            ],
            "groups must be descending by count"
        );
    }

    #[test]
    fn rendering_matches_the_documented_shape() {
        let p = plan_all(&Action::Pull { mode: PullMode::Rebase }, &worked_example());
        let out = p.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "Pull 47 repos (rebase)");
        assert_eq!(lines[1], "  \u{2713} 31 will pull        behind, clean, upstream present");
        assert_eq!(lines[2], "  \u{23ed}  9 skipped          already up to date");
        assert_eq!(lines[3], "  \u{23ed}  4 skipped          no upstream configured");
        assert_eq!(lines[4], "  \u{23ed}  2 skipped          uncommitted changes");
        assert_eq!(lines[5], "  \u{23ed}  1 skipped          rebase in progress");
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn counts_are_right_aligned_so_the_column_reads_as_numbers() {
        let mut snaps = vec![tracked("a", 0, 1)];
        for i in 0..12 {
            snaps.push(tracked(&format!("s{i}"), 0, 0));
        }
        let out = plan_all(&Action::Pull { mode: PullMode::Rebase }, &snaps).render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("\u{2713}  1 "), "{:?}", lines[1]);
        assert!(lines[2].contains("\u{23ed} 12 "), "{:?}", lines[2]);
    }

    #[test]
    fn an_empty_plan_says_so_rather_than_offering_nothing() {
        let snaps: Vec<RepoSnapshot> = (0..3).map(|i| tracked(&format!("s{i}"), 0, 0)).collect();
        let p = plan_all(&Action::Pull { mode: PullMode::FfOnly }, &snaps);
        assert!(p.is_empty());
        let out = p.render();
        assert!(out.contains("already up to date"));
        assert!(out.contains("Nothing to do"), "{out}");
    }

    #[test]
    fn a_plan_over_no_repositories_at_all_does_not_panic() {
        let p = plan_all(&Action::Fetch { prune: false, tags: false }, &[]);
        assert!(p.is_empty());
        assert_eq!(p.selected(), 0);
        assert!(p.render().contains("nothing selected"));
    }

    #[test]
    fn the_header_notes_when_the_selection_narrowed_the_set() {
        let snaps = vec![tracked("a", 0, 3), tracked("b", 0, 3)];
        let sel = Selection::ids([snaps[0].id.clone()]);
        let p =
            plan(&Action::Pull { mode: PullMode::Rebase }, &snaps, &sel, NOW, &Policy::default())
                .plan;
        assert_eq!(p.selected(), 1);
        assert_eq!(p.considered, 2);
        assert!(p.render().starts_with("Pull 1 repo (rebase) — 1 of 2 selected"), "{}", p.render());
    }

    #[test]
    fn unselected_repositories_are_not_reported_as_skips() {
        let snaps: Vec<RepoSnapshot> = (0..50).map(|i| tracked(&format!("s{i}"), 0, 3)).collect();
        let sel = Selection::ids([snaps[0].id.clone()]);
        let p =
            plan(&Action::Pull { mode: PullMode::Rebase }, &snaps, &sel, NOW, &Policy::default())
                .plan;
        assert_eq!(p.skipped.len(), 0);
        assert_eq!(p.eligible.len(), 1);
        assert!(!p.skipped.iter().any(|(_, r)| matches!(r, SkipReason::NotSelected)));
    }

    #[test]
    fn eligible_entries_carry_a_resolved_action_not_the_template() {
        let template = Action::Pull { mode: PullMode::Rebase };
        let p = plan_all(&template, &[tracked("a", 0, 3)]);
        assert_eq!(p.eligible.len(), 1);
        assert_eq!(p.eligible[0].1, template);
        assert_eq!(p.action, template);
    }

    #[test]
    fn push_resolves_the_remote_per_repository() {
        let mut has_origin = tracked("a", 1, 0);
        has_origin.remotes = vec![Remote { name: "origin".into(), host: None }];

        let mut has_other = tracked("b", 1, 0);
        has_other.remotes = vec![Remote { name: "fork".into(), host: None }];

        let mut has_both = tracked("c", 1, 0);
        has_both.remotes = vec![
            Remote { name: "fork".into(), host: None },
            Remote { name: "origin".into(), host: None },
        ];

        let template =
            Action::Push { set_upstream: Some("origin".into()), force_with_lease: false };
        let p = plan_all(&template, &[has_origin, has_other, has_both]);
        let resolved: Vec<&Action> = p.eligible.iter().map(|(_, a)| a).collect();

        assert_eq!(resolved[0], &template, "has origin, so origin");
        assert_eq!(
            resolved[1],
            &Action::Push { set_upstream: Some("fork".into()), force_with_lease: false },
            "no origin, so its only remote"
        );
        assert_eq!(resolved[2], &template, "origin present among several, so origin");
    }

    #[test]
    fn a_plan_that_resolved_to_several_commands_reports_every_one() {
        let mut a = tracked("a", 1, 0);
        a.remotes = vec![Remote { name: "origin".into(), host: None }];
        let mut b = tracked("b", 1, 0);
        b.remotes = vec![Remote { name: "fork".into(), host: None }];

        let p = plan_all(
            &Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
            &[a, b],
        );
        let variants = p.action_variants();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].repos.len(), 1);
        assert_eq!(variants[1].repos.len(), 1);
        let commands: Vec<String> = variants.iter().map(|v| v.action.to_string()).collect();
        assert!(commands.iter().any(|c| c.contains("origin")), "{commands:?}");
        assert!(commands.iter().any(|c| c.contains("fork")), "{commands:?}");
    }

    #[test]
    fn one_resolved_command_is_one_variant() {
        let p = plan_all(
            &Action::Pull { mode: PullMode::Rebase },
            &[tracked("a", 0, 3), tracked("b", 0, 3)],
        );
        let variants = p.action_variants();
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].repos.len(), 2);
    }

    #[test]
    fn skip_groups_are_expandable_to_the_repository_list() {
        let mut snaps = vec![tracked("a", 0, 0), tracked("b", 0, 0)];
        snaps.push(base("c"));
        let p = plan_all(&Action::Pull { mode: PullMode::Rebase }, &snaps);
        let groups = p.skip_groups();
        let utd = groups.iter().find(|g| g.reason == SkipReason::UpToDate).unwrap();
        assert_eq!(utd.repos, vec![RepoId::from_canonical("/r/a"), RepoId::from_canonical("/r/b")]);
    }

    #[test]
    fn equal_counts_group_in_a_stable_order() {
        let mut dirty = tracked("dirty", 0, 3);
        dirty.work.modified = 1;
        let snaps = vec![tracked("utd", 0, 0), base("noup"), dirty];
        let first = plan_all(&Action::Pull { mode: PullMode::Rebase }, &snaps).skip_groups();
        for _ in 0..5 {
            let again = plan_all(&Action::Pull { mode: PullMode::Rebase }, &snaps).skip_groups();
            assert_eq!(first, again);
        }
        let reasons: Vec<String> = first.iter().map(|g| g.reason.to_string()).collect();
        assert_eq!(
            reasons,
            vec!["already up to date", "no upstream configured", "uncommitted changes"],
            "one each, so alphabetical by rendered reason"
        );
    }

    #[test]
    fn a_stale_snapshot_becomes_a_skip_rather_than_an_omission() {
        let mut old = tracked("old", 0, 3);
        old.probed_at = NOW;
        let policy = Policy { max_snapshot_age: Duration::from_secs(30), ..Default::default() };
        let p = plan(
            &Action::Pull { mode: PullMode::Rebase },
            std::slice::from_ref(&old),
            &Selection::All,
            NOW + Duration::from_secs(60),
            &policy,
        )
        .plan;
        assert_eq!(p.eligible.len(), 0);
        assert_eq!(p.skipped, vec![(old.id.clone(), SkipReason::SnapshotStale)]);
    }

    #[test]
    fn headers_name_what_is_about_to_happen() {
        let cases: Vec<(Action, &str)> = vec![
            (Action::Fetch { prune: false, tags: false }, "Fetch 0 repos"),
            (Action::Fetch { prune: true, tags: false }, "Fetch 0 repos (prune)"),
            (Action::Fetch { prune: true, tags: true }, "Fetch 0 repos (prune, tags)"),
            (Action::Pull { mode: PullMode::FfOnly }, "Pull 0 repos (ff-only)"),
            (
                Action::Push { set_upstream: None, force_with_lease: true },
                "Push 0 repos (force-with-lease)",
            ),
            (
                Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
                "Push 0 repos (set upstream to origin)",
            ),
            (
                Action::Commit { message: "m".into(), stage_all: true, no_verify: false },
                "Commit in 0 repos (stage all, including untracked)",
            ),
            (
                Action::Custom {
                    args: vec!["gc".into(), "--prune=now".into()],
                    network: true,
                    mutating: true,
                },
                "Run in 0 repos (git gc --prune=now)",
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(header(&words(&action), 0), expected);
        }
    }

    #[test]
    fn a_force_with_lease_batch_is_labelled_by_its_dangerous_half() {
        let both = Action::Push { set_upstream: Some("origin".into()), force_with_lease: true };
        assert_eq!(words(&both).qualifier.as_deref(), Some("force-with-lease"));
    }

    #[test]
    fn one_repository_is_not_pluralised() {
        assert_eq!(header(&words(&Action::Fetch { prune: false, tags: false }), 1), "Fetch 1 repo");
    }

    #[test]
    fn custom_admits_that_it_checked_nothing() {
        assert_eq!(
            words(&Action::Custom { args: vec![], network: true, mutating: true }).rationale,
            "no preconditions apply"
        );
    }

    #[test]
    fn planning_a_hundred_repositories_is_free() {
        let snaps: Vec<RepoSnapshot> =
            (0..100)
                .map(|i| {
                    if i % 3 == 0 {
                        tracked(&format!("s{i}"), 0, 3)
                    } else {
                        base(&format!("s{i}"))
                    }
                })
                .collect();
        let action = Action::Pull { mode: PullMode::Rebase };
        let sel = Selection::parse("branch:main", None).unwrap();
        let policy = Policy::default();

        let mut times = Vec::new();
        for _ in 0..21 {
            let start = std::time::Instant::now();
            let p = plan(&action, &snaps, &sel, NOW, &policy).plan;
            times.push(start.elapsed());
            assert_eq!(p.selected(), 100);
        }
        times.sort();
        let median = times[times.len() / 2];
        assert!(median < Duration::from_millis(5), "median {median:?}, target under 5 ms");
        eprintln!("plan over 100 repositories: median {median:?}");
    }
    #[test]
    fn the_confirm_label_counts_what_will_run_not_what_was_selected() {
        let v = plan_all(&Action::Pull { mode: PullMode::Rebase }, &worked_example()).view();
        assert_eq!(v.headline, "Pull 47 repos (rebase)");
        assert_eq!(v.confirm_label.as_deref(), Some("Pull 31 repos (rebase)"));
        assert_eq!(v.eligible.as_ref().unwrap().count, 31);
        assert_eq!(v.empty_note, None);
    }

    #[test]
    fn the_view_carries_the_repositories_behind_every_count() {
        let v = plan_all(&Action::Pull { mode: PullMode::Rebase }, &worked_example()).view();
        assert_eq!(v.eligible.as_ref().unwrap().repos.len(), 31);
        for row in &v.skips {
            assert_eq!(row.repos.len(), row.count, "{}: count without a list", row.detail);
        }
        let dirty = v.skips.iter().find(|r| r.detail == "uncommitted changes").unwrap();
        assert_eq!(
            dirty.repos,
            vec![RepoId::from_canonical("/r/dirty0"), RepoId::from_canonical("/r/dirty1")]
        );
    }

    #[test]
    fn an_empty_plan_offers_no_confirm_control_at_all() {
        let snaps = vec![tracked("a", 0, 0), tracked("b", 0, 0)];
        let v = plan_all(&Action::Pull { mode: PullMode::Rebase }, &snaps).view();
        assert_eq!(v.confirm_label, None);
        assert_eq!(v.eligible, None);
        assert_eq!(
            v.empty_note.as_deref(),
            Some("Nothing to do: no repository in the selection is eligible.")
        );
        assert_eq!(v.skips.len(), 1);
        assert_eq!(v.skips[0].detail, "already up to date");
    }

    #[test]
    fn an_empty_selection_says_so_rather_than_blaming_the_repositories() {
        let v = plan_all(&Action::Pull { mode: PullMode::Rebase }, &[]).view();
        assert_eq!(v.confirm_label, None);
        assert!(v.skips.is_empty());
        assert_eq!(v.empty_note.as_deref(), Some("Nothing selected."));
    }

    #[test]
    fn several_resolved_commands_reach_the_view_and_the_text_alike() {
        let mut a = tracked("a", 1, 0);
        a.remotes = vec![Remote { name: "origin".into(), host: None }];
        let mut b = tracked("b", 1, 0);
        b.remotes = vec![Remote { name: "fork".into(), host: None }];
        let p = plan_all(
            &Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
            &[a, b],
        );

        let v = p.view();
        assert_eq!(v.variants.len(), 2);
        assert!(v.variants.iter().any(|x| x.command.contains("fork")), "{:?}", v.variants);

        let text = p.render();
        assert!(text.contains("resolved to 2 different commands:"), "{text}");
        assert!(text.contains("fork"), "{text}");
    }

    #[test]
    fn a_command_with_one_repository_is_labelled_by_name_not_by_count() {
        let mut a = tracked("a", 1, 0);
        a.remotes = vec![Remote { name: "origin".into(), host: None }];
        let mut b = tracked("b", 1, 0);
        b.remotes = vec![Remote { name: "fork".into(), host: None }];
        let action = Action::Push { set_upstream: Some("origin".into()), force_with_lease: false };
        let view = plan_all(&action, &[a, b]).view();

        assert_eq!(view.variants.len(), 2);
        let labels: Vec<&str> = view.variants.iter().map(|v| v.label.as_str()).collect();
        assert!(labels.contains(&"a") && labels.contains(&"b"), "{labels:?}");
        assert!(view.render().contains("  a  git push"), "{}", view.render());
    }

    #[test]
    fn one_command_is_not_announced_in_the_plural() {
        let mut a = tracked("a", 1, 0);
        a.remotes = vec![Remote { name: "origin".into(), host: None }];
        let action = Action::SyncDefault { mode: PullMode::FfOnly, plan: None };
        let view = plan_all(&action, &[a]).view();
        assert_eq!(view.variants.len(), 1);
        assert_eq!(view.variants_note.as_deref(), Some("resolved to one command:"));
    }

    #[test]
    fn one_resolved_command_is_left_out_of_the_view_because_the_headline_has_it() {
        let snaps = vec![tracked("a", 1, 0), tracked("b", 1, 0)];
        let p = plan_all(&Action::Push { set_upstream: None, force_with_lease: false }, &snaps);
        assert_eq!(p.action_variants().len(), 1);
        assert!(p.view().variants.is_empty());
        assert!(!p.render().contains("resolved to"), "{}", p.render());
    }

    #[test]
    fn the_view_is_the_only_source_of_the_text() {
        let p = plan_all(&Action::Pull { mode: PullMode::Rebase }, &worked_example());
        let v = p.view();
        let text = p.render();

        let mut expected: Vec<String> = vec![v.headline.clone()];
        expected.extend(v.selection_note.clone());
        expected.extend(v.empty_note.clone());
        for row in v.eligible.iter().chain(v.skips.iter()) {
            expected.push(row.phrase.clone());
            expected.push(row.detail.clone());
        }
        for variant in &v.variants {
            expected.push(variant.command.clone());
        }
        for phrase in &expected {
            assert!(text.contains(phrase.as_str()), "{phrase:?} missing from:\n{text}");
        }

        let mut residue = text.clone();
        for phrase in &expected {
            residue = residue.replace(phrase.as_str(), "");
        }
        let leftover: String = residue
            .chars()
            .filter(|c| {
                !c.is_whitespace()
                    && !c.is_ascii_digit()
                    && !"\u{2713}\u{23ed}\u{2014}:".contains(*c)
            })
            .collect();
        assert!(leftover.is_empty(), "text says {leftover:?}, which the view never provided");
    }
}
