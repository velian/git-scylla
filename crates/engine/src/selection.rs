//! Which repositories a batch is about.

use git_scylla_core::{Filter, FilterError, RepoId, RepoSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// How a batch chooses its repositories.
///
/// The filter grammar is [`git_scylla_core::Filter`], reused verbatim from
/// `scan --filter`. Not a convenience: the CLI and the GUI must share **one**
/// parser, and the cheapest guarantee is that only one exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Selection {
    /// Everything offered.
    All,
    /// An explicit set — what the GUI's multi-selection produces.
    Ids(BTreeSet<RepoId>),
    /// On the wire this is the expression text, because that is what a `Filter`
    /// serializes as — see `core::Filter`'s serde impl.
    Filter(#[cfg_attr(feature = "ts", ts(type = "string"))] Filter),
}

impl Selection {
    /// Parse a `--select` argument.
    ///
    /// `all` and `*` mean everything; anything else is a filter expression.
    /// `home` expands a leading `~/` in a `path:` term and is passed in rather
    /// than read from the environment, so this stays a pure function of its
    /// arguments.
    pub fn parse(expr: &str, home: Option<&Path>) -> Result<Self, FilterError> {
        let trimmed = expr.trim();
        if trimmed.eq_ignore_ascii_case("all") || trimmed == "*" {
            return Ok(Selection::All);
        }
        Ok(Selection::Filter(Filter::parse_with_home(trimmed, home)?))
    }

    pub fn ids(ids: impl IntoIterator<Item = RepoId>) -> Self {
        Selection::Ids(ids.into_iter().collect())
    }

    pub fn contains(&self, snap: &RepoSnapshot) -> bool {
        match self {
            Selection::All => true,
            Selection::Ids(ids) => ids.contains(&snap.id),
            Selection::Filter(f) => f.matches(snap),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_scylla_core::Head;

    fn snap(path: &str, branch: &str) -> RepoSnapshot {
        let mut s = RepoSnapshot::stub(path);
        s.head = Head::Branch(branch.into());
        s
    }

    #[test]
    fn all_selects_everything() {
        assert!(Selection::All.contains(&snap("/a", "main")));
        assert_eq!(Selection::parse("all", None).unwrap(), Selection::All);
        assert_eq!(Selection::parse("ALL", None).unwrap(), Selection::All);
        assert_eq!(Selection::parse("*", None).unwrap(), Selection::All);
    }

    #[test]
    fn an_explicit_id_set_selects_exactly_those() {
        let sel = Selection::ids([RepoId::from_canonical("/a")]);
        assert!(sel.contains(&snap("/a", "main")));
        assert!(!sel.contains(&snap("/b", "main")));
    }

    #[test]
    fn a_filter_expression_uses_the_shared_grammar() {
        let sel = Selection::parse("branch:main", None).unwrap();
        assert!(sel.contains(&snap("/a", "main")));
        assert!(!sel.contains(&snap("/a", "release")));
    }

    #[test]
    fn a_selection_round_trips_over_the_wire() {
        // The GUI sends one of these; the filter travels as its own text so
        // the one parser stays the only parser.
        for sel in [
            Selection::All,
            Selection::ids([RepoId::from_canonical("/a"), RepoId::from_canonical("/b")]),
            Selection::parse("behind:>0 & !dirty", None).unwrap(),
        ] {
            let json = serde_json::to_string(&sel).unwrap();
            assert_eq!(serde_json::from_str::<Selection>(&json).unwrap(), sel, "{json}");
        }
        assert_eq!(
            serde_json::to_string(&Selection::parse("dirty", None).unwrap()).unwrap(),
            r#"{"type":"Filter","value":"dirty"}"#
        );
    }

    #[test]
    fn a_bad_expression_is_an_error_not_an_empty_selection() {
        // Silently selecting nothing is the worst outcome: the user sees "0
        // eligible" and concludes the repositories are fine.
        assert!(Selection::parse("brunch:main", None).is_err());
        assert!(Selection::parse("", None).is_err());
    }
}
