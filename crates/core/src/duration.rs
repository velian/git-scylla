//! Durations, phrased.

use std::time::Duration;

/// A duration in the shortest unit that is not a lie: `45s`, `3m`, `2h`.
pub fn brief(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// How long ago, as a complete phrase: `just now`, `5m ago`, `3h ago`, `2d ago`.
pub fn since(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        s if s < 90 => "just now".into(),
        s if s < 5400 => format!("{}m ago", s / 60),
        s if s < 172_800 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_keeps_seconds_only_while_they_are_the_honest_unit() {
        assert_eq!(brief(Duration::from_secs(0)), "0s");
        assert_eq!(brief(Duration::from_secs(59)), "59s");
        assert_eq!(brief(Duration::from_secs(60)), "1m");
        assert_eq!(brief(Duration::from_secs(5399)), "89m");
        assert_eq!(brief(Duration::from_secs(5400)), "1h");
    }

    #[test]
    fn since_never_needs_a_suffix_added_to_it() {
        assert_eq!(since(Duration::from_secs(0)), "just now");
        assert_eq!(since(Duration::from_secs(89)), "just now");
        assert_eq!(since(Duration::from_secs(90)), "1m ago");
        assert_eq!(since(Duration::from_secs(5400)), "1h ago");
        assert_eq!(since(Duration::from_secs(172_800)), "2d ago");
        for d in [0, 89, 90, 5400, 172_800, 10_000_000] {
            assert!(!since(Duration::from_secs(d)).ends_with(' '));
        }
    }
}
