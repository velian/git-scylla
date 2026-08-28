use git_scylla_core::LogLine;
use std::collections::VecDeque;

/// Bytes of transcript retained per job, before elision.
///
/// A `git fetch --progress` of a large repository, or a hook that logs a build,
/// can produce tens of megabytes. Every transcript is kept for the session, so
/// forty of those is the difference between a tool and a leak. Four megabytes is
/// more than anyone reads and small enough that a hundred cost nothing.
pub const DEFAULT_TRANSCRIPT_CAP: usize = 4 * 1024 * 1024;

/// A capped, order-preserving accumulator with head/tail retention.
///
/// Both ends are kept because both matter and for different reasons: the head
/// holds the command and what it started doing, the tail holds the error that
/// ended it. Keeping only the tail loses the context; keeping only the head
/// loses the answer.
#[derive(Debug)]
pub struct Transcript {
    cap: usize,
    head: Vec<LogLine>,
    head_bytes: usize,
    /// Set the first time a line goes to the tail, and never cleared.
    ///
    /// Without it the head stays open on a *per-line* test, so a long line
    /// overflows to the tail and then a later short one still fits under the
    /// half-cap and lands in the head — in front of the line it followed.
    /// [`Transcript::finish`] concatenates head then tail, so the transcript
    /// comes out in the wrong order, and with nothing elided there is not even a
    /// marker to explain it. Ordering is the whole point of an interleaved
    /// transcript, so the head closes for good.
    head_closed: bool,
    tail: VecDeque<LogLine>,
    tail_bytes: usize,
    elided_lines: u64,
    elided_bytes: u64,
}

impl Transcript {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            head_bytes: 0,
            head_closed: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            elided_lines: 0,
            elided_bytes: 0,
        }
    }

    /// Half the cap, which is what each end gets.
    fn half(&self) -> usize {
        self.cap / 2
    }

    pub fn push(&mut self, line: LogLine) {
        let n = line.text.len();
        if !self.head_closed && self.head_bytes + n <= self.half() {
            self.head_bytes += n;
            self.head.push(line);
            return;
        }
        // From here on every line belongs to the tail, however small. A line
        // that happens to fit must not overtake one that did not.
        self.head_closed = true;
        self.tail_bytes += n;
        self.tail.push_back(line);
        while self.tail_bytes > self.half() {
            // `tail` is non-empty: we just pushed, and a single line larger than
            // the half-cap drains to empty and then stops, retaining nothing —
            // which is the honest outcome for a line bigger than the budget.
            let Some(dropped) = self.tail.pop_front() else { break };
            self.tail_bytes -= dropped.text.len();
            self.elided_lines += 1;
            self.elided_bytes += dropped.text.len() as u64;
        }
    }

    /// Head, then a marker if anything was dropped, then tail.
    ///
    /// The marker is a [`git_scylla_core::Stream::Notice`] line, so nothing
    /// attributes it to git, and it carries the counts — a transcript that
    /// silently omits ten thousand lines is worse than one that says it did.
    pub fn finish(mut self) -> Vec<LogLine> {
        let mut out = std::mem::take(&mut self.head);
        if self.elided_lines > 0 {
            let mut marker = LogLine::notice(format!(
                "... {} lines ({}) elided ...",
                self.elided_lines,
                human_bytes(self.elided_bytes)
            ));
            // Timestamp it at the resume point so the transcript stays
            // monotonic when read top to bottom.
            if let Some(first_tail) = self.tail.front() {
                marker.at = first_tail.at;
            }
            out.push(marker);
        }
        out.extend(self.tail);
        out
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_scylla_core::Stream;

    fn line(text: &str) -> LogLine {
        LogLine::new(Stream::Stdout, text)
    }

    #[test]
    fn under_the_cap_nothing_changes() {
        let mut t = Transcript::new(1024);
        for i in 0..10 {
            t.push(line(&format!("line {i}")));
        }
        let out = t.finish();
        assert_eq!(out.len(), 10);
        assert_eq!(out[0].text, "line 0");
        assert_eq!(out[9].text, "line 9");
        assert!(out.iter().all(|l| l.stream == Stream::Stdout), "no marker when nothing elided");
    }

    #[test]
    fn over_the_cap_keeps_both_ends_and_says_what_it_dropped() {
        // 20 bytes of budget: 10 for the head, 10 for the tail.
        let mut t = Transcript::new(20);
        for i in 0..100 {
            t.push(line(&format!("{i:04}"))); // 4 bytes each
        }
        let out = t.finish();

        // The head is the start of the run...
        assert_eq!(out[0].text, "0000");
        // ...the tail is the end of it...
        assert_eq!(out.last().unwrap().text, "0099");
        // ...and exactly one Notice sits between them.
        let markers: Vec<_> = out.iter().filter(|l| l.stream == Stream::Notice).collect();
        assert_eq!(markers.len(), 1);
        assert!(markers[0].text.contains("elided"), "{}", markers[0].text);

        let marker_at = out.iter().position(|l| l.stream == Stream::Notice).unwrap();
        assert!(marker_at > 0 && marker_at < out.len() - 1, "marker must be between the ends");
    }

    #[test]
    fn accounts_for_every_line() {
        let mut t = Transcript::new(40);
        for i in 0..50 {
            t.push(line(&format!("{i:04}")));
        }
        // Read the count back out of the marker: the transcript's own account of
        // what it dropped is the thing that has to be right, since it is what a
        // reader sees.
        let out = t.finish();
        let marker = out.iter().find(|l| l.stream == Stream::Notice).expect("a marker");
        let elided: u64 = marker.text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let kept = out.iter().filter(|l| l.stream != Stream::Notice).count() as u64;
        assert_eq!(kept + elided, 50, "every line is either kept or counted as elided");
    }

    #[test]
    fn a_short_line_never_overtakes_a_long_one_it_followed() {
        // The head is filled to just under its half, so the next line — too big
        // to fit — starts the tail, and the one after it is small enough that a
        // per-line test would put it back in the head, ahead of the line it
        // came after. Nothing is elided here, so a reordering would be silent.
        let mut t = Transcript::new(100); // 50 bytes per end
        t.push(line(&"a".repeat(45)));
        t.push(line(&"b".repeat(10))); // 55 > 50: the tail starts here
        t.push(line(&"c".repeat(4))); // 49 <= 50, and must go to the tail anyway

        let order: Vec<char> = t.finish().iter().filter_map(|l| l.text.chars().next()).collect();
        assert_eq!(order, ['a', 'b', 'c'], "the transcript reordered its lines");
    }

    #[test]
    fn a_single_line_bigger_than_the_budget_does_not_loop_forever() {
        let mut t = Transcript::new(16);
        t.push(line(&"x".repeat(1000)));
        t.push(line(&"y".repeat(1000)));
        let out = t.finish();
        // The assertion that matters is that this terminates at all. Retaining
        // nothing but the marker is the honest outcome.
        assert!(out.len() <= 2);
    }

    #[test]
    fn a_zero_cap_is_survivable() {
        let mut t = Transcript::new(0);
        for i in 0..10 {
            t.push(line(&format!("{i}")));
        }
        let out = t.finish();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].stream, Stream::Notice);
        assert!(out[0].text.contains("10 lines"), "{}", out[0].text);
    }

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
