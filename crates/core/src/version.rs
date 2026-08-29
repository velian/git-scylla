//! Deriving the next tag in a pre-release series.

use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
    fn bumped(self, bump: Bump) -> Self {
        match bump {
            Bump::Major => Version { major: self.major + 1, minor: 0, patch: 0 },
            Bump::Minor => Version { minor: self.minor + 1, patch: 0, ..self },
            Bump::Patch => Version { patch: self.patch + 1, ..self },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pre {
    None,
    Series { channel: String, n: u64 },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
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

pub fn next_dev_tag(tags: &[String], channel: &str, bump: Bump) -> String {
    let parsed: Vec<Parsed> = tags.iter().filter_map(|t| parse(t)).collect();

    let newest_release = parsed.iter().filter(|p| p.pre == Pre::None).max_by_key(|p| p.version);
    let newest_in_series = parsed
        .iter()
        .filter(|p| matches!(&p.pre, Pre::Series { channel: c, .. } if c == channel))
        .max_by_key(|p| (p.version, series_n(p)));

    let from_release = newest_release.map(|p| p.version.bumped(bump));
    let from_series = newest_in_series.map(|p| p.version);
    let target = from_release
        .max(from_series)
        .unwrap_or_else(|| Version { major: 0, minor: 0, patch: 0 }.bumped(bump));

    let n = parsed
        .iter()
        .filter(|p| p.version == target)
        .filter_map(|p| match &p.pre {
            Pre::Series { channel: c, n } if c == channel => Some(*n),
            _ => None,
        })
        .max()
        .map_or(1, |n| n + 1);

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
        assert_eq!(next(&["v2.9.0", "v2.10.0"]), "v2.11.0-dev.1");
        assert_eq!(next(&["v2.4.0-dev.9", "v2.4.0-dev.10", "v2.3.0"]), "v2.4.0-dev.11");
    }

    #[test]
    fn a_series_ahead_of_the_bump_wins() {
        assert_eq!(next(&["v2.3.7", "v3.0.0-dev.1"]), "v3.0.0-dev.2");
    }

    #[test]
    fn a_release_ahead_of_the_series_wins_too() {
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
        assert_eq!(next(&["2.3.7"]), "2.4.0-dev.1");
        assert_eq!(next(&["2.3.7", "2.4.0-dev.1"]), "2.4.0-dev.2");
        assert_eq!(next(&["v2.3.7"]), "v2.4.0-dev.1");
        assert_eq!(next(&["2.3.7", "v2.4.0-dev.1"]), "v2.4.0-dev.2");
    }

    #[test]
    fn the_derived_name_is_never_one_that_already_exists() {
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
