use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One line of a job's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(with = "crate::serde_time")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub at: SystemTime,
    pub stream: Stream,
    /// Lossy UTF-8: git can write non-UTF-8 filenames into its error messages.
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
    /// elision marker.
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
