//! Finding the descendants of a managed command that left its process
//! group. Pure: the OS-specific reader gathers what each process's
//! environment says ([`Seen`]) and this decides.
//!
//! A managed command is started with `CANIRUN_MANAGED_ID=<id>` in its
//! environment. Environments are inherited, so a descendant that called
//! `setsid` or double-forked out of the process group still carries it. A
//! descendant that clears its environment (`env -i`) does not, and is not
//! found.

use crate::reap::Probe;

/// The environment variable a managed command and everything it starts
/// carries.
pub const ENV_VAR: &str = "CANIRUN_MANAGED_ID";

/// A process, identified by pid plus start time (pids are reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcRef {
    pub pid: u32,
    pub start_time: Option<u64>,
}

/// What one process's environment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    Id(String),
    Absent,
    /// The environment could not be read although the process exists.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub proc: ProcRef,
    pub marker: Marker,
}

/// The outcome of looking for the processes marked with one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// `Alive`: a marked process is running. `Gone`: every process was read
    /// and none is marked. `Unknown`: none was found marked, but some
    /// process could not be read.
    pub probe: Probe,
    /// The marked processes found (empty unless `probe` is `Alive`).
    pub alive: Vec<ProcRef>,
}

/// Decide from what was `seen`. The scanning process itself is never
/// counted: it may have been started inside a managed command.
pub fn classify(seen: &[Seen], id: &str, own_pid: u32) -> Scan {
    let mut alive = Vec::new();
    let mut unreadable = false;
    for s in seen.iter().filter(|s| s.proc.pid != own_pid) {
        match &s.marker {
            Marker::Id(found) if found == id => alive.push(s.proc),
            Marker::Unreadable => unreadable = true,
            Marker::Id(_) | Marker::Absent => {}
        }
    }
    let probe = if !alive.is_empty() {
        Probe::Alive
    } else if unreadable {
        Probe::Unknown
    } else {
        Probe::Gone
    };
    Scan { probe, alive }
}

/// The marker in a block of NUL-separated `KEY=VALUE` strings. `skip` bytes
/// of header come first (0 for Linux's `/proc/<pid>/environ`, 4 for the
/// argument count at the front of macOS's `KERN_PROCARGS2`). Every string
/// is looked at, arguments included: a stray argument that looks like the
/// marker keeps a budget held (safe), a parser that miscounted arguments
/// could miss the real one (not safe). The first marker wins, as with
/// `getenv`.
pub fn find_marker(bytes: &[u8], skip: usize) -> Marker {
    let Some(rest) = bytes.get(skip..) else {
        return Marker::Unreadable;
    };
    let prefix = format!("{ENV_VAR}=");
    rest.split(|b| *b == 0)
        .find_map(|s| s.strip_prefix(prefix.as_bytes()))
        .map_or(Marker::Absent, |v| {
            Marker::Id(String::from_utf8_lossy(v).into_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(pid: u32, marker: Marker) -> Seen {
        Seen {
            proc: ProcRef {
                pid,
                start_time: Some(100 + pid as u64),
            },
            marker,
        }
    }

    fn id(s: &str) -> Marker {
        Marker::Id(s.into())
    }

    #[test]
    fn nothing_marked_and_everything_read_is_gone() {
        let s = classify(&[seen(1, Marker::Absent), seen(2, id("other"))], "a", 99);
        assert_eq!(s.probe, Probe::Gone);
        assert!(s.alive.is_empty());
        assert_eq!(classify(&[], "a", 99).probe, Probe::Gone);
    }

    #[test]
    fn a_marked_process_is_alive_and_listed() {
        let s = classify(
            &[
                seen(1, id("a")),
                seen(2, id("b")),
                seen(3, Marker::Absent),
                seen(4, id("a")),
            ],
            "a",
            99,
        );
        assert_eq!(s.probe, Probe::Alive);
        let pids: Vec<u32> = s.alive.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1, 4]);
        assert_eq!(s.alive[0].start_time, Some(101));
    }

    #[test]
    fn an_unreadable_process_makes_a_clear_scan_unknown() {
        let s = classify(
            &[seen(1, Marker::Absent), seen(2, Marker::Unreadable)],
            "a",
            99,
        );
        assert_eq!(s.probe, Probe::Unknown);
        assert!(s.alive.is_empty());
    }

    #[test]
    fn a_marked_process_wins_over_an_unreadable_one() {
        let s = classify(&[seen(1, Marker::Unreadable), seen(2, id("a"))], "a", 99);
        assert_eq!(s.probe, Probe::Alive);
        assert_eq!(s.alive.len(), 1);
    }

    #[test]
    fn the_scanner_itself_is_never_counted() {
        let s = classify(&[seen(7, id("a")), seen(7, Marker::Unreadable)], "a", 7);
        assert_eq!(s.probe, Probe::Gone);
        assert!(s.alive.is_empty());
    }

    #[test]
    fn the_marker_is_found_among_the_strings() {
        let env = b"PATH=/bin\0CANIRUN_MANAGED_ID=run-1\0HOME=/x\0";
        assert_eq!(find_marker(env, 0), id("run-1"));
        assert_eq!(find_marker(b"PATH=/bin\0", 0), Marker::Absent);
        assert_eq!(find_marker(b"", 0), Marker::Absent);
        // The first one wins; a similar name or an empty value is exact.
        assert_eq!(
            find_marker(b"CANIRUN_MANAGED_ID=a\0CANIRUN_MANAGED_ID=b\0", 0),
            id("a")
        );
        assert_eq!(find_marker(b"CANIRUN_MANAGED_IDX=a\0", 0), Marker::Absent);
        assert_eq!(find_marker(b"CANIRUN_MANAGED_ID=\0", 0), id(""));
    }

    #[test]
    fn a_header_is_skipped_but_needs_to_exist() {
        let mut args = vec![2, 0, 0, 0];
        args.extend_from_slice(b"/bin/sh\0\0sh\0-c\0CANIRUN_MANAGED_ID=m\0");
        assert_eq!(find_marker(&args, 4), id("m"));
        // The skipped bytes are not searched.
        assert_eq!(
            find_marker(b"CANIRUN_MANAGED_ID=m\0", "CANIRUN_MANAGED_ID=m\0".len()),
            Marker::Absent
        );
        assert_eq!(find_marker(&[1, 0], 4), Marker::Unreadable);
        assert_eq!(find_marker(&[1, 0, 0, 0], 4), Marker::Absent);
    }
}
