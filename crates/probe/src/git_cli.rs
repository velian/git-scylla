use crate::config::parse_remotes;
use crate::gitdir::{detect_in_progress, last_fetch, resolve_common_dir};
use crate::porcelain::{parse_porcelain_v2, PorcelainStatus};
use crate::{BoxFuture, Probe, ProbeRequest};
use git_scylla_core::{
    FetchHealth, Head, Oid, ProbeOutcome, RepoId, RepoKind, RepoSnapshot, Upstream,
};
use git_scylla_exec::{GitCommand, Stop};
use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

/// The single most important command in the project.
///
/// `--no-optional-locks` is **mandatory**: without it `git status` refreshes the
/// index and takes `index.lock`, contending with the user's own git usage. A
/// read-only monitoring tool that makes the user's `git rebase -i` fail is worse
/// than no tool.
const STATUS_ARGS: &[&str] = &[
    "--no-optional-locks",
    "status",
    "--porcelain=v2",
    "--branch",
    "--show-stash",
    "-z",
    "-unormal",
];

/// The production [`Probe`]: one `git status` plus a few file reads.
#[derive(Debug, Clone, Default)]
pub struct GitCliProbe {
    /// Environment overrides applied to every child.
    ///
    /// Empty in production: the probe must see the user's real configuration,
    /// because that is what their own `git` sees. Tests set
    /// `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` here so a developer's
    /// `~/.gitconfig` cannot change an assertion.
    extra_env: Vec<(OsString, OsString)>,
}

impl GitCliProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// A probe insulated from the ambient git configuration. For tests.
    pub fn hermetic() -> Self {
        Self {
            extra_env: vec![
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
        }
    }

    /// The status invocation, built through `crates/exec` like every other
    /// spawned `git`.
    ///
    /// There is deliberately no second way to start a subprocess here: the
    /// environment hardening, the process group and the deadline are guarantees
    /// only if nothing can opt out of them.
    fn status_command(&self, path: &Path) -> GitCommand {
        let cmd = GitCommand::new(path).args(STATUS_ARGS).envs(&self.extra_env);
        // Belt and braces with `--no-optional-locks`: neither the probe nor
        // anything it spawns may take `index.lock`. Not part of the hardening
        // table, because a *job* — commit, checkout — genuinely needs that lock.
        cmd.env("GIT_OPTIONAL_LOCKS", "0")
    }

    async fn probe_inner(&self, req: ProbeRequest) -> RepoSnapshot {
        let found = &req.found;
        let at = SystemTime::now();
        // Discovery already resolved this, and re-deriving it would be worse
        // than redundant: a second canonicalization can disagree with the first
        // if the path vanished in between, and a caller keyed on the first id
        // would then never see this snapshot.
        let id = found.id.clone();

        let common_dir = resolve_common_dir(&found.git_dir);
        let remotes = parse_remotes(&common_dir.join("config"));

        // A bare repository has no worktree, so `git status` is meaningless
        // there. Head and upstream still are.
        if matches!(found.kind, RepoKind::Bare) {
            return self.probe_bare(id, found.kind.clone(), &common_dir, remotes, at, &req).await;
        }

        let status = match self.status_command(&found.path).capture(req.deadline).await {
            Ok(out) if out.success() => parse_porcelain_v2(&out.stdout),
            Ok(out) if out.timed_out() => {
                return RepoSnapshot::failed(id, found.kind.clone(), at, ProbeOutcome::Timeout)
            }
            Ok(out) => {
                let msg = if out.stderr.is_empty() {
                    match out.stop {
                        Stop::Cancelled => "cancelled".to_string(),
                        _ => format!("git status exited {}", out.code.map_or(-1, |c| c)),
                    }
                } else {
                    out.stderr
                };
                return RepoSnapshot::failed(id, found.kind.clone(), at, ProbeOutcome::Error(msg));
            }
            Err(e) => {
                return RepoSnapshot::failed(
                    id,
                    found.kind.clone(),
                    at,
                    ProbeOutcome::Error(e.to_string()),
                )
            }
        };

        let head = head_from(&status);
        // Undo needs this: deciding whether somebody committed on top of a
        // job's result means comparing where HEAD is now against where the job
        // left it.
        let head_oid = status.oid.as_deref().and_then(|o| Oid::parse(o).ok());
        let upstream = upstream_from(&status, &remotes, &common_dir);
        let fetch = initial_fetch_health(&remotes, at);

        RepoSnapshot {
            path: id.path().to_path_buf(),
            id,
            kind: found.kind.clone(),
            head,
            head_oid,
            upstream,
            remotes,
            work: status.work,
            // Per-worktree state, so the *found* git dir and not the common one.
            op: detect_in_progress(&found.git_dir),
            stashes: status.stashes,
            fetch,
            probed_at: at,
            outcome: ProbeOutcome::Ok,
            from_cache: false,
            watched: false,
        }
    }

    /// Bare repositories: head from `HEAD`, no worktree, no stashes.
    ///
    /// `git status` is skipped entirely rather than run and discarded — in a bare
    /// repository it fails, and a failure we induced ourselves must not present
    /// as a broken repository.
    async fn probe_bare(
        &self,
        id: RepoId,
        kind: RepoKind,
        common_dir: &Path,
        remotes: Vec<git_scylla_core::Remote>,
        at: SystemTime,
        _req: &ProbeRequest,
    ) -> RepoSnapshot {
        let head = read_bare_head(common_dir).unwrap_or(Head::Unborn("HEAD".into()));
        RepoSnapshot {
            path: id.path().to_path_buf(),
            id,
            kind,
            head_oid: match &head {
                git_scylla_core::Head::Detached(oid) => Some(oid.clone()),
                _ => None,
            },
            head,
            // A bare repository has no checked-out branch, so nothing tracks an
            // upstream. Reporting `ahead: 0, behind: 0` here would be an
            // invention.
            upstream: None,
            work: git_scylla_core::WorkTree::default(),
            op: None,
            stashes: 0,
            fetch: initial_fetch_health(&remotes, at),
            remotes,
            probed_at: at,
            outcome: ProbeOutcome::Ok,
            from_cache: false,
            watched: false,
        }
    }
}

impl Probe for GitCliProbe {
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot> {
        Box::pin(self.probe_inner(req))
    }
}

fn head_from(status: &PorcelainStatus) -> Head {
    match (&status.branch, &status.oid) {
        // A branch name with no oid is an unborn HEAD: `git init` and nothing
        // committed. The branch name is real information even though no commit
        // carries it.
        (Some(b), None) => Head::Unborn(b.clone()),
        (Some(b), Some(_)) => Head::Branch(b.clone()),
        (None, Some(oid)) => match Oid::parse(oid) {
            Ok(o) => Head::Detached(o),
            Err(_) => Head::Unborn("HEAD".into()),
        },
        (None, None) => Head::Unborn("HEAD".into()),
    }
}

fn upstream_from(
    status: &PorcelainStatus,
    remotes: &[git_scylla_core::Remote],
    common_dir: &Path,
) -> Option<Upstream> {
    let remote_ref = status.upstream.clone()?;
    Some(Upstream {
        remote: split_remote(&remote_ref, remotes),
        remote_ref,
        // `None` when git omitted `# branch.ab`, i.e. the tracking ref is gone.
        sync: status.ab,
        last_fetch: last_fetch(common_dir),
    })
}

/// Split `origin/feature/x` into its remote and the rest.
///
/// Splitting on the first `/` is wrong — branch names contain slashes far more
/// often than remote names do — so match against the remotes we actually read
/// from the config, longest name first. The fallback only runs for a repository
/// whose config we could not read.
fn split_remote(remote_ref: &str, remotes: &[git_scylla_core::Remote]) -> String {
    let mut names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in names {
        if remote_ref.strip_prefix(name).is_some_and(|r| r.starts_with('/')) {
            return name.to_string();
        }
    }
    remote_ref.split('/').next().unwrap_or(remote_ref).to_string()
}

/// The initial auto-fetch state, and the only one the probe ever sets:
/// `Disabled` with no remote to fetch from, otherwise due immediately, so the
/// scheduler's first tick has a full work list.
fn initial_fetch_health(remotes: &[git_scylla_core::Remote], at: SystemTime) -> FetchHealth {
    if remotes.is_empty() {
        FetchHealth::disabled()
    } else {
        FetchHealth::due_now(at)
    }
}

/// Read `HEAD` of a bare repository without spawning git.
fn read_bare_head(git_dir: &Path) -> Option<Head> {
    let raw = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let raw = raw.trim();
    if let Some(ref_name) = raw.strip_prefix("ref: refs/heads/") {
        // Whether that branch has any commits needs a ref lookup; for the grid
        // the branch name is what matters and Unborn/Branch both display it.
        let name = ref_name.to_string();
        let full = format!("refs/heads/{name}");
        return Some(if ref_exists(git_dir, &full) {
            Head::Branch(name)
        } else {
            Head::Unborn(name)
        });
    }
    Oid::parse(raw).ok().map(Head::Detached)
}

/// Does this repository have a ref the user could check out by this name?
///
/// **`None` when the question cannot be answered from the filesystem** — a raw
/// object id, `main~3`, `origin/main@{2}`, anything carrying revision syntax.
/// That is not a gap to fill in later: answering those needs `git rev-parse`,
/// which is a subprocess *per repository per plan*, and a plan is computed
/// every time the user proposes an action. Guessing "missing" instead would
/// refuse a checkout that would have worked, which is worse than not claiming.
///
/// So a caller treats `None` as "let the job try, and translate the failure",
/// and `Some(false)` as a plan-time skip with `RefNotFound`.
///
/// The names it *can* answer are the ones bulk checkout is actually for: a
/// local branch, a tag, and the remote-tracking branch git would DWIM into a
/// local one.
pub fn has_ref(git_dir: &Path, rev: &str) -> Option<bool> {
    if looks_like_revision(rev) {
        return None;
    }
    let direct = [
        format!("refs/heads/{rev}"),
        format!("refs/tags/{rev}"),
        // `git checkout main` with no local `main` creates one tracking
        // `origin/main`. A caller asking about `main` means that too.
        format!("refs/remotes/{rev}"),
    ];
    if direct.iter().any(|full| ref_exists(git_dir, full)) {
        return Some(true);
    }
    // ...and the DWIM form, where the remote is not named.
    let remotes = std::fs::read_dir(git_dir.join("refs/remotes"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().join(rev).exists());
    if remotes {
        return Some(true);
    }
    // Packed remote-tracking refs are the common case for a fresh clone.
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
    let dwim = packed.lines().any(|line| {
        !line.starts_with('#')
            && !line.starts_with('^')
            && line.split_once(' ').is_some_and(|(_, name)| {
                let name = name.trim_end();
                name.strip_prefix("refs/remotes/")
                    .and_then(|rest| rest.split_once('/'))
                    .is_some_and(|(_, branch)| branch == rev)
            })
    });
    Some(dwim)
}

/// Every tag this repository has, by name.
///
/// Loose refs under `refs/tags/`, plus `packed-refs`. Both, because a repository
/// that has been `git gc`-ed has its tags packed and no loose files at all, so a
/// loose-only read would report a decade-old project as having never been
/// released.
///
/// Names only. Deriving the next tag in a series is arithmetic over names
/// (`core::version`), so the object each one points at is not needed, and
/// reading it would mean opening every ref.
///
/// Cold data, but a directory walk rather than a subprocess. Cold still
/// matters — this runs once per plan, never per row.
pub fn tags(git_dir: &Path) -> Vec<String> {
    let root = git_dir.join("refs/tags");
    let mut out = Vec::new();
    // Tags nest (`refs/tags/release/1.0`), so this is a walk and not a listing.
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(&root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
    out.extend(packed.lines().filter_map(|line| {
        if line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        // `^<oid>` lines are the peeled targets of annotated tags and are
        // skipped above; the tag's own line is the one before.
        let (_, name) = line.split_once(' ')?;
        Some(name.trim_end().strip_prefix("refs/tags/")?.to_string())
    }));
    // A tag can be both loose and packed after a partial `gc`.
    out.sort();
    out.dedup();
    out
}

/// Which branch does this repository treat as its default?
///
/// `refs/remotes/<remote>/HEAD` is the authoritative answer and the one `git
/// clone` writes. It is a symbolic ref, so it is always a loose file — packed
/// refs cannot hold symrefs — which is why this reads one file and not
/// `packed-refs` as well.
///
/// A filesystem read rather than a subprocess: the fact lives in a file, so it
/// costs nothing.
///
/// The fallback matters as much as the primary answer: `git remote add` plus a
/// fetch never writes `origin/HEAD`, so a repository set up by hand has none,
/// and refusing to sync it would be refusing on a technicality. `main` then
/// `master`, in that order, checked through [`has_ref`] so a branch that exists
/// only as `origin/main` still counts — `git checkout main` creates it.
///
/// `None` means neither was found, and the caller must skip the repository by
/// name rather than guess. There is no third guess worth making: whatever else
/// this repository calls its trunk, the tool does not know it.
pub fn default_branch(git_dir: &Path, remotes: &[String]) -> Option<String> {
    // `origin` first when it exists, then the rest in configured order — the
    // same precedence `plan::preferred_remote` uses, for the same reason.
    let ordered =
        remotes.iter().filter(|r| *r == "origin").chain(remotes.iter().filter(|r| *r != "origin"));
    for remote in ordered {
        let head = git_dir.join("refs/remotes").join(remote).join("HEAD");
        let Ok(raw) = std::fs::read_to_string(&head) else { continue };
        let prefix = format!("ref: refs/remotes/{remote}/");
        if let Some(branch) = raw.trim().strip_prefix(&prefix) {
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
    }
    ["main", "master"].into_iter().find(|name| has_ref(git_dir, name) == Some(true)).map(Into::into)
}

/// Does this look like a revision *expression* rather than a plain ref name?
///
/// Generous on purpose: anything unusual counts as an expression, which makes
/// [`has_ref`] answer `None`, which makes the caller let the job try. Being
/// wrong in that direction costs a job that fails with a good message; being
/// wrong the other way refuses work that would have succeeded.
fn looks_like_revision(rev: &str) -> bool {
    rev.is_empty()
        || rev.contains(['~', '^', ':', '@', '?', '*', '[', '\\', ' '])
        || rev.starts_with('-')
        || rev.ends_with(".lock")
        // A raw object id is a legitimate checkout target and is not a ref.
        || (rev.len() >= 7 && rev.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Does a ref by this full name exist, loose or packed?
///
/// The loose file is checked first because it is the cheaper answer and the one
/// a working repository usually has. `packed-refs` is not an optimisation to
/// skip: a bare mirror is normally packed, so its branch has no loose file at
/// all, and a loose-only test reported every healthy mirror as an unborn HEAD —
/// a fresh `git init` and a mirror of the company monorepo rendering alike.
fn ref_exists(git_dir: &Path, full_name: &str) -> bool {
    if git_dir.join(full_name).exists() {
        return true;
    }
    std::fs::read_to_string(git_dir.join("packed-refs"))
        .is_ok_and(|packed| packed_refs_contains(&packed, full_name))
}

/// One `<oid> <refname>` per line.
///
/// `#` opens the header line and `^` a peeled tag target; neither names a ref,
/// and treating a peel line as one would invent refs whose "name" is an oid.
fn packed_refs_contains(packed: &str, full_name: &str) -> bool {
    packed.lines().any(|line| {
        !line.starts_with('#')
            && !line.starts_with('^')
            && line.split_once(' ').is_some_and(|(_, name)| name.trim_end() == full_name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_scylla_core::Remote;

    fn remote(name: &str) -> Remote {
        Remote { name: name.to_string(), host: None }
    }

    #[test]
    fn splits_the_remote_off_a_slashed_branch_name() {
        let remotes = vec![remote("origin"), remote("origin/mirror")];
        assert_eq!(split_remote("origin/feature/x", &remotes), "origin");
        // Longest match wins, so a remote whose name contains a slash works.
        assert_eq!(split_remote("origin/mirror/main", &remotes), "origin/mirror");
        // Fallback when the config was unreadable.
        assert_eq!(split_remote("upstream/main", &[]), "upstream");
    }

    #[test]
    fn head_from_status_covers_every_shape() {
        let branch = PorcelainStatus {
            branch: Some("main".into()),
            oid: Some("abc1234".into()),
            ..Default::default()
        };
        assert_eq!(head_from(&branch), Head::Branch("main".into()));

        let unborn =
            PorcelainStatus { branch: Some("main".into()), oid: None, ..Default::default() };
        assert_eq!(head_from(&unborn), Head::Unborn("main".into()));

        let detached =
            PorcelainStatus { branch: None, oid: Some("abc1234".into()), ..Default::default() };
        assert_eq!(head_from(&detached), Head::Detached(Oid::parse("abc1234").unwrap()));
    }

    #[test]
    fn a_bare_repository_with_packed_refs_has_a_branch_not_an_unborn_one() {
        // The shape that made this necessary: `git pack-refs` leaves no loose
        // file, and bare mirrors are normally packed. A loose-only test read a
        // perfectly healthy mirror as a fresh `git init`.
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        std::fs::write(g.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            g.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \n\
             a94a8fe5ccb19ba61c4c0873d391e987982fbbd3 refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(read_bare_head(g), Some(Head::Branch("main".into())));
    }

    #[test]
    fn a_loose_ref_still_answers_and_a_genuinely_absent_one_is_still_unborn() {
        // The unborn case has to keep working: a fresh `git init --bare` has
        // HEAD pointing at a branch that does not exist yet, and calling that
        // one a branch would be the opposite lie.
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        std::fs::write(g.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_bare_head(g), Some(Head::Unborn("feature/x".into())));

        // A slashed branch name is a nested path under refs/heads.
        std::fs::create_dir_all(g.join("refs/heads/feature")).unwrap();
        std::fs::write(g.join("refs/heads/feature/x"), "abc1234\n").unwrap();
        assert_eq!(read_bare_head(g), Some(Head::Branch("feature/x".into())));
    }

    #[test]
    fn a_packed_refs_header_or_peel_line_does_not_name_a_ref() {
        let packed = "# pack-refs with: peeled fully-peeled sorted \n\
                      aaaaaaaaaa refs/tags/v1\n\
                      ^bbbbbbbbbb\n\
                      cccccccccc refs/heads/main\n";
        assert!(packed_refs_contains(packed, "refs/heads/main"));
        assert!(packed_refs_contains(packed, "refs/tags/v1"));
        assert!(!packed_refs_contains(packed, "refs/heads/other"));
        // A prefix of a real ref is not that ref.
        assert!(!packed_refs_contains(packed, "refs/heads/mai"));
        assert!(!packed_refs_contains(packed, "^bbbbbbbbbb"));
    }

    #[test]
    fn a_ref_question_is_answered_only_when_it_can_be_answered_cheaply() {
        // `None` is not a gap. Answering these needs `git rev-parse`, which is
        // a subprocess per repository per plan, and a plan is computed every
        // time somebody proposes an action. Refusing a checkout that would have
        // worked is worse than letting the job try.
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        std::fs::create_dir_all(g.join("refs/heads")).unwrap();
        std::fs::write(g.join("refs/heads/main"), "abc1234\n").unwrap();

        assert_eq!(has_ref(g, "main"), Some(true));
        assert_eq!(has_ref(g, "nope"), Some(false));
        for expression in ["main~3", "main^", "HEAD@{2}", "a1b2c3d4e5", "-x", "", "with space"] {
            assert_eq!(has_ref(g, expression), None, "{expression:?}");
        }
    }

    #[test]
    fn a_remote_tracking_branch_answers_for_the_name_git_would_dwim() {
        // `git checkout main` with no local `main` creates one tracking
        // `origin/main`, so a caller asking about `main` means that too — and
        // for a fresh clone the ref is packed rather than loose.
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        std::fs::write(
            g.join("packed-refs"),
            "# pack-refs with: peeled\nabc1234 refs/remotes/origin/release\n",
        )
        .unwrap();
        assert_eq!(has_ref(g, "release"), Some(true));
        assert_eq!(has_ref(g, "origin/release"), Some(true));
        assert_eq!(has_ref(g, "other"), Some(false));
    }

    #[test]
    fn no_remotes_means_auto_fetch_is_disabled_not_failing() {
        // A repository with nothing to fetch from must never enter backoff:
        // it would retry forever and quarantine for the wrong reason.
        let at = SystemTime::UNIX_EPOCH;
        assert_eq!(initial_fetch_health(&[], at), FetchHealth::disabled());
        assert_eq!(initial_fetch_health(&[remote("origin")], at), FetchHealth::due_now(at));
    }
}
