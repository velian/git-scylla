use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchHealth {
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_attempt: Option<SystemTime>,
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_success: Option<SystemTime>,
    pub schedule: FetchSchedule,
}

impl FetchHealth {
    pub fn disabled() -> Self {
        Self { last_attempt: None, last_success: None, schedule: FetchSchedule::Disabled }
    }

    pub fn due_now(at: SystemTime) -> Self {
        Self { last_attempt: None, last_success: None, schedule: FetchSchedule::Due(at) }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FetchSchedule {
    Due(
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
    Quarantined {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        since: SystemTime,
        last_error: String,
    },
    Disabled,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Whether this machine can reach anything at all.
///
/// A fact about the machine, not about a repository, and it is the reason the
/// two are kept apart: a repository that keeps refusing is quarantined, and a
/// machine with no network is simply waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Network {
    Up,
    /// Nothing has been reachable since `since`. Automatic fetching of
    /// anything with a remote host is held.
    Down {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        since: SystemTime,
        cause: Outage,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// What put the network verdict down, which is also what lifts it — and so
/// what the user is told is happening in the meantime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outage {
    /// The routing table has nothing off this machine. Nothing is tried; a
    /// route coming back lifts it.
    NoRoute,
    /// There is a route, but fetches found nothing at the end of it. One
    /// repository is asked again at the recheck interval, and only a fetch
    /// that reaches something lifts it.
    Unreachable,
}

impl Network {
    pub fn is_down(self) -> bool {
        self.outage().is_some()
    }

    pub fn outage(self) -> Option<Outage> {
        match self {
            Network::Up => None,
            Network::Down { cause, .. } => Some(cause),
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FetchStatus {
    NoRemote,
    Off,
    Quarantined {
        reason: String,
    },
    BackingOff {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        until: SystemTime,
        failures: u32,
    },
    Fetched {
        #[serde(with = "crate::serde_time")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        at: SystemTime,
    },
    Never,
}

impl FetchStatus {
    pub fn is_problem(&self) -> bool {
        matches!(self, FetchStatus::Quarantined { .. } | FetchStatus::BackingOff { .. })
    }
}
