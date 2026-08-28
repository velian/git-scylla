//! What a failed command looks like on the other side of the boundary.

use serde::Serialize;

/// A structured error, never a stringified `Debug`.
///
/// `kind` is for the UI to branch on; `message` is for a person to read. A
/// `Debug`-formatted Rust error is neither — it cannot be matched on without
/// parsing prose, and it shows the user type names.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "camelCase")]
pub struct BridgeError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ErrorKind {
    /// The engine stopped. Nothing the user can do; the window is dead.
    EngineStopped,
    /// A selection expression the user typed did not parse.
    BadSelection,
    /// The user cancelled a native dialog. Not an error to report, which is why
    /// it is distinguishable from one.
    Cancelled,
    /// Something on the filesystem or in the OS refused. Saving settings,
    /// opening System Settings.
    Io,
    /// The user has not set something the action needs — an editor, so far.
    /// Distinct from a failure so the UI can offer to configure it rather than
    /// just apologising.
    NotConfigured,
}

impl BridgeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

impl From<git_scylla_engine::Gone> for BridgeError {
    fn from(e: git_scylla_engine::Gone) -> Self {
        Self::new(ErrorKind::EngineStopped, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BridgeError>;
