//! Turning a failed job's transcript into something a person can act on.
//!
//! Matching on English is sound because the environment sets `LC_ALL=C`.

use crate::{LogLine, Stream};
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// What kind of thing went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    NonFastForward,
    ProtectedBranch,
    Auth,
    Unreachable,
    /// `--force-with-lease` refused because the remote moved since the last fetch.
    LeaseStale,
    Conflict,
    /// Usually a hook on the remote; a failing local hook surfaces as `Unknown`.
    HookRejected,
    RefNotFound,
    /// The remote already has a tag by this name, at a different commit.
    TagExists,
    WouldOverwrite,
    Unknown,
}

impl FailureKind {
    /// What to do about it, or `None` when the honest answer is "read the
    /// transcript".
    pub fn remedy(self) -> Option<&'static str> {
        match self {
            FailureKind::NonFastForward => Some("pull first, then push again"),
            FailureKind::ProtectedBranch => {
                Some("the remote refused this branch; push from a branch it accepts")
            }
            FailureKind::Auth => Some("check the credential helper or SSH agent for this remote"),
            FailureKind::Unreachable => Some("check the remote URL and the network"),
            FailureKind::LeaseStale => Some("fetch, look at what arrived, then push again"),
            FailureKind::Conflict => Some("resolve it in this repository, then run the rest"),
            FailureKind::HookRejected => {
                Some("fix what the hook reported, or re-run without hooks")
            }
            FailureKind::RefNotFound => Some("no such ref in this repository"),
            FailureKind::TagExists => {
                Some("that name is already taken; fetch tags, then derive again")
            }
            FailureKind::WouldOverwrite => Some("commit or stash the changes first"),
            FailureKind::Unknown => None,
        }
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FailureKind::NonFastForward => "the remote has commits this does not",
            FailureKind::ProtectedBranch => "the remote refused the branch",
            FailureKind::Auth => "authentication failed",
            FailureKind::Unreachable => "the remote could not be reached",
            FailureKind::LeaseStale => "the remote moved since the last fetch",
            FailureKind::Conflict => "conflicts",
            FailureKind::HookRejected => "a hook refused it",
            FailureKind::RefNotFound => "no such ref",
            FailureKind::TagExists => "the remote already has that tag",
            FailureKind::WouldOverwrite => "would overwrite local changes",
            FailureKind::Unknown => "failed",
        })
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// A failure, explained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    pub kind: FailureKind,
    /// What to do, when there is a useful answer.
    pub remedy: Option<String>,
    /// The line from git that this was read off.
    pub evidence: String,
}

/// Read a failed job's transcript.
///
/// `None` when nothing was written to stderr at all, which for a failed job
/// means the failure was not git's to explain — a spawn error, or a deadline.
pub fn explain(log: &[LogLine]) -> Option<Explanation> {
    let stderr: Vec<&str> = log
        .iter()
        .filter(|l| l.stream == Stream::Stderr)
        .map(|l| l.text.trim())
        .filter(|t| !t.is_empty())
        .collect();

    for line in &stderr {
        if let Some(kind) = classify(line) {
            return Some(Explanation {
                kind,
                remedy: kind.remedy().map(str::to_string),
                evidence: (*line).to_string(),
            });
        }
    }
    stderr.first().map(|line| Explanation {
        kind: FailureKind::Unknown,
        remedy: None,
        evidence: (*line).to_string(),
    })
}

/// One line of git's stderr, if it says something recognisable.
///
/// Ordered by specificity, not by likelihood.
fn classify(line: &str) -> Option<FailureKind> {
    let lower = line.to_ascii_lowercase();

    if lower.contains("stale info") {
        return Some(FailureKind::LeaseStale);
    }
    if lower.contains("already exists") && (lower.contains("tag") || lower.contains("[rejected]")) {
        return Some(FailureKind::TagExists);
    }
    if lower.contains("non-fast-forward")
        || lower.contains("fetch first")
        || lower.contains("cannot lock ref")
    {
        return Some(FailureKind::NonFastForward);
    }
    // A pre-receive hook is the far side refusing; a local hook is this side.
    if lower.contains("protected branch")
        || lower.contains("pre-receive hook declined")
        || lower.contains("branch is protected")
    {
        return Some(FailureKind::ProtectedBranch);
    }
    if lower.contains("terminal prompts disabled")
        || lower.contains("authentication failed")
        || lower.contains("permission denied (publickey)")
        || lower.contains("could not read username")
        || lower.contains("access denied")
    {
        return Some(FailureKind::Auth);
    }
    if lower.contains("could not resolve host")
        || lower.contains("connection refused")
        || lower.contains("does not appear to be a git repository")
        || lower.contains("connection timed out")
        || lower.contains("network is unreachable")
    {
        return Some(FailureKind::Unreachable);
    }
    if lower.contains("automatic merge failed")
        || lower.contains("fix conflicts")
        || lower.contains("could not apply")
        || lower.contains("conflict (")
    {
        return Some(FailureKind::Conflict);
    }
    if lower.contains("hook declined") || lower.contains("hook failed") {
        return Some(FailureKind::HookRejected);
    }
    if lower.contains("did not match any file(s) known to git")
        || lower.contains("pathspec")
        || lower.contains("unknown revision")
        || lower.contains("not a valid ref")
    {
        return Some(FailureKind::RefNotFound);
    }
    if lower.contains("would be overwritten") || lower.contains("local changes") {
        return Some(FailureKind::WouldOverwrite);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(lines: &[(Stream, &str)]) -> Vec<LogLine> {
        lines.iter().map(|(s, t)| LogLine::new(*s, *t)).collect()
    }

    fn kind_of(lines: &[&str]) -> Option<FailureKind> {
        let l: Vec<(Stream, &str)> = lines.iter().map(|t| (Stream::Stderr, *t)).collect();
        explain(&log(&l)).map(|e| e.kind)
    }

    #[test]
    fn a_rejected_push_says_to_pull_first() {
        let e = explain(&log(&[
            (Stream::Stderr, " ! [rejected]        main -> main (fetch first)"),
            (Stream::Stderr, "error: failed to push some refs to 'origin'"),
            (Stream::Stderr, "hint: Updates were rejected because the remote contains work"),
        ]))
        .expect("an explanation");
        assert_eq!(e.kind, FailureKind::NonFastForward);
        assert_eq!(e.remedy.as_deref(), Some("pull first, then push again"));
        assert!(e.evidence.contains("[rejected]"), "{}", e.evidence);
    }

    #[test]
    fn losing_a_race_to_the_same_ref_is_a_non_fast_forward() {
        assert_eq!(
            kind_of(&[
                "remote: error: cannot lock ref 'refs/heads/main': is at 5ec57ea but expected ee78ac6",
                "error: failed to push some refs to '/tmp/origin.git'",
            ]),
            Some(FailureKind::NonFastForward)
        );
    }

    #[test]
    fn a_stale_lease_is_not_an_ordinary_rejection() {
        assert_eq!(
            kind_of(&[
                " ! [rejected]        main -> main (stale info)",
                "error: failed to push some refs",
            ]),
            Some(FailureKind::LeaseStale)
        );
    }

    #[test]
    fn credentials_are_a_configuration_problem_and_never_a_missed_dialog() {
        for line in [
            "fatal: could not read Username for 'https://example.invalid': terminal prompts disabled",
            "git@example.invalid: Permission denied (publickey).",
            "remote: Authentication failed for 'https://example.invalid/'",
        ] {
            assert_eq!(kind_of(&[line]), Some(FailureKind::Auth), "{line}");
        }
    }

    #[test]
    fn a_far_side_hook_is_a_protected_branch_and_a_near_side_one_is_not() {
        assert_eq!(
            kind_of(&["remote: error: GH006: Protected branch update failed"]),
            Some(FailureKind::ProtectedBranch)
        );
        assert_eq!(
            kind_of(&["remote: pre-receive hook declined"]),
            Some(FailureKind::ProtectedBranch)
        );
        assert_eq!(kind_of(&["error: pre-commit hook failed"]), Some(FailureKind::HookRejected));
    }

    #[test]
    fn the_shapes_the_other_actions_fail_in() {
        assert_eq!(
            kind_of(&["CONFLICT (content): Merge conflict in a.txt"]),
            Some(FailureKind::Conflict)
        );
        assert_eq!(
            kind_of(&["error: pathspec 'nope' did not match any file(s) known to git"]),
            Some(FailureKind::RefNotFound)
        );
    }

    #[test]
    fn a_tag_the_remote_already_has_is_not_a_non_fast_forward() {
        assert_eq!(
            kind_of(&["! [rejected]        HEAD -> v1.0.0-dev.1 (already exists)"]),
            Some(FailureKind::TagExists)
        );
        assert_eq!(
            kind_of(&["hint: Updates were rejected because the tag already exists in the remote."]),
            Some(FailureKind::TagExists)
        );
        assert!(FailureKind::TagExists.remedy().unwrap().contains("fetch tags"));

        assert_eq!(
            kind_of(&["! [rejected]        HEAD -> main (fetch first)"]),
            Some(FailureKind::NonFastForward)
        );
        assert_eq!(
            kind_of(&["fatal: a branch named 'wip' already exists"]),
            Some(FailureKind::Unknown),
            "`git branch` says \"already exists\" about something that is not a tag"
        );
        assert_eq!(
            kind_of(&["error: Your local changes to the following files would be overwritten"]),
            Some(FailureKind::WouldOverwrite)
        );
        assert_eq!(
            kind_of(&["fatal: 'nowhere' does not appear to be a git repository"]),
            Some(FailureKind::Unreachable)
        );
    }

    #[test]
    fn an_unrecognised_failure_still_carries_gits_own_words() {
        let e = explain(&log(&[(Stream::Stderr, "fatal: something entirely new")]))
            .expect("an explanation");
        assert_eq!(e.kind, FailureKind::Unknown);
        assert_eq!(e.remedy, None);
        assert_eq!(e.evidence, "fatal: something entirely new");
    }

    #[test]
    fn the_first_recognised_line_wins_not_the_last() {
        assert_eq!(
            kind_of(&[
                " ! [rejected]        main -> main (non-fast-forward)",
                "hint: see the 'Note about fast-forwards' in 'git push --help'",
                "error: failed to push some refs to 'origin'",
            ]),
            Some(FailureKind::NonFastForward)
        );
    }

    #[test]
    fn stdout_is_not_evidence_and_neither_is_silence() {
        assert_eq!(explain(&log(&[(Stream::Stdout, "error: not really")])), None);
        assert_eq!(explain(&[]), None);
        assert_eq!(explain(&log(&[(Stream::Stderr, "   ")])), None);
    }

    #[test]
    fn a_notice_this_tool_wrote_is_not_gits_word_for_anything() {
        assert_eq!(explain(&log(&[(Stream::Notice, "cancelled; killed the process group")])), None);
    }
}
