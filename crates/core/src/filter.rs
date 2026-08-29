//! The selection-expression grammar shared by the CLI's `--filter` and the
//! GUI's filter box.
//!
//! ```text
//! expr   := term ('&' term)*
//! term   := '!'? (key ':' value | badge | fuzzy)
//! key    := badge | branch | name | path | kind | upstream | op
//!         | ahead | behind | staged | modified | untracked | conflicted | stashes
//! value  := glob | keyword | comparison
//! cmp    := ('>' | '>=' | '<' | '<=' | '=')? number
//! glob   := literal with '*' (any run) and '?' (any one)
//! fuzzy  := a bare word that is not a recognized badge, matched against the
//!           repository name as a case-insensitive subsequence
//! ```

use crate::{Badge, Head, InProgress, RepoKind, RepoSnapshot};
use std::path::Path;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilterError {
    #[error("empty filter expression")]
    Empty,
    #[error("empty term (a stray '&'?)")]
    EmptyTerm,
    #[error("unknown key {0:?}")]
    UnknownKey(String),
    #[error("unknown badge {0:?}")]
    UnknownBadge(String),
    #[error("unknown kind {0:?} (expected normal, bare, worktree or submodule)")]
    UnknownKind(String),
    #[error(
        "unknown upstream state {0:?} (expected none, gone, set, ok, ahead, behind or diverged)"
    )]
    UnknownUpstream(String),
    #[error("unknown operation {0:?}")]
    UnknownOp(String),
    #[error("{0:?} is not a number or comparison")]
    BadComparison(String),
}

/// A conjunction of terms. Every term must match.
///
/// Keeps the text it was parsed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    source: String,
    terms: Vec<(bool, Term)>, // (negated, term)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Badge(Badge),
    Branch(Glob),
    Name(Glob),
    NameFuzzy(String),
    Path(Glob),
    Kind(KindMatch),
    Upstream(UpstreamMatch),
    Op(Option<InProgress>),
    Count(CountField, Cmp, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindMatch {
    Normal,
    Bare,
    Worktree,
    Submodule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamMatch {
    None,
    Gone,
    Set,
    Ahead,
    Behind,
    Diverged,
    InSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountField {
    Ahead,
    Behind,
    Staged,
    Modified,
    Untracked,
    Conflicted,
    Stashes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Cmp {
    fn test(self, lhs: u32, rhs: u32) -> bool {
        match self {
            Cmp::Eq => lhs == rhs,
            Cmp::Gt => lhs > rhs,
            Cmp::Ge => lhs >= rhs,
            Cmp::Lt => lhs < rhs,
            Cmp::Le => lhs <= rhs,
        }
    }
}

impl Filter {
    pub fn parse(expr: &str) -> Result<Self, FilterError> {
        Self::parse_with_home(expr, None)
    }

    pub fn parse_with_home(expr: &str, home: Option<&Path>) -> Result<Self, FilterError> {
        if expr.trim().is_empty() {
            return Err(FilterError::Empty);
        }
        let mut terms = Vec::new();
        let source = expr.trim().to_string();
        for raw in expr.split('&') {
            let raw = raw.trim();
            if raw.is_empty() {
                return Err(FilterError::EmptyTerm);
            }
            let (negated, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, raw),
            };
            if body.is_empty() {
                return Err(FilterError::EmptyTerm);
            }
            terms.push((negated, Term::parse(body, home)?));
        }
        Ok(Self { source, terms })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn matches(&self, snap: &RepoSnapshot) -> bool {
        self.terms.iter().all(|(neg, t)| t.matches(snap) != *neg)
    }

    pub fn terms(&self) -> &[(bool, Term)] {
        &self.terms
    }
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source)
    }
}

impl serde::Serialize for Filter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.source)
    }
}

impl<'de> serde::Deserialize<'de> for Filter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let expr = <String as serde::Deserialize>::deserialize(d)?;
        Filter::parse(&expr).map_err(serde::de::Error::custom)
    }
}

impl Term {
    fn parse(body: &str, home: Option<&Path>) -> Result<Self, FilterError> {
        let Some((key, value)) = body.split_once(':') else {
            return Ok(match parse_badge(body) {
                Some(b) => Term::Badge(b),
                None => Term::NameFuzzy(body.to_string()),
            });
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "badge" => parse_badge(value)
                .map(Term::Badge)
                .ok_or_else(|| FilterError::UnknownBadge(value.to_string())),
            "branch" => Ok(Term::Branch(Glob::new(value))),
            "name" => Ok(Term::Name(Glob::new(value))),
            "path" => Ok(Term::Path(Glob::new(&expand_home(value, home)))),
            "kind" => Ok(Term::Kind(match value.to_ascii_lowercase().as_str() {
                "normal" => KindMatch::Normal,
                "bare" => KindMatch::Bare,
                "worktree" => KindMatch::Worktree,
                "submodule" => KindMatch::Submodule,
                _ => return Err(FilterError::UnknownKind(value.to_string())),
            })),
            "upstream" => Ok(Term::Upstream(match value.to_ascii_lowercase().as_str() {
                "none" => UpstreamMatch::None,
                "gone" => UpstreamMatch::Gone,
                "set" => UpstreamMatch::Set,
                "ahead" => UpstreamMatch::Ahead,
                "behind" => UpstreamMatch::Behind,
                "diverged" => UpstreamMatch::Diverged,
                "ok" | "sync" | "insync" => UpstreamMatch::InSync,
                _ => return Err(FilterError::UnknownUpstream(value.to_string())),
            })),
            "op" => Ok(Term::Op(match value.to_ascii_lowercase().as_str() {
                "any" => None,
                "merge" => Some(InProgress::Merge),
                "rebase" => Some(InProgress::Rebase),
                "cherry-pick" | "cherrypick" => Some(InProgress::CherryPick),
                "revert" => Some(InProgress::Revert),
                "bisect" => Some(InProgress::Bisect),
                _ => return Err(FilterError::UnknownOp(value.to_string())),
            })),
            k => {
                let field = match k {
                    "ahead" => CountField::Ahead,
                    "behind" => CountField::Behind,
                    "staged" => CountField::Staged,
                    "modified" => CountField::Modified,
                    "untracked" => CountField::Untracked,
                    "conflicted" => CountField::Conflicted,
                    "stashes" | "stash" => CountField::Stashes,
                    _ => return Err(FilterError::UnknownKey(k.to_string())),
                };
                let (cmp, n) = parse_cmp(value)?;
                Ok(Term::Count(field, cmp, n))
            }
        }
    }

    fn matches(&self, s: &RepoSnapshot) -> bool {
        match self {
            Term::Badge(b) => s.badge() == *b,
            Term::Branch(g) => match &s.head {
                Head::Branch(b) | Head::Unborn(b) => g.matches(b),
                Head::Detached(_) => false,
            },
            Term::Name(g) => g.matches(s.id.name()),
            Term::NameFuzzy(needle) => fuzzy_match(needle, s.id.name()),
            Term::Path(g) => g.matches(&s.path.to_string_lossy()),
            Term::Kind(k) => matches!(
                (k, &s.kind),
                (KindMatch::Normal, RepoKind::Normal)
                    | (KindMatch::Bare, RepoKind::Bare)
                    | (KindMatch::Worktree, RepoKind::Worktree { .. })
                    | (KindMatch::Submodule, RepoKind::Submodule { .. })
            ),
            Term::Upstream(u) => match (u, &s.upstream) {
                (UpstreamMatch::None, None) => true,
                (_, None) => false,
                (UpstreamMatch::None, Some(_)) => false,
                (UpstreamMatch::Gone, Some(up)) => up.is_gone(),
                (UpstreamMatch::Set, Some(up)) => !up.is_gone(),
                (UpstreamMatch::Ahead, Some(up)) => up.sync.is_some_and(|ab| ab.ahead > 0),
                (UpstreamMatch::Behind, Some(up)) => up.sync.is_some_and(|ab| ab.behind > 0),
                (UpstreamMatch::Diverged, Some(up)) => up.sync.is_some_and(|ab| ab.diverged()),
                (UpstreamMatch::InSync, Some(up)) => {
                    up.sync.is_some_and(|ab| ab.ahead == 0 && ab.behind == 0)
                }
            },
            Term::Op(None) => s.op.is_some(),
            Term::Op(Some(want)) => s.op == Some(*want),
            Term::Count(field, cmp, n) => {
                let lhs = match field {
                    CountField::Ahead => match s.upstream.as_ref().and_then(|u| u.ahead()) {
                        Some(v) => v,
                        None => return false,
                    },
                    CountField::Behind => match s.upstream.as_ref().and_then(|u| u.behind()) {
                        Some(v) => v,
                        None => return false,
                    },
                    CountField::Staged => s.work.staged,
                    CountField::Modified => s.work.modified,
                    CountField::Untracked => s.work.untracked,
                    CountField::Conflicted => s.work.conflicted,
                    CountField::Stashes => s.stashes,
                };
                cmp.test(lhs, *n)
            }
        }
    }
}

fn parse_badge(s: &str) -> Option<Badge> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "conflict" => Badge::Conflict,
        "in-progress" | "inprogress" => Badge::InProgress,
        "diverged" => Badge::Diverged,
        "behind" => Badge::Behind,
        "ahead" => Badge::Ahead,
        "dirty" => Badge::Dirty,
        "staged" => Badge::Staged,
        "clean" => Badge::Clean,
        "unreachable" => Badge::Unreachable,
        "unknown" => Badge::Unknown,
        _ => return None,
    })
}

fn parse_cmp(v: &str) -> Result<(Cmp, u32), FilterError> {
    let bad = || FilterError::BadComparison(v.to_string());
    let (cmp, rest) = if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = v.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        (Cmp::Eq, v)
    };
    Ok((cmp, rest.trim().parse::<u32>().map_err(|_| bad())?))
}

fn expand_home(v: &str, home: Option<&Path>) -> String {
    match (v.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => format!("{}/{}", h.display(), rest),
        _ => v.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob(String);

impl Glob {
    pub fn new(pattern: &str) -> Self {
        Self(pattern.to_string())
    }

    pub fn matches(&self, s: &str) -> bool {
        glob_match(self.0.as_bytes(), s.as_bytes())
    }
}

fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    needle.chars().flat_map(char::to_lowercase).all(|n| hay.by_ref().any(|h| h == n))
}

fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while t < text.len() {
        match pat.get(p) {
            Some(b'*') => {
                star = Some(p);
                p += 1;
                resume = t;
            }
            Some(b'?') => {
                p += 1;
                t += 1;
            }
            Some(&c) if c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some(sp) => {
                    p = sp + 1;
                    resume += 1;
                    t = resume;
                }
                None => return false,
            },
        }
    }
    pat[p..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AheadBehind, Head, Upstream};

    fn snap(path: &str, branch: &str) -> RepoSnapshot {
        let mut s = RepoSnapshot::stub(path);
        s.head = Head::Branch(branch.into());
        s
    }

    #[test]
    fn globs() {
        assert!(Glob::new("main").matches("main"));
        assert!(!Glob::new("main").matches("maintenance"), "no implicit substring match");
        assert!(Glob::new("feat/*").matches("feat/thing"));
        assert!(Glob::new("*/work/*").matches("/Users/x/work/repo"));
        assert!(Glob::new("v?.?").matches("v1.2"));
        assert!(!Glob::new("v?.?").matches("v1.22"));
        assert!(Glob::new("*").matches(""));
        assert!(Glob::new("a*b*c").matches("axxbyyc"));
        assert!(!Glob::new("a*b*c").matches("axxbyy"));
        assert!(Glob::new("*a*a*a*").matches("bbaabbaabbaabb"));
        assert!(!Glob::new("*a*a*a*a").matches("bbaabbaabb"));
    }

    #[test]
    fn conjunction_and_negation() {
        let mut s = snap("/Users/x/work/api", "main");
        s.work.modified = 2;
        assert!(Filter::parse("badge:dirty & branch:main").unwrap().matches(&s));
        assert!(Filter::parse("dirty & path:*/work/*").unwrap().matches(&s));
        assert!(!Filter::parse("dirty & !branch:main").unwrap().matches(&s));
        assert!(Filter::parse("!branch:release").unwrap().matches(&s));
    }

    #[test]
    fn comparisons() {
        let mut s = snap("/r", "main");
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead: 0, behind: 7 }),
            last_fetch: None,
        });
        assert!(Filter::parse("behind:>0").unwrap().matches(&s));
        assert!(Filter::parse("behind:7").unwrap().matches(&s));
        assert!(Filter::parse("behind:>=7").unwrap().matches(&s));
        assert!(Filter::parse("behind:<8").unwrap().matches(&s));
        assert!(!Filter::parse("behind:>7").unwrap().matches(&s));
        assert!(Filter::parse("ahead:0").unwrap().matches(&s));
    }

    #[test]
    fn a_gone_upstream_matches_no_count_comparison() {
        let mut s = snap("/r", "main");
        s.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: None,
            last_fetch: None,
        });
        assert!(!Filter::parse("behind:0").unwrap().matches(&s));
        assert!(!Filter::parse("behind:>0").unwrap().matches(&s));
        assert!(Filter::parse("upstream:gone").unwrap().matches(&s));
        assert!(Filter::parse("!behind:>0").unwrap().matches(&s));
    }

    #[test]
    fn no_upstream_is_distinguishable_from_in_sync() {
        let bare = snap("/r", "main");
        let mut synced = snap("/r", "main");
        synced.upstream = Some(Upstream {
            remote: "origin".into(),
            remote_ref: "origin/main".into(),
            sync: Some(AheadBehind { ahead: 0, behind: 0 }),
            last_fetch: None,
        });
        assert!(Filter::parse("upstream:none").unwrap().matches(&bare));
        assert!(!Filter::parse("upstream:none").unwrap().matches(&synced));
        assert!(Filter::parse("upstream:ok").unwrap().matches(&synced));
        assert!(!Filter::parse("upstream:ok").unwrap().matches(&bare));
    }

    #[test]
    fn detached_head_matches_no_branch_glob() {
        let mut s = snap("/r", "main");
        s.head = Head::Detached(crate::Oid::parse("deadbeef").unwrap());
        assert!(!Filter::parse("branch:*").unwrap().matches(&s));
        assert!(Filter::parse("!branch:*").unwrap().matches(&s));
    }

    #[test]
    fn home_expansion_is_explicit() {
        let s = snap("/Users/x/work/api", "main");
        let f = Filter::parse_with_home("path:~/work/*", Some(Path::new("/Users/x"))).unwrap();
        assert!(f.matches(&s));
        assert!(!Filter::parse("path:~/work/*").unwrap().matches(&s));
    }

    #[test]
    fn a_filter_keeps_and_round_trips_its_source() {
        let f = Filter::parse("behind:>0 & !dirty").unwrap();
        assert_eq!(f.source(), "behind:>0 & !dirty");
        assert_eq!(f.to_string(), "behind:>0 & !dirty");

        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, r#""behind:>0 & !dirty""#, "a filter is its text on the wire");
        assert_eq!(serde_json::from_str::<Filter>(&json).unwrap(), f);
    }

    #[test]
    fn a_malformed_expression_cannot_arrive_over_the_wire() {
        assert!(serde_json::from_str::<Filter>(r#""brunch:main""#).is_err());
        assert!(serde_json::from_str::<Filter>(r#""""#).is_err());
    }

    #[test]
    fn errors_are_specific() {
        assert_eq!(Filter::parse("").unwrap_err(), FilterError::Empty);
        assert_eq!(Filter::parse("dirty &").unwrap_err(), FilterError::EmptyTerm);
        assert_eq!(
            Filter::parse("brunch:main").unwrap_err(),
            FilterError::UnknownKey("brunch".into())
        );
        assert_eq!(
            Filter::parse("badge:filthy").unwrap_err(),
            FilterError::UnknownBadge("filthy".into())
        );
        assert_eq!(
            Filter::parse("behind:lots").unwrap_err(),
            FilterError::BadComparison("lots".into())
        );
    }

    #[test]
    fn an_unrecognized_bare_word_fuzzy_matches_the_name() {
        let s = snap("/Users/x/work/git-scyllae", "main");
        assert!(Filter::parse("scyll").unwrap().matches(&s), "not a badge, falls back to fuzzy");
        assert!(Filter::parse("SCYLL").unwrap().matches(&s), "case-insensitive");
        assert!(Filter::parse("gitae").unwrap().matches(&s), "characters may skip over a gap");
        assert!(!Filter::parse("drity").unwrap().matches(&s), "no 'd' anywhere in the name");
        assert!(!Filter::parse("zzz").unwrap().matches(&s));
        assert!(!Filter::parse("scyllax").unwrap().matches(&s), "needle longer than any match");
    }
}
