//! May a ledger entry whose owner lease is gone be released? Pure: the
//! caller gathers the facts (lease, sidecar, boot id, process probes) and
//! this decides.
//!
//! An entry without a sidecar is a plain `canirun exec` entry: when its
//! lease is gone it is released, as it always was. An entry with a sidecar
//! is managed: a vanished lease only means the supervisor died, not that
//! its child process tree stopped using the budget, so it is kept
//! (quarantined, charge held) until the tree is known to be stopped or the
//! machine rebooted.
//!
//! When the boot id cannot be read (on either side) the reboot shortcut is
//! off: only the process checks can release. Nothing is then released
//! across a reboot by accident, and nothing leaks forever because the
//! checks still see that the old processes are gone.

use crate::receipt::Phase;

/// What a process probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Alive,
    Gone,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarFacts {
    /// No sidecar file: a plain entry.
    Missing,
    /// The file exists but cannot be read or parsed.
    Corrupt,
    Managed {
        phase: Phase,
        /// Boot id recorded when the command was registered.
        boot_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub lease_held: bool,
    pub sidecar: SidecarFacts,
    pub current_boot: Option<String>,
    /// The recorded child pid, matched on its start time.
    pub child: Probe,
    /// Anyone left in the recorded process group.
    pub group: Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    LeaseHeld,
    SidecarCorrupt,
    /// Intent recorded, no child identity: a child may exist.
    LaunchUnknown,
    DescendantsAlive,
    /// A probe could not tell.
    ProbeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// A plain entry whose lease is gone.
    Plain,
    /// The terminal outcome was already recorded.
    Terminal,
    /// The machine rebooted since registration.
    BootChanged,
    /// Nothing was ever spawned.
    NeverLaunched,
    /// The child and its process group are gone.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep(Keep),
    Release(Release),
}

impl Verdict {
    pub fn is_release(self) -> bool {
        matches!(self, Verdict::Release(_))
    }
}

pub fn verdict(i: &Inputs) -> Verdict {
    if i.lease_held {
        return Verdict::Keep(Keep::LeaseHeld);
    }
    let (phase, boot) = match &i.sidecar {
        SidecarFacts::Missing => return Verdict::Release(Release::Plain),
        SidecarFacts::Corrupt => return Verdict::Keep(Keep::SidecarCorrupt),
        SidecarFacts::Managed { phase, boot_id } => (*phase, boot_id),
    };
    if phase == Phase::Terminal {
        return Verdict::Release(Release::Terminal);
    }
    if let (Some(then), Some(now)) = (boot, &i.current_boot) {
        if then != now {
            return Verdict::Release(Release::BootChanged);
        }
    }
    match phase {
        Phase::Registered => Verdict::Release(Release::NeverLaunched),
        Phase::LaunchIntent => Verdict::Keep(Keep::LaunchUnknown),
        // Terminal returned above; ChildStarted is what is left.
        Phase::ChildStarted | Phase::Terminal => match (i.child, i.group) {
            (Probe::Alive, _) | (_, Probe::Alive) => Verdict::Keep(Keep::DescendantsAlive),
            (Probe::Gone, Probe::Gone) => Verdict::Release(Release::Stopped),
            _ => Verdict::Keep(Keep::ProbeUnknown),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(sidecar: SidecarFacts) -> Inputs {
        Inputs {
            lease_held: false,
            sidecar,
            current_boot: Some("b1".into()),
            child: Probe::Gone,
            group: Probe::Gone,
        }
    }

    fn managed(phase: Phase, boot: Option<&str>) -> SidecarFacts {
        SidecarFacts::Managed {
            phase,
            boot_id: boot.map(String::from),
        }
    }

    #[test]
    fn a_held_lease_always_keeps() {
        for s in [
            SidecarFacts::Missing,
            SidecarFacts::Corrupt,
            managed(Phase::Terminal, Some("b1")),
        ] {
            let mut i = base(s);
            i.lease_held = true;
            assert_eq!(verdict(&i), Verdict::Keep(Keep::LeaseHeld));
        }
    }

    #[test]
    fn plain_entries_are_released_as_before() {
        let v = verdict(&base(SidecarFacts::Missing));
        assert_eq!(v, Verdict::Release(Release::Plain));
        assert!(v.is_release());
    }

    #[test]
    fn a_corrupt_sidecar_is_never_free_capacity() {
        let mut i = base(SidecarFacts::Corrupt);
        i.current_boot = Some("other".into());
        assert_eq!(verdict(&i), Verdict::Keep(Keep::SidecarCorrupt));
        assert!(!verdict(&i).is_release());
    }

    #[test]
    fn terminal_is_released_whatever_the_probes_say() {
        let mut i = base(managed(Phase::Terminal, Some("b1")));
        i.child = Probe::Alive;
        i.group = Probe::Alive;
        assert_eq!(verdict(&i), Verdict::Release(Release::Terminal));
    }

    #[test]
    fn a_reboot_releases_even_with_a_launch_unknown() {
        for phase in [Phase::Registered, Phase::LaunchIntent, Phase::ChildStarted] {
            let mut i = base(managed(phase, Some("old")));
            i.child = Probe::Alive;
            i.group = Probe::Unknown;
            assert_eq!(verdict(&i), Verdict::Release(Release::BootChanged));
        }
    }

    #[test]
    fn without_a_boot_id_on_either_side_the_shortcut_is_off() {
        let mut i = base(managed(Phase::LaunchIntent, None));
        assert_eq!(verdict(&i), Verdict::Keep(Keep::LaunchUnknown));
        i.sidecar = managed(Phase::LaunchIntent, Some("old"));
        i.current_boot = None;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::LaunchUnknown));
    }

    #[test]
    fn same_boot_registered_means_nothing_was_spawned() {
        let i = base(managed(Phase::Registered, Some("b1")));
        assert_eq!(verdict(&i), Verdict::Release(Release::NeverLaunched));
    }

    #[test]
    fn a_launch_intent_without_a_child_identity_is_kept() {
        let i = base(managed(Phase::LaunchIntent, Some("b1")));
        assert_eq!(verdict(&i), Verdict::Keep(Keep::LaunchUnknown));
    }

    #[test]
    fn a_started_child_is_released_only_when_everything_is_gone() {
        let started = managed(Phase::ChildStarted, Some("b1"));
        let mut i = base(started);
        assert_eq!(verdict(&i), Verdict::Release(Release::Stopped));
        for (c, g, want) in [
            (Probe::Alive, Probe::Gone, Keep::DescendantsAlive),
            (Probe::Gone, Probe::Alive, Keep::DescendantsAlive),
            (Probe::Alive, Probe::Unknown, Keep::DescendantsAlive),
            (Probe::Unknown, Probe::Alive, Keep::DescendantsAlive),
            (Probe::Unknown, Probe::Gone, Keep::ProbeUnknown),
            (Probe::Gone, Probe::Unknown, Keep::ProbeUnknown),
            (Probe::Unknown, Probe::Unknown, Keep::ProbeUnknown),
        ] {
            i.child = c;
            i.group = g;
            assert_eq!(verdict(&i), Verdict::Keep(want), "{c:?} {g:?}");
        }
    }
}
