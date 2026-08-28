use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Automatic-fetch bookkeeping for one repository.
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
        // Without this, the wire form is serde's default `{secs, nanos}` map.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FetchStatus {
    NoRemote,
    Off,
    Quarantined { reason: String },
    BackingOff {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        until: SystemTime,
        failures: u32,
    },
    /// `at` is the newest fetch by anyone, including the user's own terminal.
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
    /// `Off`, `NoRemote`, and `Never` are not problems.
    pub fn is_problem(&self) -> bool {
        matches!(self, FetchStatus::Quarantined { .. } | FetchStatus::BackingOff { .. })
    }
}
