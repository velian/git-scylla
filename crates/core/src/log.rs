use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One line of a job's transcript.
///
/// Timestamped and stream-tagged so that after operating on forty repositories,
/// "what happened to number 37" is answerable. The two child streams merge into
/// one ordered sequence, because that is how a human saw it when they ran the
/// command by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(with = "crate::serde_time")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub at: SystemTime,
    pub stream: Stream,
    /// Lossy UTF-8. Git writes filenames into its error messages, and on a
    /// non-UTF-8 name that byte sequence must become a readable transcript line
    /// rather than a failed job.
    pub text: String,
}

impl LogLine {
    pub fn new(stream: Stream, text: impl Into<String>) -> Self {
        Self { at: SystemTime::now(), stream, text: text.into() }
    }

    /// A line this tool wrote, not the child.
    pub fn notice(text: impl Into<String>) -> Self {
        Self::new(Stream::Notice, text)
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
    /// Written by git-scylla, not by the child: a timeout, a cancellation, an
    /// elision marker. Tagged distinctly so a transcript never attributes the
    /// tool's own words to git.
    Notice,
}

impl std::fmt::Display for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stream::Stdout => "out",
            Stream::Stderr => "err",
            Stream::Notice => "---",
        })
    }
}
