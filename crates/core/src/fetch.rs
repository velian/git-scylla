use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Automatic-fetch bookkeeping for one repository.
///
/// Engine state rather than a fact on disk, but it rides in the snapshot: that
/// is the single object shipped to every surface, and a parallel stream for
/// this would buy nothing but joins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchHealth {
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_attempt: Option<SystemTime>,
    /// Last success **by this tool's scheduler**. Compare
    /// [`crate::Upstream::last_fetch`], which moves for any fetch by anyone.
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_success: Option<SystemTime>,
    pub schedule: FetchSchedule,
}

impl FetchHealth {
    /// No remote, or the user opted this repository out.
    pub fn disabled() -> Self {
        Self { last_attempt: None, last_success: None, schedule: FetchSchedule::Disabled }
    }

    /// Eligible, never yet attempted by us.
    pub fn due_now(at: SystemTime) -> Self {
        Self { last_attempt: None, last_success: None, schedule: FetchSchedule::Due(at) }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FetchSchedule {
    /// Next attempt, already jittered.
    Due(
        // Without this the wire form is serde's `{secs, nanos}` map, sitting
        // next to `last_attempt`, which is a number. One representation for a
        // timestamp, everywhere.
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        SystemTime,
    ),
    BackingOff {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        until: SystemTime,
        failures: u32,
    },
    /// Repeated failure. Never retried automatically; a manual fetch clears it.
    Quarantined {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        since: SystemTime,
        last_error: String,
    },
    /// No remote to fetch from, or opted out.
    Disabled,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// What the fetch column is saying about a repository.
///
/// The **decision** — which of these six things is true — is one decision and
/// lives here. The phrasing is not: a CLI table cell has one line to say it in,
/// and the grid has a cell plus a tooltip plus a button, so each surface
/// renders this its own way. What they may not do is disagree about which case
/// they are in, which is what they did while they each read
/// [`FetchSchedule`] for themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FetchStatus {
    /// Nothing to fetch from. Not a fault: a repository with no remote is a
    /// repository with no remote.
    NoRemote,
    /// A remote exists, and fetching it is off.
    Off,
    /// Repeated failure; nothing will retry it automatically.
    Quarantined { reason: String },
    BackingOff {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        until: SystemTime,
        failures: u32,
    },
    /// Healthy. `at` is the newest fetch by anyone — the user in their own
    /// terminal included — which is what the `behind` count is as current as.
    Fetched {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        at: SystemTime,
    },
    /// Healthy, and nothing has fetched it yet.
    Never,
}

impl FetchStatus {
    /// Is something wrong that the user can act on?
    ///
    /// `Off` and `NoRemote` are not problems, and `Never` is a repository the
    /// scheduler simply has not reached yet.
    pub fn is_problem(&self) -> bool {
        matches!(self, FetchStatus::Quarantined { .. } | FetchStatus::BackingOff { .. })
    }
}
