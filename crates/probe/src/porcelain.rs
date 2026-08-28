//! Parser for `git status --porcelain=v2 --branch --show-stash -z -unormal`.
//!
//! Positional, byte-oriented, and never regex-based. Two properties drive the
//! shape:
//!
//! * **Records are NUL-separated, not newline-separated.** Paths may contain
//!   newlines, and a line-oriented parser silently miscounts when they do.
//! * **A rename entry occupies two records.** The parser must consume the
//!   second, or every entry after the first rename is read as the wrong type.
//!
//! Paths are never decoded to `String`: with `-z` they are raw bytes, and a
//! repository containing non-UTF-8 filenames must parse rather than error. We
//! only ever count them, so there is nothing to gain by decoding.

use git_scylla_core::{AheadBehind, WorkTree};

/// Everything one `git status` invocation tells us.
///
/// A flat parse result, not a `RepoSnapshot` — the snapshot also needs facts
/// from the git directory, so the parser stays a pure function of a byte
/// buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PorcelainStatus {
    /// `# branch.oid`. `None` for an unborn HEAD, where git emits `(initial)`.
    pub oid: Option<String>,
    /// `# branch.head`. `None` when git emits `(detached)`.
    pub branch: Option<String>,
    /// `# branch.upstream` — the short tracking ref, e.g. `origin/main`.
    pub upstream: Option<String>,
    /// `# branch.ab`. **Absent when the upstream is configured but its
    /// remote-tracking ref does not exist**, which is how the deleted-upstream
    /// case is detected.
    pub ab: Option<AheadBehind>,
    pub stashes: u32,
    pub work: WorkTree,
}

impl PorcelainStatus {
    /// An upstream is configured but git could not resolve it.
    pub fn upstream_gone(&self) -> bool {
        self.upstream.is_some() && self.ab.is_none()
    }
}

/// Parse the raw stdout of the status command.
///
/// Unrecognised or malformed records are ignored rather than rejected, so a
/// future git version adding a header — or one malformed record — does not
/// turn a whole repository into an error.
pub fn parse_porcelain_v2(bytes: &[u8]) -> PorcelainStatus {
    let mut out = PorcelainStatus::default();
    let mut records = bytes.split(|&b| b == 0).filter(|r| !r.is_empty());

    while let Some(rec) = records.next() {
        match rec[0] {
            b'#' => parse_header(rec, &mut out),
            // `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
            b'1' => count_xy(rec, &mut out.work),
            // Rename/copy: same prefix, then `<X><score>`, then two paths —
            // the new one here, the original in the next record.
            b'2' => {
                count_xy(rec, &mut out.work);
                let _original_path = records.next();
            }
            b'u' => out.work.conflicted += 1,
            b'?' => out.work.untracked += 1,
            b'!' => {}
            _ => {}
        }
    }
    out
}

fn parse_header(rec: &[u8], out: &mut PorcelainStatus) {
    let Ok(text) = std::str::from_utf8(rec) else { return };
    let Some(rest) = text.strip_prefix("# ") else { return };
    let (key, value) = match rest.split_once(' ') {
        Some(kv) => kv,
        None => (rest, ""),
    };
    match key {
        "branch.oid" => out.oid = (value != "(initial)").then(|| value.to_string()),
        "branch.head" => out.branch = (value != "(detached)").then(|| value.to_string()),
        "branch.upstream" => out.upstream = Some(value.to_string()),
        "branch.ab" => out.ab = parse_ab(value),
        "stash" => out.stashes = value.parse().unwrap_or(0),
        _ => {}
    }
}

/// `+3 -7` — always both, always in that order, always signed.
fn parse_ab(value: &str) -> Option<AheadBehind> {
    let mut parts = value.split_whitespace();
    let ahead = parts.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = parts.next()?.strip_prefix('-')?.parse().ok()?;
    Some(AheadBehind { ahead, behind })
}

/// The two-character status field of a type-1 or type-2 record.
///
/// Positional: the record is `<type><SP><X><Y><SP>...`, so XY is bytes 2 and 3.
/// `.` means unmodified on that side. A path can be both staged and modified,
/// and both counters must then increment — which is the whole reason `WorkTree`
/// is a struct of counts and not a state.
fn count_xy(rec: &[u8], work: &mut WorkTree) {
    if rec.len() < 4 || rec[1] != b' ' {
        return;
    }
    if rec[2] != b'.' {
        work.staged += 1;
    }
    if rec[3] != b'.' {
        work.modified += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a NUL-separated buffer the way git does.
    fn buf(records: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for r in records {
            v.extend_from_slice(r.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn headers() {
        let s = parse_porcelain_v2(&buf(&[
            "# branch.oid a94a8fe5ccb19ba61c4c0873d391e987982fbbd3",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +3 -7",
            "# stash 2",
        ]));
        assert_eq!(s.oid.as_deref(), Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.ab, Some(AheadBehind { ahead: 3, behind: 7 }));
        assert_eq!(s.stashes, 2);
    }

    #[test]
    fn unborn_and_detached_are_not_oids_or_branches() {
        let s = parse_porcelain_v2(&buf(&["# branch.oid (initial)", "# branch.head main"]));
        assert_eq!(s.oid, None);
        assert_eq!(s.branch.as_deref(), Some("main"));

        let s = parse_porcelain_v2(&buf(&["# branch.oid abc123", "# branch.head (detached)"]));
        assert_eq!(s.oid.as_deref(), Some("abc123"));
        assert_eq!(s.branch, None);
    }

    #[test]
    fn upstream_without_ab_means_the_tracking_ref_is_gone() {
        let s = parse_porcelain_v2(&buf(&[
            "# branch.oid abc123",
            "# branch.head main",
            "# branch.upstream origin/deleted",
        ]));
        assert!(s.upstream_gone());
        assert_eq!(s.ab, None);

        let s = parse_porcelain_v2(&buf(&["# branch.upstream origin/main", "# branch.ab +0 -0"]));
        assert!(!s.upstream_gone(), "in sync is not gone");
    }

    #[test]
    fn counts_by_side_of_the_index() {
        let s = parse_porcelain_v2(&buf(&[
            "1 M. N... 100644 100644 100644 aaa bbb staged-only",
            "1 .M N... 100644 100644 100644 aaa bbb modified-only",
            "1 MM N... 100644 100644 100644 aaa bbb both",
            "? untracked-one",
            "? untracked-two",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflicted",
            "! an-ignored-file",
        ]));
        assert_eq!(s.work, WorkTree { staged: 2, modified: 2, untracked: 2, conflicted: 1 });
    }

    #[test]
    fn a_rename_consumes_its_second_record() {
        let s = parse_porcelain_v2(&buf(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 new-name",
            "old-name",
            "? scratch",
        ]));
        assert_eq!(s.work.staged, 1);
        assert_eq!(s.work.modified, 0);
        assert_eq!(s.work.untracked, 1);
    }

    #[test]
    fn two_renames_in_a_row() {
        let s = parse_porcelain_v2(&buf(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 new-a",
            "old-a",
            "2 R. N... 100644 100644 100644 aaa bbb R090 new-b",
            "old-b",
            "1 .M N... 100644 100644 100644 aaa bbb touched",
        ]));
        assert_eq!(s.work, WorkTree { staged: 2, modified: 1, untracked: 0, conflicted: 0 });
    }

    #[test]
    fn paths_with_newlines_spaces_and_non_utf8_bytes() {
        let mut v = Vec::new();
        v.extend_from_slice(
            b"1 .M N... 100644 100644 100644 aaa bbb dir/with a space/and\nnewline",
        );
        v.push(0);
        v.extend_from_slice(b"? bad-");
        v.extend_from_slice(&[0xff, 0xfe]);
        v.extend_from_slice(b"-name");
        v.push(0);
        v.extend_from_slice(b"1 M. N... 100644 100644 100644 aaa bbb after");
        v.push(0);

        let s = parse_porcelain_v2(&v);
        assert_eq!(s.work, WorkTree { staged: 1, modified: 1, untracked: 1, conflicted: 0 });
    }

    #[test]
    fn a_rename_to_a_path_containing_a_nul_is_impossible_but_empty_is_not() {
        let s = parse_porcelain_v2(b"? one\0\0? two\0");
        assert_eq!(s.work.untracked, 2);
    }

    #[test]
    fn garbage_is_ignored_not_fatal() {
        let s = parse_porcelain_v2(&buf(&[
            "# branch.tomorrow something",
            "# branch.ab nonsense",
            "9 unknown record type",
            "1 short",
            "? real",
        ]));
        assert_eq!(s.ab, None);
        assert_eq!(s.work.untracked, 1);
    }

    #[test]
    fn empty_input_is_a_clean_repository() {
        let s = parse_porcelain_v2(b"");
        assert_eq!(s.work, WorkTree::default());
        assert_eq!(s.branch, None);
    }
}
