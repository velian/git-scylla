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
