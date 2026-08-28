use anstyle::{AnsiColor, Style};
use git_scylla_core::{duration, Badge, FetchStatus, Head, ProbeOutcome, RepoSnapshot};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const LEGEND: &str = "\
STATUS COLUMN
  \u{2191}3   3 commits ahead of upstream  (\"committed but not pushed\")
  \u{2193}7   7 commits behind upstream, as of the last fetch
  \u{25cf}2   2 paths modified in the worktree
  +1   1 path staged
  ?4   4 untracked paths
  \u{00d7}1   1 path conflicted
  \u{2691}2   2 stash entries
  -    no upstream configured  (NOT the same as \u{2191}0 \u{2193}0)
  \u{2191}? \u{2193}?  upstream configured but its remote-tracking ref is gone

BADGES, in sort priority order
  conflict diverged behind ahead dirty staged clean unknown

FETCH COLUMN
  The fetch scheduler's health for this repository. When it is healthy this is
  the age of the newest fetch by anyone, which is what the behind count is as
  current as. Otherwise it says what is wrong: 'retry in 5m (2)' after failures,
  or 'quarantined: <reason>' once the tool has stopped trying. A scan itself
  never fetches.

SELECTION EXPRESSIONS  (--select, or --filter on `scan`)
  Terms joined by '&', any term negatable with '!'. Every term must match.
    badge:dirty  branch:main  name:api  path:~/work/*  kind:bare
    upstream:none|gone|set|ahead|behind|diverged|ok
    op:any|merge|rebase|cherry-pick|revert|bisect
    ahead|behind|staged|modified|untracked|conflicted|stashes  with >N >=N <N <=N N
  A bare badge name is shorthand: 'dirty' means 'badge:dirty'.
  Examples:
    --select 'behind:>0 & !dirty'
    --select 'upstream:none'
    --select 'op:any'
";

struct Palette {
    on: bool,
}

impl Palette {
    fn new() -> Self {
        // Auto-disabled when not a TTY, so `| tee` and CI logs stay readable.
        Self { on: std::io::stdout().is_terminal() }
    }

    fn paint(&self, style: Style, text: &str) -> String {
        if self.on {
            format!("{style}{text}{style:#}")
        } else {
            text.to_string()
        }
    }

    fn badge(&self, b: Badge) -> String {
        let style = Style::new().fg_color(Some(
            match b {
                Badge::Conflict => AnsiColor::Red,
                Badge::InProgress => AnsiColor::Red,
                Badge::Diverged => AnsiColor::Magenta,
                Badge::Behind => AnsiColor::Yellow,
                Badge::Ahead => AnsiColor::Cyan,
                Badge::Dirty => AnsiColor::Yellow,
                Badge::Staged => AnsiColor::Blue,
                Badge::Clean => AnsiColor::Green,
                Badge::Unknown => AnsiColor::BrightBlack,
            }
            .into(),
        ));
        self.paint(style, &b.to_string())
    }

    fn dim(&self, text: &str) -> String {
        self.paint(Style::new().fg_color(Some(AnsiColor::BrightBlack.into())), text)
    }
}

pub fn table(rows: &[RepoSnapshot]) {
    if rows.is_empty() {
        println!("no repositories found");
        return;
    }
    let p = Palette::new();
    let base = common_root(rows);
    let now = SystemTime::now();

    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|s| {
            [
                display_path(&s.path, &base),
                branch_cell(s),
                s.badge().to_string(),
                s.status_line(),
                fetch_cell(s, now),
            ]
        })
        .collect();

    let headers = ["PATH", "BRANCH", "BADGE", "STATUS", "FETCH"];
    // Widths from the plain strings; colour escapes must not count toward them.
    let mut w = headers.map(str::len);
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            w[i] = w[i].max(cell.chars().count());
        }
    }

    let head: Vec<String> = headers.iter().enumerate().map(|(i, h)| pad(h, w[i])).collect();
    println!("{}", p.dim(head.join("  ").trim_end()));

    for (row, snap) in cells.iter().zip(rows) {
        let line = format!(
            "{}  {}  {}  {}  {}",
            pad(&row[0], w[0]),
            pad(&row[1], w[1]),
            pad(&p.badge(snap.badge()), w[2] + colour_slack(&p, &row[2], &p.badge(snap.badge()))),
            pad(&row[3], w[3]),
            p.dim(&row[4]),
        );
        println!("{}", line.trim_end());
        if let ProbeOutcome::Error(msg) = &snap.outcome {
            // A failed probe must never be silently a row of dashes.
            println!("    {}", p.dim(&format!("error: {}", first_line(msg))));
        }
    }
}

/// Padding a coloured string needs the escape bytes added back to the width.
fn colour_slack(p: &Palette, plain: &str, painted: &str) -> usize {
    if p.on {
        painted.chars().count().saturating_sub(plain.chars().count())
    } else {
        0
    }
}

fn pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - len))
    }
}

fn branch_cell(s: &RepoSnapshot) -> String {
    // An untrustworthy snapshot has no real head; its oid is a placeholder.
    if !s.is_trustworthy() {
        return "?".into();
    }
    match &s.head {
        Head::Branch(b) => b.clone(),
        Head::Unborn(b) => format!("{b} (unborn)"),
        Head::Detached(oid) => format!("({})", oid.short()),
    }
}

/// The scheduler's health readout for this repository.
pub fn fetch_cell(s: &RepoSnapshot, now: SystemTime) -> String {
    match s.fetch_status() {
        FetchStatus::NoRemote => "no remote".into(),
        FetchStatus::Off => "off".into(),
        FetchStatus::Quarantined { reason } => format!("quarantined: {}", first_line(&reason)),
        FetchStatus::BackingOff { until, failures } => match until.duration_since(now) {
            Ok(d) => format!("retry in {} ({failures})", duration::brief(d)),
            Err(_) => format!("retry due ({failures})"),
        },
        // A future timestamp means the clock moved; "just now" is still honest.
        FetchStatus::Fetched { at } => {
            now.duration_since(at).map_or_else(|_| "just now".into(), duration::since)
        }
        FetchStatus::Never => "never".into(),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// The longest directory prefix shared by every row, so paths are readable.
fn common_root(rows: &[RepoSnapshot]) -> PathBuf {
    let mut iter = rows.iter().map(|s| s.path.as_path());
    let Some(first) = iter.next() else { return PathBuf::new() };
    let mut base: PathBuf = first.parent().unwrap_or(first).to_path_buf();
    for path in iter {
        while !path.starts_with(&base) {
            match base.parent() {
                Some(p) => base = p.to_path_buf(),
                None => return PathBuf::new(),
            }
        }
    }
    base
}

fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> RepoSnapshot {
        RepoSnapshot::stub("/r/a")
    }

    #[test]
    fn a_failed_probe_shows_no_branch_rather_than_a_placeholder() {
        let mut s = snap();
        s.outcome = ProbeOutcome::Error("boom".into());
        assert_eq!(branch_cell(&s), "?");
        s.outcome = ProbeOutcome::Timeout;
        assert_eq!(branch_cell(&s), "?");
    }

    #[test]
    fn detached_and_unborn_heads_are_distinguishable() {
        let mut s = snap();
        s.head = Head::Detached(git_scylla_core::Oid::parse("deadbeefcafe").unwrap());
        assert_eq!(branch_cell(&s), "(deadbee)");
        s.head = Head::Unborn("main".into());
        assert_eq!(branch_cell(&s), "main (unborn)");
    }

    #[test]
    fn no_remote_says_so_rather_than_never() {
        assert_eq!(fetch_cell(&snap(), SystemTime::UNIX_EPOCH), "no remote");
    }

    #[test]
    fn common_root_strips_the_shared_prefix() {
        let mut a = snap();
        a.path = "/r/x/a".into();
        let mut b = snap();
        b.path = "/r/y/b".into();
        let base = common_root(&[a.clone(), b.clone()]);
        assert_eq!(base, PathBuf::from("/r"));
        assert_eq!(display_path(&a.path, &base), "x/a");
    }

    #[test]
    fn a_single_row_still_shows_its_name() {
        let rows = [snap()];
        let base = common_root(&rows);
        assert_eq!(display_path(&rows[0].path, &base), "a");
    }
}
