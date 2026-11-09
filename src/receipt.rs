//! The stages a managed command goes through, and which moves between them
//! are legal. Pure: no I/O.
//!
//! `accepted` (the caller's own durable record) happens before canirun is
//! involved and has no stage here. From `Registered` on, canirun holds the
//! lease and a ledger entry:
//!
//! ```text
//! Registered -> LaunchIntent -> ChildStarted -> Terminal
//!      \______________\______________\_________/
//! ```
//!
//! Any non-terminal stage may jump straight to `Terminal` (cancelled,
//! spawn failed, reaped). `ChildStarted` is only reachable through
//! `LaunchIntent`, so a child never exists without a persisted intent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Lease held and ledger entry present; nothing spawned.
    Registered,
    /// About to spawn (persisted before the spawn). Whether a child exists
    /// is unknown from here.
    LaunchIntent,
    /// The child's identity (pid, start time, process group) is recorded.
    ChildStarted,
    /// Exit, stop and cleanup are settled; the entry may be released.
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Already there; nothing to write.
    Noop,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: Phase,
    pub to: Phase,
}

impl std::fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "illegal transition {:?} -> {:?}", self.from, self.to)
    }
}

/// Whether `from -> to` is allowed, and whether it changes anything.
pub fn advance(from: Phase, to: Phase) -> Result<Advance, IllegalTransition> {
    use Phase::*;
    match (from, to) {
        (a, b) if a == b => Ok(Advance::Noop),
        (Registered, LaunchIntent) | (LaunchIntent, ChildStarted) => Ok(Advance::Forward),
        (Registered | LaunchIntent | ChildStarted, Terminal) => Ok(Advance::Forward),
        _ => Err(IllegalTransition { from, to }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Phase::*;

    #[test]
    fn forward_moves_are_legal() {
        assert_eq!(advance(Registered, LaunchIntent), Ok(Advance::Forward));
        assert_eq!(advance(LaunchIntent, ChildStarted), Ok(Advance::Forward));
        for from in [Registered, LaunchIntent, ChildStarted] {
            assert_eq!(advance(from, Terminal), Ok(Advance::Forward));
        }
    }

    #[test]
    fn same_stage_is_a_noop() {
        for p in [Registered, LaunchIntent, ChildStarted, Terminal] {
            assert_eq!(advance(p, p), Ok(Advance::Noop));
        }
    }

    #[test]
    fn nothing_moves_backwards_or_skips_the_intent() {
        assert!(advance(Registered, ChildStarted).is_err());
        assert!(advance(LaunchIntent, Registered).is_err());
        assert!(advance(ChildStarted, LaunchIntent).is_err());
        assert!(advance(ChildStarted, Registered).is_err());
        for to in [Registered, LaunchIntent, ChildStarted] {
            let e = advance(Terminal, to).unwrap_err();
            assert_eq!((e.from, e.to), (Terminal, to));
        }
        assert!(advance(Registered, ChildStarted)
            .unwrap_err()
            .to_string()
            .contains("illegal transition"));
    }
}
