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
//! Descendants that left the process group (`setsid`, double fork) are found
//! by the `CANIRUN_MANAGED_ID` marker in their environment ([`crate::marker`]):
//! a started child is only free when the child, its group *and* every marked
//! process are gone.
//!
//! A launch intent (the spawn may or may not have happened) is settled the
//! same way: with no lease holder, no marked process, and enough time since
//! the intent, nothing is running that the entry could be charging for. An
//! unknown scan or a missing timestamp never releases it.
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

/// Seconds after a launch intent before it may be settled as "no process
/// is running", so a supervisor that died in the middle of its spawn is not
/// mistaken for one that never spawned.
pub const LAUNCH_INTENT_GRACE_SECS: i64 = 60;

/// Has a launch intent aged enough to be settled? An unknown age has not.
pub fn grace_passed(age_secs: Option<i64>) -> bool {
    age_secs.is_some_and(|a| a >= LAUNCH_INTENT_GRACE_SECS)
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
        /// Seconds since the launch intent was recorded, if it was.
        launch_age_secs: Option<i64>,
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
    /// Any process still carrying the command's `CANIRUN_MANAGED_ID`.
    pub marker: Probe,
    /// A person asked to settle this entry (`canirun resolve`): the grace
    /// period is skipped and an unreadable sidecar is judged by the scan.
    pub manual: bool,
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
    /// The child, its process group and every marked process are gone.
    Stopped,
    /// A launch intent with no lease holder, no marked process, and past the
    /// grace period (or settled by hand): nothing was started, or it is over.
    LaunchSettled,
    /// An unreadable sidecar, settled by hand after a scan found no marked
    /// process.
    NoMarkedProcess,
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
    let (phase, boot, launch_age) = match &i.sidecar {
        SidecarFacts::Missing => return Verdict::Release(Release::Plain),
        SidecarFacts::Corrupt if !i.manual => return Verdict::Keep(Keep::SidecarCorrupt),
        SidecarFacts::Corrupt => {
            return match i.marker {
                Probe::Alive => Verdict::Keep(Keep::DescendantsAlive),
                Probe::Unknown => Verdict::Keep(Keep::ProbeUnknown),
                Probe::Gone => Verdict::Release(Release::NoMarkedProcess),
            }
        }
        SidecarFacts::Managed {
            phase,
            boot_id,
            launch_age_secs,
        } => (*phase, boot_id, *launch_age_secs),
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
        // Too soon (or no timestamp) to tell "never spawned" from "spawned":
        // the scan is not even needed.
        Phase::LaunchIntent if !i.manual && !grace_passed(launch_age) => {
            Verdict::Keep(Keep::LaunchUnknown)
        }
        Phase::LaunchIntent => match i.marker {
            Probe::Alive => Verdict::Keep(Keep::DescendantsAlive),
            Probe::Unknown => Verdict::Keep(Keep::ProbeUnknown),
            Probe::Gone => Verdict::Release(Release::LaunchSettled),
        },
        // Terminal returned above; ChildStarted is what is left.
        Phase::ChildStarted | Phase::Terminal => match (i.child, i.group, i.marker) {
            (Probe::Alive, _, _) | (_, Probe::Alive, _) | (_, _, Probe::Alive) => {
                Verdict::Keep(Keep::DescendantsAlive)
            }
            (Probe::Gone, Probe::Gone, Probe::Gone) => Verdict::Release(Release::Stopped),
            _ => Verdict::Keep(Keep::ProbeUnknown),
        },
    }
}

/// How often an entry kept for want of proof is looked at again.
pub const REASSESS_SECS: i64 = 5;

/// How long a release verdict stays good for acting on. It is acted on
/// within moments of being reached; one that has waited longer is
/// assessed again.
pub const RELEASE_VALID_SECS: i64 = 10;

/// Must the entry be assessed (scanned) again, given its last assessment?
/// `prev` is `(was a release, age in seconds)`; `None` when it was never
/// assessed by this process. A kept entry is looked at again once its last
/// look is `interval_secs` old, so waiters polling twice a second do not
/// rescan every process each time. A release is looked at again as soon as
/// it is too old to act on, and an age below zero (the clock went back) is
/// never trusted.
pub fn reassess_due(prev: Option<(bool, i64)>, interval_secs: i64) -> bool {
    match prev {
        None => true,
        Some((_, age)) if age < 0 => true,
        Some((true, age)) => !release_usable(age),
        Some((false, age)) => age >= interval_secs,
    }
}

/// Is a live process group a *different* one that reused the recorded group
/// number? Only positive evidence says so: a process whose pid is that number
/// (the leader) is alive and started at a different time than the recorded
/// child, which led the original group. No leader, or a start time missing on
/// either side, cannot show reuse (the group may be the original one, with
/// members that carry no marker), so the answer is no.
pub fn group_reused(leader_start: Option<u64>, child_start: Option<u64>) -> bool {
    matches!((leader_start, child_start), (Some(l), Some(c)) if l != c)
}

/// Does a process that has the recorded session id belong to this run's
/// session, or to a *different* session that reused the number? A session id
/// is the pid of its leader, and the kernel hands a pid out again only once
/// the session is empty. So a process with that id is a member of the run's
/// session unless a process now leads it that started at a different time
/// than the recorded child, which led the original session. Counts: the
/// leader alive with the child's start time, and no leader at all (it
/// exited; nobody new has taken the session). A start time missing on either
/// side cannot show reuse, so the answer is yes (the entry stays held).
pub fn session_member(leader_start: Option<u64>, child_start: Option<u64>) -> bool {
    !group_reused(leader_start, child_start)
}

/// What the members of the run's session say: `found` processes carry the
/// recorded session id; `failed` when some process's session could not be
/// read (EPERM and the like), which is treated like an unreadable
/// environment: nothing can be ruled out.
pub fn session_probe(found: usize, failed: bool) -> Probe {
    if found > 0 {
        Probe::Alive
    } else if failed {
        Probe::Unknown
    } else {
        Probe::Gone
    }
}

/// Two probes of the same run's processes taken together: alive if either
/// is, else unknown if either is, else gone.
pub fn merge_probes(a: Probe, b: Probe) -> Probe {
    match (a, b) {
        (Probe::Alive, _) | (_, Probe::Alive) => Probe::Alive,
        (Probe::Unknown, _) | (_, Probe::Unknown) => Probe::Unknown,
        _ => Probe::Gone,
    }
}

/// May a release verdict reached `age_secs` ago be acted on?
pub fn release_usable(age_secs: i64) -> bool {
    (0..RELEASE_VALID_SECS).contains(&age_secs)
}

/// What a supervisor taking over an existing registration does with the
/// boot id the sidecar recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopt {
    /// Take it over as it is.
    Keep,
    /// Nothing was launched: stamp the current boot id and take it over.
    RefreshBoot,
    /// The recorded run began on an earlier boot. Whatever it started is
    /// gone with that boot, so it is settled as interrupted (the id stays used up).
    /// Taking it over with the old boot id would have the reaper's reboot
    /// shortcut free the budget of a child that now runs on this boot.
    SettleStale,
}

/// Pure: decide from the phase and the recorded and current boot ids. An
/// unknown boot id on either side changes nothing, as in [`verdict`].
pub fn adopt(phase: Phase, recorded: Option<&str>, current: Option<&str>) -> Adopt {
    match (recorded, current) {
        (Some(then), Some(now)) if then != now => match phase {
            Phase::Registered => Adopt::RefreshBoot,
            Phase::Terminal => Adopt::Keep,
            Phase::LaunchIntent | Phase::ChildStarted => Adopt::SettleStale,
        },
        _ => Adopt::Keep,
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
            marker: Probe::Gone,
            manual: false,
        }
    }

    fn managed(phase: Phase, boot: Option<&str>) -> SidecarFacts {
        SidecarFacts::Managed {
            phase,
            boot_id: boot.map(String::from),
            launch_age_secs: None,
        }
    }

    fn launch(age: Option<i64>) -> SidecarFacts {
        SidecarFacts::Managed {
            phase: Phase::LaunchIntent,
            boot_id: Some("b1".into()),
            launch_age_secs: age,
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
            i.marker = Probe::Alive;
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
        // Equal ids are no change either.
        i.current_boot = Some("old".into());
        assert_eq!(verdict(&i), Verdict::Keep(Keep::LaunchUnknown));
    }

    #[test]
    fn same_boot_registered_means_nothing_was_spawned() {
        let i = base(managed(Phase::Registered, Some("b1")));
        assert_eq!(verdict(&i), Verdict::Release(Release::NeverLaunched));
    }

    #[test]
    fn a_launch_intent_is_settled_only_by_a_clear_scan_after_the_grace() {
        let at = |age: Option<i64>, marker: Probe| {
            let mut i = base(launch(age));
            i.marker = marker;
            verdict(&i)
        };
        let settled = Verdict::Release(Release::LaunchSettled);
        let young = Verdict::Keep(Keep::LaunchUnknown);
        assert_eq!(at(Some(LAUNCH_INTENT_GRACE_SECS), Probe::Gone), settled);
        assert_eq!(at(Some(LAUNCH_INTENT_GRACE_SECS + 1), Probe::Gone), settled);
        // Too young, negative (clock skew) or unstamped: kept, whatever the
        // scan says.
        for marker in [Probe::Gone, Probe::Unknown, Probe::Alive] {
            assert_eq!(at(Some(LAUNCH_INTENT_GRACE_SECS - 1), marker), young);
            assert_eq!(at(Some(-5), marker), young);
            assert_eq!(at(None, marker), young);
        }
        // Old enough, but the scan cannot say or finds a marked process.
        assert_eq!(
            at(Some(3600), Probe::Unknown),
            Verdict::Keep(Keep::ProbeUnknown)
        );
        assert_eq!(
            at(Some(3600), Probe::Alive),
            Verdict::Keep(Keep::DescendantsAlive)
        );
    }

    #[test]
    fn the_grace_is_sixty_seconds() {
        assert_eq!(LAUNCH_INTENT_GRACE_SECS, 60);
        assert!(!grace_passed(None));
        assert!(!grace_passed(Some(59)));
        assert!(grace_passed(Some(60)));
    }

    #[test]
    fn a_person_settling_a_launch_intent_skips_the_grace_not_the_scan() {
        let mut i = base(launch(None));
        i.manual = true;
        assert_eq!(verdict(&i), Verdict::Release(Release::LaunchSettled));
        i.marker = Probe::Unknown;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::ProbeUnknown));
        i.marker = Probe::Alive;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::DescendantsAlive));
    }

    #[test]
    fn an_unreadable_sidecar_is_settled_by_hand_only_after_a_clear_scan() {
        let mut i = base(SidecarFacts::Corrupt);
        i.manual = true;
        assert_eq!(verdict(&i), Verdict::Release(Release::NoMarkedProcess));
        i.marker = Probe::Unknown;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::ProbeUnknown));
        i.marker = Probe::Alive;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::DescendantsAlive));
        // The reaper alone never does.
        i.manual = false;
        i.marker = Probe::Gone;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::SidecarCorrupt));
    }

    #[test]
    fn a_started_child_is_released_only_when_everything_is_gone() {
        let started = managed(Phase::ChildStarted, Some("b1"));
        let mut i = base(started);
        assert_eq!(verdict(&i), Verdict::Release(Release::Stopped));
        let probes = [Probe::Alive, Probe::Gone, Probe::Unknown];
        for c in probes {
            for g in probes {
                for m in probes {
                    i.child = c;
                    i.group = g;
                    i.marker = m;
                    let want = if [c, g, m].contains(&Probe::Alive) {
                        Verdict::Keep(Keep::DescendantsAlive)
                    } else if [c, g, m].iter().all(|p| *p == Probe::Gone) {
                        Verdict::Release(Release::Stopped)
                    } else {
                        Verdict::Keep(Keep::ProbeUnknown)
                    };
                    assert_eq!(verdict(&i), want, "{c:?} {g:?} {m:?}");
                }
            }
        }
    }

    #[test]
    fn a_marked_descendant_outside_the_group_keeps_the_entry() {
        let mut i = base(managed(Phase::ChildStarted, Some("b1")));
        i.marker = Probe::Alive;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::DescendantsAlive));
        i.marker = Probe::Unknown;
        assert_eq!(verdict(&i), Verdict::Keep(Keep::ProbeUnknown));
    }

    #[test]
    fn adopting_across_a_reboot_depends_on_how_far_the_run_got() {
        use Phase::*;
        let across = |p| adopt(p, Some("b0"), Some("b1"));
        assert_eq!(across(Registered), Adopt::RefreshBoot);
        assert_eq!(across(LaunchIntent), Adopt::SettleStale);
        assert_eq!(across(ChildStarted), Adopt::SettleStale);
        assert_eq!(across(Terminal), Adopt::Keep);
    }

    #[test]
    fn adopting_on_the_same_boot_or_without_a_boot_id_changes_nothing() {
        use Phase::*;
        for p in [Registered, LaunchIntent, ChildStarted, Terminal] {
            assert_eq!(adopt(p, Some("b1"), Some("b1")), Adopt::Keep);
            assert_eq!(adopt(p, None, Some("b1")), Adopt::Keep);
            assert_eq!(adopt(p, Some("b0"), None), Adopt::Keep);
            assert_eq!(adopt(p, None, None), Adopt::Keep);
        }
    }

    #[test]
    fn a_kept_entry_is_reassessed_only_once_the_interval_has_passed() {
        assert!(reassess_due(None, REASSESS_SECS));
        assert!(!reassess_due(Some((false, 0)), REASSESS_SECS));
        assert!(!reassess_due(
            Some((false, REASSESS_SECS - 1)),
            REASSESS_SECS
        ));
        assert!(reassess_due(Some((false, REASSESS_SECS)), REASSESS_SECS));
        assert!(reassess_due(Some((false, 3600)), REASSESS_SECS));
        // Interval 0 (tests, tools that want a fresh look) is always due.
        assert!(reassess_due(Some((false, 0)), 0));
    }

    #[test]
    fn a_release_is_reassessed_once_it_is_too_old_to_act_on() {
        assert!(!reassess_due(Some((true, 0)), REASSESS_SECS));
        assert!(!reassess_due(
            Some((true, RELEASE_VALID_SECS - 1)),
            REASSESS_SECS
        ));
        assert!(reassess_due(
            Some((true, RELEASE_VALID_SECS)),
            REASSESS_SECS
        ));
        assert!(release_usable(0));
        assert!(release_usable(RELEASE_VALID_SECS - 1));
        assert!(!release_usable(RELEASE_VALID_SECS));
        assert!(!release_usable(-1));
    }

    #[test]
    fn a_clock_that_went_back_is_never_trusted() {
        assert!(reassess_due(Some((false, -1)), REASSESS_SECS));
        assert!(reassess_due(Some((true, -1)), REASSESS_SECS));
    }

    #[test]
    fn group_reuse_needs_a_new_leader() {
        // A new leader with another start time: reused.
        assert!(group_reused(Some(200), Some(100)));
        // No leader (the original group, members without a marker): not reused.
        assert!(!group_reused(None, Some(100)));
        // The leader is the recorded child itself.
        assert!(!group_reused(Some(100), Some(100)));
        // Start time unknown on the record: cannot tell.
        assert!(!group_reused(Some(200), None));
        assert!(!group_reused(None, None));
    }

    #[test]
    fn a_session_id_is_ours_unless_a_newer_leader_took_the_number() {
        // The recorded child still leads it, or nobody does.
        assert!(session_member(Some(100), Some(100)));
        assert!(session_member(None, Some(100)));
        // A different process leads the number: reused.
        assert!(!session_member(Some(200), Some(100)));
        // A start time we cannot compare is no proof of reuse.
        assert!(session_member(Some(200), None));
        assert!(session_member(None, None));
    }

    #[test]
    fn the_session_probe_reads_members_before_failures() {
        assert_eq!(session_probe(1, false), Probe::Alive);
        assert_eq!(session_probe(3, true), Probe::Alive);
        assert_eq!(session_probe(0, true), Probe::Unknown);
        assert_eq!(session_probe(0, false), Probe::Gone);
    }

    #[test]
    fn merged_probes_keep_the_stronger_answer() {
        use Probe::*;
        for (a, b, want) in [
            (Alive, Gone, Alive),
            (Gone, Alive, Alive),
            (Alive, Unknown, Alive),
            (Unknown, Alive, Alive),
            (Unknown, Gone, Unknown),
            (Gone, Unknown, Unknown),
            (Gone, Gone, Gone),
            (Unknown, Unknown, Unknown),
        ] {
            assert_eq!(merge_probes(a, b), want, "{a:?} {b:?}");
        }
    }
}
