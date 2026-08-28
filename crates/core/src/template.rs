//! Per-repository text: commit messages and branch names.
//!
//! A tiny, closed substitution set, not a template language — the moment this
//! grows conditionals it becomes something that has to be debugged, and the
//! thing being debugged would be a commit message about to be written to
//! thirty-one repositories.

use crate::RepoSnapshot;
use std::time::SystemTime;

/// What may appear in braces, and what it means.
///
/// Documented as data so a surface can show the list without restating it —
/// help text that repeats a table is help text that goes stale.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{repo}", "the repository's directory name"),
    ("{branch}", "the current branch, or `HEAD` when detached"),
    ("{date}", "today, as YYYY-MM-DD"),
];

/// Substitute the placeholders for one repository.
///
/// An unknown placeholder is left exactly as written. A commit message
/// legitimately contains braces — JSON, a shell snippet, an issue template — and
/// refusing them would be hostile. A typo like `{brnach}` is caught by the plan
/// sheet, which shows every rendered message before anything runs.
pub fn render(template: &str, snap: &RepoSnapshot, now: SystemTime) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('}') else {
            // An unterminated brace is just text.
            break;
        };
        let name = &rest[..=close];
        match name {
            "{repo}" => out.push_str(snap.id.name()),
            "{branch}" => out.push_str(snap.branch().unwrap_or("HEAD")),
            "{date}" => out.push_str(&today(now)),
            other => out.push_str(other),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Does this text use any placeholder?
///
/// For a surface deciding whether to offer a per-repository preview: a message
/// with no placeholder is the same in every repository, and thirty-one
/// identical lines is not a preview.
pub fn is_templated(text: &str) -> bool {
    PLACEHOLDERS.iter().any(|(p, _)| text.contains(p))
}

/// `YYYY-MM-DD`, UTC.
///
/// Computed rather than pulled from a date library: this is the only date
/// arithmetic in the project, and a dependency whose entire use is one format
/// string is one more thing to keep in step for nothing.
///
/// UTC, not local time. Git's own timestamps carry a zone and this string does
/// not, so it uses the one zone that is unambiguous — a message stamped with a
/// date that disagrees with its committer timestamp across midnight is the kind
/// of thing nobody notices for a year.
fn today(now: SystemTime) -> String {
    let secs = now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a civil date.
///
/// Howard Hinnant's `civil_from_days`: exact for every date this will see.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FetchHealth, Head, ProbeOutcome, RepoId, RepoKind, WorkTree};
    use std::time::Duration;

    fn snap(path: &str, head: Head) -> RepoSnapshot {
        RepoSnapshot {
            id: RepoId::from_canonical(path),
            path: path.into(),
            kind: RepoKind::Normal,
            head,
            head_oid: None,
            upstream: None,
            remotes: vec![],
            work: WorkTree::default(),
            op: None,
            stashes: 0,
            fetch: FetchHealth::disabled(),
            probed_at: SystemTime::UNIX_EPOCH,
            outcome: ProbeOutcome::Ok,
            from_cache: false,
            watched: false,
        }
    }

    fn at(days: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(days * 86_400)
    }

    #[test]
    fn every_placeholder_substitutes() {
        let s = snap("/work/api", Head::Branch("release/2.4".into()));
        assert_eq!(
            render("chore({repo}): sync {branch} on {date}", &s, at(20_000)),
            "chore(api): sync release/2.4 on 2024-10-04"
        );
    }

    #[test]
    fn the_same_template_differs_per_repository() {
        // The whole point: one template, thirty-one distinct messages, each of
        // which the plan shows.
        let now = at(20_000);
        let a = render("sync {repo}", &snap("/work/api", Head::Branch("main".into())), now);
        let b = render("sync {repo}", &snap("/work/web", Head::Branch("main".into())), now);
        assert_eq!((a.as_str(), b.as_str()), ("sync api", "sync web"));
    }

    #[test]
    fn a_detached_head_still_renders_something_true() {
        let s = snap("/work/api", Head::Detached(crate::Oid::parse("abc1234").unwrap()));
        assert_eq!(render("on {branch}", &s, at(0)), "on HEAD");
    }

    #[test]
    fn an_unborn_branch_renders_its_name() {
        // It is a real branch name even though no commit carries it, and a first
        // commit is exactly when a template is useful.
        let s = snap("/work/new", Head::Unborn("main".into()));
        assert_eq!(render("first commit on {branch}", &s, at(0)), "first commit on main");
    }

    #[test]
    fn an_unknown_placeholder_is_left_alone() {
        // A typo is caught by the plan showing the rendered message, not by an
        // error here — braces are legitimate message content.
        let s = snap("/work/api", Head::Branch("main".into()));
        assert_eq!(render("{brnach} and {\"json\": 1}", &s, at(0)), "{brnach} and {\"json\": 1}");
    }

    #[test]
    fn text_with_no_placeholder_is_returned_unchanged() {
        let s = snap("/work/api", Head::Branch("main".into()));
        assert_eq!(render("a plain message", &s, at(0)), "a plain message");
        assert!(!is_templated("a plain message"));
        assert!(is_templated("sync {repo}"));
    }

    #[test]
    fn an_unterminated_brace_is_text() {
        let s = snap("/work/api", Head::Branch("main".into()));
        assert_eq!(render("look: {repo", &s, at(0)), "look: {repo");
        assert_eq!(render("{", &s, at(0)), "{");
    }

    #[test]
    fn the_date_is_right_at_the_edges() {
        // The only date arithmetic in the project, pinned where off-by-one
        // lives.
        assert_eq!(today(at(0)), "1970-01-01");
        assert_eq!(today(at(59)), "1970-03-01");
        // 2000 is a leap year; 1900 was not, which is what catches naive rules.
        assert_eq!(today(at(11_016)), "2000-02-29");
        assert_eq!(today(at(20_000)), "2024-10-04");
    }
}
