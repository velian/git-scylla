//! Deriving the next tag in a pre-release series.
//!
//! Pure arithmetic over tag *names*. Nothing here reads a repository, and
//! nothing here knows what a commit is: the input is the list of tags a
//! repository has, the output is the one name it does not have yet.
//!
//! Ordering is by name, never by commit date. A tag's date only says when
//! somebody typed `git tag`; `v2.4.0-dev.7` follows `v2.4.0-dev.6` whichever
//! order they were created in, and a repository where those two were cut out of
//! order is exactly the one a date-based answer gets wrong.

use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Which component the next series starts from.
///
/// A team decision rather than a fact, so it is asked rather than assumed. The
/// plan shows the derived name, so a wrong answer is visible before anything is
/// created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bump {
    Major,
    Minor,
    Patch,
}

impl std::fmt::Display for Bump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Bump::Major => "major",
            Bump::Minor => "minor",
            Bump::Patch => "patch",
        })
    }
}

/// `major.minor.patch`, ordered as numbers.
///
/// Derived `Ord` over the fields in declaration order is the right comparison,
/// and is why these are `u64` rather than the strings they were parsed from:
/// `v2.10.0` is newer than `v2.9.0`, and a string sort says the opposite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Where the next series starts, from this release.
    fn bumped(self, bump: Bump) -> Self {
        match bump {
            Bump::Major => Version { major: self.major + 1, minor: 0, patch: 0 },
            Bump::Minor => Version { minor: self.minor + 1, patch: 0, ..self },
            Bump::Patch => Version { patch: self.patch + 1, ..self },
        }
    }
}

/// What a tag name turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pre {
    /// No pre-release part: an actual release, and a candidate for "newest".
    None,
    /// `-<channel>.<n>` — a numbered series.
    Series { channel: String, n: u64 },
    /// A pre-release this cannot count: `-beta`, `-rc1`, `-dev.x`.
    ///
    /// Distinct from `None` rather than discarded: `v3.0.0-beta` is not a
    /// release, and treating it as one would derive the next series from a
    /// version that never shipped.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    /// Whether the name carried a leading `v`, so the derived name can match
    /// the repository's own convention rather than imposing one.
    v_prefix: bool,
    version: Version,
    pre: Pre,
}

fn parse(tag: &str) -> Option<Parsed> {
    let (v_prefix, rest) = match tag.strip_prefix('v') {
        Some(rest) => (true, rest),
        None => (false, tag),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let mut parts = core.split('.');
    let mut number = || parts.next().and_then(|p| p.parse::<u64>().ok());
    let version = Version { major: number()?, minor: number()?, patch: number()? };
    // A fourth component is not a version this understands. Refusing beats
    // silently reading `1.2.3.4` as `1.2.3`.
    if parts.next().is_some() {
        return None;
    }
    let pre = match pre {
        None => Pre::None,
        Some(pre) => match pre.rsplit_once('.') {
            Some((channel, n)) => match n.parse::<u64>() {
                Ok(n) if !channel.is_empty() => Pre::Series { channel: channel.to_string(), n },
                _ => Pre::Other,
            },
            None => Pre::Other,
        },
    };
    Some(Parsed { v_prefix, version, pre })
}

/// The next tag in `channel`'s series, given every tag this repository has.
///
/// The rule, in order:
///
/// 1. The newest **release** — a tag with no pre-release part — decides where
///    the next series would start, via `bump`.
/// 2. The newest existing tag *in this channel* decides where a series already
///    under way is. Whichever of the two is higher wins, so cutting `dev` tags
///    toward `3.0.0` keeps going toward `3.0.0` even when `bump` is `minor` and
///    the last release was `2.3.7`. A counter that reset itself every time
///    somebody chose the smaller bump would produce a name that already exists.
/// 3. The number is one past the highest already used *at that version*, or 1.
///
/// Unparseable tags are ignored, and so are pre-releases in other shapes
/// (`-beta`, `-rc1`): counting `v3.0.0-beta` as a release would derive the next
/// series from a version that never shipped.
///
/// Total — a repository with no tags gets the first one — because the
/// alternative is a skip reason for a repository whose only problem is being
/// new.
pub fn next_dev_tag(tags: &[String], channel: &str, bump: Bump) -> String {
    let parsed: Vec<Parsed> = tags.iter().filter_map(|t| parse(t)).collect();

    let newest_release = parsed.iter().filter(|p| p.pre == Pre::None).max_by_key(|p| p.version);
    let newest_in_series = parsed
        .iter()
        .filter(|p| matches!(&p.pre, Pre::Series { channel: c, .. } if c == channel))
        .max_by_key(|p| (p.version, series_n(p)));

    let from_release = newest_release.map(|p| p.version.bumped(bump));
    let from_series = newest_in_series.map(|p| p.version);
    let target = from_release.max(from_series).unwrap_or_else(|| {
        // No tags this understands. Start from nothing and apply the same bump,
        // so `--bump major` on a fresh repository cuts `v1.0.0-dev.1` rather
        // than something the user has to explain.
        Version { major: 0, minor: 0, patch: 0 }.bumped(bump)
    });

    let n = parsed
        .iter()
        .filter(|p| p.version == target)
        .filter_map(|p| match &p.pre {
            Pre::Series { channel: c, n } if c == channel => Some(*n),
            _ => None,
        })
        .max()
        .map_or(1, |n| n + 1);

    // The repository's own convention, taken from whichever tag decided the
    // answer. Imposing `v` on a project that does not use it leaves a tag list
    // with two spellings in it — the kind of mess a bulk tool excels at.
    let v_prefix = newest_in_series.or(newest_release).is_none_or(|p| p.v_prefix);
    format!("{}{target}-{channel}.{n}", if v_prefix { "v" } else { "" })
}

fn series_n(p: &Parsed) -> u64 {
    match &p.pre {
        Pre::Series { n, .. } => *n,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn next(names: &[&str]) -> String {
        next_dev_tag(&tags(names), "dev", Bump::Minor)
    }

    #[test]
    fn the_first_dev_tag_after_a_release_starts_the_next_series() {
        assert_eq!(next(&["v2.3.7"]), "v2.4.0-dev.1");
    }

    #[test]
    fn a_series_already_under_way_carries_on() {
        assert_eq!(next(&["v2.3.7", "v2.4.0-dev.1", "v2.4.0-dev.2"]), "v2.4.0-dev.3");
    }

    #[test]
    fn versions_are_ordered_as_numbers_not_as_strings() {
        // A string sort puts `v2.9.0` after `v2.10.0`, cutting the next series
        // from a release two versions old — onto a tag that already exists.
        assert_eq!(next(&["v2.9.0", "v2.10.0"]), "v2.11.0-dev.1");
        assert_eq!(next(&["v2.4.0-dev.9", "v2.4.0-dev.10", "v2.3.0"]), "v2.4.0-dev.11");
    }

    #[test]
    fn a_series_ahead_of_the_bump_wins() {
        // Releases stop at 2.3.7 and a minor bump would say 2.4.0, but somebody
        // is already cutting toward 3.0.0. Resetting to 2.4.0 would be
        // abandoning that series — and if 2.4.0-dev.1 also existed, it would be
        // deriving a name that is already taken.
        assert_eq!(next(&["v2.3.7", "v3.0.0-dev.1"]), "v3.0.0-dev.2");
    }

    #[test]
    fn a_release_ahead_of_the_series_wins_too() {
        // The mirror image: 2.4.0 shipped, so its dev series is over.
        assert_eq!(next(&["v2.4.0-dev.1", "v2.4.0-dev.2", "v2.4.0"]), "v2.5.0-dev.1");
    }

    #[test]
    fn the_bump_decides_where_a_new_series_starts() {
        assert_eq!(next_dev_tag(&tags(&["v2.3.7"]), "dev", Bump::Major), "v3.0.0-dev.1");
        assert_eq!(next_dev_tag(&tags(&["v2.3.7"]), "dev", Bump::Minor), "v2.4.0-dev.1");
        assert_eq!(next_dev_tag(&tags(&["v2.3.7"]), "dev", Bump::Patch), "v2.3.8-dev.1");
    }

    #[test]
    fn channels_do_not_see_each_other() {
        let have = tags(&["v2.3.7", "v2.4.0-dev.4", "v2.4.0-rc.1"]);
        assert_eq!(next_dev_tag(&have, "dev", Bump::Minor), "v2.4.0-dev.5");
        assert_eq!(next_dev_tag(&have, "rc", Bump::Minor), "v2.4.0-rc.2");
    }

    #[test]
    fn a_pre_release_in_another_shape_is_not_a_release() {
        // `v3.0.0-beta` has not shipped. Counting it as a release would derive
        // the next series from a version nobody has.
        assert_eq!(next(&["v2.3.7", "v3.0.0-beta"]), "v2.4.0-dev.1");
        assert_eq!(next(&["v2.3.7", "v3.0.0-rc1"]), "v2.4.0-dev.1");
    }

    #[test]
    fn tags_that_are_not_versions_are_ignored() {
        assert_eq!(next(&["release-2024-06", "latest", "v2.3.7", "1.2.3.4"]), "v2.4.0-dev.1");
    }

    #[test]
    fn a_repository_with_no_tags_gets_the_first_one() {
        assert_eq!(next(&[]), "v0.1.0-dev.1");
        assert_eq!(next_dev_tag(&[], "dev", Bump::Major), "v1.0.0-dev.1");
    }

    #[test]
    fn the_repositorys_own_prefix_convention_is_kept() {
        // A project that does not write `v` must not be given one, or its tag
        // list ends up with two spellings in it.
        assert_eq!(next(&["2.3.7"]), "2.4.0-dev.1");
        assert_eq!(next(&["2.3.7", "2.4.0-dev.1"]), "2.4.0-dev.2");
        assert_eq!(next(&["v2.3.7"]), "v2.4.0-dev.1");
        // Mixed: the tag that decided the answer decides the spelling.
        assert_eq!(next(&["2.3.7", "v2.4.0-dev.1"]), "v2.4.0-dev.2");
    }

    #[test]
    fn the_derived_name_is_never_one_that_already_exists() {
        // The property that matters, over a corpus rather than an example.
        let corpus: Vec<Vec<&str>> = vec![
            vec![],
            vec!["v1.0.0"],
            vec!["v1.0.0", "v1.1.0-dev.1"],
            vec!["v1.0.0", "v1.1.0-dev.1", "v1.1.0-dev.2", "v1.1.0"],
            vec!["v0.9.0", "v0.10.0-dev.3", "v0.10.0-dev.11"],
            vec!["2.3.7", "2.4.0-dev.1", "junk", "v9.9.9-beta"],
            vec!["v1.2.3", "v1.2.3-dev.1", "v2.0.0-dev.1"],
        ];
        for have in corpus {
            for bump in [Bump::Major, Bump::Minor, Bump::Patch] {
                let names = tags(&have);
                let next = next_dev_tag(&names, "dev", bump);
                assert!(!names.contains(&next), "{next} already exists in {have:?} ({bump})");
            }
        }
    }
}
