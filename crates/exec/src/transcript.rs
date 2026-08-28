use git_scylla_core::LogLine;
use std::collections::VecDeque;

/// Bytes of transcript retained per job, before elision.
pub const DEFAULT_TRANSCRIPT_CAP: usize = 4 * 1024 * 1024;

/// A capped, order-preserving accumulator with head/tail retention: the head
/// holds the start of the transcript, the tail holds how it ended.
#[derive(Debug)]
pub struct Transcript {
    cap: usize,
    head: Vec<LogLine>,
    head_bytes: usize,
    /// Set the first time a line goes to the tail, and never cleared, so a
    /// later short line cannot land back in the head ahead of one that
    /// already overflowed.
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
        self.head_closed = true;
        self.tail_bytes += n;
        self.tail.push_back(line);
        while self.tail_bytes > self.half() {
            let Some(dropped) = self.tail.pop_front() else { break };
            self.tail_bytes -= dropped.text.len();
            self.elided_lines += 1;
            self.elided_bytes += dropped.text.len() as u64;
        }
    }

    /// Head, then a `Notice` marker if anything was dropped, then tail.
    pub fn finish(mut self) -> Vec<LogLine> {
        let mut out = std::mem::take(&mut self.head);
        if self.elided_lines > 0 {
            let mut marker = LogLine::notice(format!(
                "... {} lines ({}) elided ...",
                self.elided_lines,
                human_bytes(self.elided_bytes)
            ));
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
            t.push(line(&format!("{i:04}")));
        }
        let out = t.finish();

        assert_eq!(out[0].text, "0000");
        assert_eq!(out.last().unwrap().text, "0099");
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
        let out = t.finish();
        let marker = out.iter().find(|l| l.stream == Stream::Notice).expect("a marker");
        let elided: u64 = marker.text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let kept = out.iter().filter(|l| l.stream != Stream::Notice).count() as u64;
        assert_eq!(kept + elided, 50, "every line is either kept or counted as elided");
    }

    #[test]
    fn a_short_line_never_overtakes_a_long_one_it_followed() {
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
