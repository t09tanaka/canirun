//! Finding the descendants of a managed command that left its process
//! group. Pure: the OS-specific reader gathers what each process's
//! environment says ([`Seen`]) and this decides.
//!
//! A managed command is started with two variables in its environment:
//! `TASK_TRAFFIC_CONTROLLER_MANAGED_ID=<id>` and `TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=<scope>`, where the
//! scope names the state directory the command was admitted by. Command ids
//! are chosen by the caller, so the same id can exist in two state
//! directories; a process counts only when both match. Environments are
//! inherited, so a descendant that called `setsid` or double-forked out of
//! the process group still carries them. A descendant that clears its
//! environment (`env -i`) does not, and is not found. A managed command
//! started inside another one overwrites the outer id and scope, so only
//! the innermost is tracked.

use crate::reap::Probe;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The environment variable a managed command and everything it starts
/// carries.
pub const ENV_VAR: &str = "TASK_TRAFFIC_CONTROLLER_MANAGED_ID";
/// The state directory the command was admitted by (see [`scope`]).
pub const SCOPE_VAR: &str = "TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE";

/// The scope of a state directory: the first 16 hex digits of the SHA-256
/// of its canonical path (the path as given when it cannot be resolved).
pub fn scope(state_dir: &Path) -> String {
    let canon = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    let digest = Sha256::digest(canon.as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A process, identified by pid plus start time (pids are reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcRef {
    pub pid: u32,
    pub start_time: Option<u64>,
}

/// What one process's environment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// It carries the id variable; `scope` is the scope variable, if any.
    Marked {
        id: String,
        scope: Option<String>,
    },
    Absent,
    /// The OS shows the arguments but withholds the environment (or it is
    /// empty: the two cannot be told apart). On macOS this is every Apple
    /// platform binary and hardened-runtime app. It says nothing about
    /// the marker, so it never makes a scan unknown, but it is not proof of
    /// absence either (see [`suspicious_hidden`]).
    Hidden,
    /// The environment could not be read although the process exists, and
    /// the cause is not one of the above (a garbled buffer).
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub proc: ProcRef,
    /// The parent pid, if the OS gave one.
    pub parent: Option<u32>,
    pub marker: Marker,
}

/// A process whose environment is hidden, with its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HiddenProc {
    pub proc: ProcRef,
    pub parent: Option<u32>,
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
    /// Processes whose environment is hidden from us.
    pub hidden: Vec<HiddenProc>,
}

impl Scan {
    /// A scan result that saw no hidden process.
    pub fn new(probe: Probe, alive: Vec<ProcRef>) -> Self {
        Scan {
            probe,
            alive,
            hidden: Vec::new(),
        }
    }
}

/// Decide from what was `seen`. A process counts only when its id and its
/// scope both match; one marked for another state directory is somebody
/// else's. The scanning process itself is never counted: it may have been
/// started inside a managed command.
pub fn classify(seen: &[Seen], id: &str, scope: &str, own_pid: u32) -> Scan {
    let mut alive = Vec::new();
    let mut hidden = Vec::new();
    let mut unreadable = false;
    for s in seen.iter().filter(|s| s.proc.pid != own_pid) {
        match &s.marker {
            Marker::Marked { id: i, scope: sc } if i == id && sc.as_deref() == Some(scope) => {
                alive.push(s.proc)
            }
            Marker::Unreadable => unreadable = true,
            Marker::Hidden => hidden.push(HiddenProc {
                proc: s.proc,
                parent: s.parent,
            }),
            Marker::Marked { .. } | Marker::Absent => {}
        }
    }
    let probe = if !alive.is_empty() {
        Probe::Alive
    } else if unreadable {
        Probe::Unknown
    } else {
        Probe::Gone
    };
    Scan {
        probe,
        alive,
        hidden,
    }
}

/// The marker in a block of NUL-separated `KEY=VALUE` strings (Linux's
/// `/proc/<pid>/environ`, or the environment part of [`parse_procargs`]).
/// The first of each variable wins, as with `getenv`.
pub fn find_marker(env: &[u8]) -> Marker {
    let var = |name: &str| {
        let prefix = format!("{name}=");
        env.split(|b| *b == 0)
            .find_map(|s| s.strip_prefix(prefix.as_bytes()))
            .map(|v| String::from_utf8_lossy(v).into_owned())
    };
    match var(ENV_VAR) {
        Some(id) => Marker::Marked {
            id,
            scope: var(SCOPE_VAR),
        },
        None => Marker::Absent,
    }
}

/// The marker in macOS's `KERN_PROCARGS2` buffer: an `i32` argument count,
/// the executable path (NUL-padded), `argc` argument strings, then the
/// environment. Only the environment is searched: an argument that looks
/// like the marker is not one. An environment of no strings at all is
/// [`Marker::Hidden`] (the OS withholds it, or it is truly empty). A buffer
/// that ends before the arguments do is unreadable.
pub fn parse_procargs(buf: &[u8]) -> Marker {
    let Some(head) = buf.get(..4) else {
        return Marker::Unreadable;
    };
    let argc = i32::from_ne_bytes([head[0], head[1], head[2], head[3]]);
    let Ok(argc) = usize::try_from(argc) else {
        return Marker::Unreadable;
    };
    let rest = &buf[4..];
    let Some(path_end) = rest.iter().position(|b| *b == 0) else {
        return Marker::Unreadable;
    };
    let rest = &rest[path_end..];
    let padding = rest.iter().take_while(|b| **b == 0).count();
    let rest = &rest[padding..];
    let mut off = 0;
    for _ in 0..argc {
        let Some(nul) = rest[off..].iter().position(|b| *b == 0) else {
            return Marker::Unreadable;
        };
        off += nul + 1;
    }
    let env = &rest[off..];
    if env.iter().all(|b| *b == 0) {
        return Marker::Hidden;
    }
    find_marker(env)
}

/// The marker to report when macOS refused `KERN_PROCARGS2` for a process
/// that has not vanished. Only `EINVAL` and `EPERM` are the OS withholding the
/// environment of a process of ours (an Apple platform binary, a hardened
/// runtime app), and only when the kernel confirmed the owner is us
/// (`owned`): that is [`Marker::Hidden`]. Anything else (`ENOMEM`, `EIO`,
/// no errno at all) is a failure to look, so the process is
/// [`Marker::Unreadable`] and the scan unknown.
pub fn procargs_failure(errno: Option<i32>, owned: bool) -> Marker {
    match errno {
        Some(libc::EINVAL | libc::EPERM) if owned => Marker::Hidden,
        _ => Marker::Unreadable,
    }
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
            parent: Some(1000 + pid),
            marker,
        }
    }

    fn marked(id: &str, scope: &str) -> Marker {
        Marker::Marked {
            id: id.into(),
            scope: Some(scope.into()),
        }
    }

    #[test]
    fn nothing_marked_and_everything_read_is_gone() {
        let s = classify(
            &[seen(1, Marker::Absent), seen(2, marked("other", "s"))],
            "a",
            "s",
            99,
        );
        assert_eq!(s.probe, Probe::Gone);
        assert!(s.alive.is_empty());
        assert_eq!(classify(&[], "a", "s", 99).probe, Probe::Gone);
    }

    #[test]
    fn a_marked_process_is_alive_and_listed() {
        let s = classify(
            &[
                seen(1, marked("a", "s")),
                seen(2, marked("b", "s")),
                seen(3, Marker::Absent),
                seen(4, marked("a", "s")),
            ],
            "a",
            "s",
            99,
        );
        assert_eq!(s.probe, Probe::Alive);
        let pids: Vec<u32> = s.alive.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1, 4]);
        assert_eq!(s.alive[0].start_time, Some(101));
    }

    #[test]
    fn the_same_id_in_another_state_dir_is_somebody_elses() {
        let s = classify(
            &[
                seen(1, marked("a", "other-scope")),
                seen(
                    2,
                    Marker::Marked {
                        id: "a".into(),
                        scope: None,
                    },
                ),
                seen(3, marked("a", "mine")),
            ],
            "a",
            "mine",
            99,
        );
        assert_eq!(s.probe, Probe::Alive);
        assert_eq!(s.alive.len(), 1);
        assert_eq!(s.alive[0].pid, 3);
        let none = classify(&[seen(1, marked("a", "other-scope"))], "a", "mine", 99);
        assert_eq!(none.probe, Probe::Gone);
        assert!(none.alive.is_empty());
    }

    #[test]
    fn an_unreadable_process_makes_a_clear_scan_unknown() {
        let s = classify(
            &[seen(1, Marker::Absent), seen(2, Marker::Unreadable)],
            "a",
            "s",
            99,
        );
        assert_eq!(s.probe, Probe::Unknown);
        assert!(s.alive.is_empty());
    }

    #[test]
    fn a_marked_process_wins_over_an_unreadable_one() {
        let s = classify(
            &[seen(1, Marker::Unreadable), seen(2, marked("a", "s"))],
            "a",
            "s",
            99,
        );
        assert_eq!(s.probe, Probe::Alive);
        assert_eq!(s.alive.len(), 1);
    }

    #[test]
    fn the_scanner_itself_is_never_counted() {
        let s = classify(
            &[seen(7, marked("a", "s")), seen(7, Marker::Unreadable)],
            "a",
            "s",
            7,
        );
        assert_eq!(s.probe, Probe::Gone);
        assert!(s.alive.is_empty());
    }

    #[test]
    fn the_marker_is_found_among_the_strings() {
        let env = b"PATH=/bin\0TASK_TRAFFIC_CONTROLLER_MANAGED_ID=run-1\0TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=abc\0HOME=/x\0";
        assert_eq!(find_marker(env), marked("run-1", "abc"));
        assert_eq!(find_marker(b"PATH=/bin\0"), Marker::Absent);
        assert_eq!(find_marker(b""), Marker::Absent);
        // The scope alone is not a marker; without it the id has none.
        assert_eq!(
            find_marker(b"TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=abc\0"),
            Marker::Absent
        );
        assert_eq!(
            find_marker(b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=a\0"),
            Marker::Marked {
                id: "a".into(),
                scope: None
            }
        );
        // The first one wins; a similar name or an empty value is exact.
        assert_eq!(
            find_marker(
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=a\0TASK_TRAFFIC_CONTROLLER_MANAGED_ID=b\0TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=1\0TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=2\0"
            ),
            marked("a", "1")
        );
        assert_eq!(
            find_marker(b"TASK_TRAFFIC_CONTROLLER_MANAGED_IDX=a\0"),
            Marker::Absent
        );
        assert_eq!(
            find_marker(b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=\0"),
            Marker::Marked {
                id: String::new(),
                scope: None
            }
        );
    }

    fn procargs(argc: i32, exec: &[u8], strings: &[&[u8]]) -> Vec<u8> {
        let mut v = argc.to_ne_bytes().to_vec();
        v.extend_from_slice(exec);
        v.extend_from_slice(&[0, 0, 0]);
        for s in strings {
            v.extend_from_slice(s);
            v.push(0);
        }
        v
    }

    #[test]
    fn procargs_find_the_marker_in_the_environment() {
        let buf = procargs(
            2,
            b"/bin/x",
            &[
                b"x",
                b"-c",
                b"PATH=/bin",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=m",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=s",
            ],
        );
        assert_eq!(parse_procargs(&buf), marked("m", "s"));
    }

    #[test]
    fn an_argument_that_looks_like_the_marker_does_not_count() {
        // The marker only in argv: not a marker.
        let only_arg = procargs(
            2,
            b"/bin/x",
            &[
                b"x",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=other",
                b"PATH=/bin",
            ],
        );
        assert_eq!(parse_procargs(&only_arg), Marker::Absent);
        // A decoy in argv in front of the real one.
        let both = procargs(
            2,
            b"/bin/x",
            &[
                b"x",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=other",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=real",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=s",
            ],
        );
        assert_eq!(parse_procargs(&both), marked("real", "s"));
        // An empty argument still counts as an argument.
        let empty_arg = procargs(
            3,
            b"/bin/x",
            &[
                b"x",
                b"",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=other",
                b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=r",
            ],
        );
        assert_eq!(
            parse_procargs(&empty_arg),
            Marker::Marked {
                id: "r".into(),
                scope: None
            }
        );
    }

    #[test]
    fn an_empty_or_withheld_environment_is_hidden_not_absent() {
        let buf = procargs(2, b"/bin/sh", &[b"sh", b"-c"]);
        assert_eq!(parse_procargs(&buf), Marker::Hidden);
        let none = procargs(0, b"/bin/sh", &[]);
        assert_eq!(parse_procargs(&none), Marker::Hidden);
        // Only padding after the arguments is still no environment, and a
        // string with no terminator is still a string.
        let mut padded = procargs(1, b"/bin/x", &[b"x"]);
        padded.extend_from_slice(&[0, 0, 0]);
        assert_eq!(parse_procargs(&padded), Marker::Hidden);
        let mut bare = procargs(1, b"/bin/x", &[b"x"]);
        bare.extend_from_slice(b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=u");
        assert_eq!(
            parse_procargs(&bare),
            Marker::Marked {
                id: "u".into(),
                scope: None
            }
        );
        let mut other = procargs(1, b"/bin/x", &[b"x"]);
        other.extend_from_slice(b"PATH=/bin");
        assert_eq!(parse_procargs(&other), Marker::Absent);
        // An environment with strings but no marker is a real absence.
        let plain = procargs(1, b"/bin/x", &[b"x", b"PATH=/bin"]);
        assert_eq!(parse_procargs(&plain), Marker::Absent);
    }

    #[test]
    fn a_hidden_process_never_makes_a_scan_unknown_but_is_listed() {
        let s = classify(
            &[
                seen(1, Marker::Hidden),
                seen(2, Marker::Absent),
                seen(7, Marker::Hidden),
            ],
            "a",
            "s",
            7,
        );
        assert_eq!(s.probe, Probe::Gone);
        assert_eq!(s.hidden.len(), 1);
        assert_eq!(s.hidden[0].proc.pid, 1);
        assert_eq!(s.hidden[0].parent, Some(1001));
        assert_eq!(s.hidden[0].proc.start_time, Some(101));
        // With a marked process the scan is still alive.
        let m = classify(
            &[seen(1, Marker::Hidden), seen(2, marked("a", "s"))],
            "a",
            "s",
            99,
        );
        assert_eq!(m.probe, Probe::Alive);
        assert_eq!(m.hidden.len(), 1);
        assert!(Scan::new(Probe::Gone, vec![]).hidden.is_empty());
    }

    #[test]
    fn only_a_refusal_for_our_own_process_is_hidden() {
        for e in [libc::EINVAL, libc::EPERM] {
            assert_eq!(procargs_failure(Some(e), true), Marker::Hidden);
            assert_eq!(procargs_failure(Some(e), false), Marker::Unreadable);
        }
        for e in [libc::ENOMEM, libc::EIO, libc::EACCES, libc::ESRCH, 0] {
            assert_eq!(procargs_failure(Some(e), true), Marker::Unreadable);
        }
        assert_eq!(procargs_failure(None, true), Marker::Unreadable);
    }

    #[test]
    fn a_truncated_buffer_is_unreadable() {
        let full = procargs(
            2,
            b"/bin/x",
            &[b"x", b"-c", b"TASK_TRAFFIC_CONTROLLER_MANAGED_ID=m"],
        );
        // No count, no end of the path, fewer arguments than the count.
        assert_eq!(parse_procargs(&[]), Marker::Unreadable);
        assert_eq!(parse_procargs(&full[..3]), Marker::Unreadable);
        assert_eq!(parse_procargs(&full[..8]), Marker::Unreadable);
        let one_arg = procargs(2, b"/bin/x", &[b"x"]);
        assert_eq!(parse_procargs(&one_arg), Marker::Unreadable);
        let half_arg = &full[..full.len() - "c\0TASK_TRAFFIC_CONTROLLER_MANAGED_ID=m\0".len()];
        assert_eq!(parse_procargs(half_arg), Marker::Unreadable);
        // A negative count is garbage.
        let mut bad = full.clone();
        bad[..4].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(parse_procargs(&bad), Marker::Unreadable);
    }

    #[test]
    fn the_scope_is_16_hex_digits_of_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = scope(dir.path());
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        // A path with a `..` or a symlink resolves to the same scope.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(scope(&sub.join("..")), a);
        assert_ne!(scope(&sub), a);
        // A path that does not exist is hashed as given, and differs.
        let missing = dir.path().join("missing");
        assert_eq!(scope(&missing), scope(&missing));
        assert_ne!(scope(&missing), a);
    }
}
