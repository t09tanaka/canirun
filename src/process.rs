//! Process identity and the probes the reaper uses on a managed command's
//! recorded child. A pid alone is not an identity (pids are reused), so a
//! child is recognised by pid plus start time.

use crate::reap::Probe;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

/// Start time of `pid` in seconds since the epoch, or `None` if there is no
/// such live (non-zombie) process.
pub fn start_time(pid: u32) -> Option<u64> {
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let proc = sys.process(p)?;
    if matches!(proc.status(), ProcessStatus::Zombie) {
        return None;
    }
    Some(proc.start_time())
}

/// How to recognise a spawned child again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildIdentity {
    pub pid: u32,
    pub start_time: Option<u64>,
    /// The child's own process group (a managed child leads one).
    pub pgid: Option<i32>,
}

impl ChildIdentity {
    /// The identity of a just-spawned `pid`.
    pub fn capture(pid: u32) -> Self {
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        ChildIdentity {
            pid,
            start_time: start_time(pid),
            pgid: (pgid > 0).then_some(pgid),
        }
    }
}

/// Is the recorded child itself still running? A pid that now has a
/// different start time is a different process.
pub fn probe_child(c: &ChildIdentity) -> Probe {
    match (start_time(c.pid), c.start_time) {
        (None, _) => Probe::Gone,
        (Some(now), Some(then)) => {
            if now == then {
                Probe::Alive
            } else {
                Probe::Gone
            }
        }
        // Alive but we never learned its start time: cannot tell if it is
        // the same process.
        (Some(_), None) => Probe::Unknown,
    }
}

/// Is anyone left in the process group? `EPERM` still means a process
/// exists.
pub fn probe_group(pgid: Option<i32>) -> Probe {
    let Some(pgid) = pgid else {
        return Probe::Unknown;
    };
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return Probe::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Probe::Gone,
        Some(libc::EPERM) => Probe::Alive,
        _ => Probe::Unknown,
    }
}

/// This machine's boot session id, if the OS gives one.
pub fn system_boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let name = std::ffi::CString::new("kern.bootsessionuuid").ok()?;
        let mut buf = [0u8; 128];
        let mut len: libc::size_t = buf.len();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let s = std::str::from_utf8(&buf[..len]).ok()?;
        let s = s.trim_end_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn we_see_ourselves_and_not_a_missing_pid() {
        let me = std::process::id();
        let id = ChildIdentity::capture(me);
        assert!(id.start_time.is_some());
        assert_eq!(probe_child(&id), Probe::Alive);
        let other = ChildIdentity {
            start_time: id.start_time.map(|t| t + 1000),
            ..id.clone()
        };
        assert_eq!(probe_child(&other), Probe::Gone);
        let unknown = ChildIdentity {
            start_time: None,
            ..id
        };
        assert_eq!(probe_child(&unknown), Probe::Unknown);
        let gone = ChildIdentity {
            pid: 0x3fff_fff0,
            start_time: Some(1),
            pgid: None,
        };
        assert_eq!(probe_child(&gone), Probe::Gone);
    }

    #[test]
    fn a_process_group_is_alive_until_its_members_are_gone() {
        use std::os::unix::process::CommandExt;
        assert_eq!(probe_group(None), Probe::Unknown);
        let mut c = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let id = ChildIdentity::capture(c.id());
        assert_eq!(id.pgid, Some(c.id() as i32));
        assert_eq!(probe_group(id.pgid), Probe::Alive);
        assert_eq!(probe_child(&id), Probe::Alive);
        c.kill().unwrap();
        c.wait().unwrap();
        assert_eq!(probe_group(id.pgid), Probe::Gone);
        assert_eq!(probe_child(&id), Probe::Gone);
    }

    #[test]
    fn the_boot_id_is_stable_and_nonempty_where_the_os_has_one() {
        let a = system_boot_id();
        assert_eq!(a, system_boot_id());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(a.is_some_and(|s| !s.is_empty()));
        }
    }
}
