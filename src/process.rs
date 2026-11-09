//! Process identity and the probes the reaper uses on a managed command's
//! recorded child. A pid alone is not an identity (pids are reused), so a
//! child is recognised by pid plus start time.

use crate::marker::{self, Marker, ProcRef, Scan, Seen};
use crate::reap::Probe;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

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

/// Send `sig` to `p` only if the pid still belongs to the process that was
/// seen (same start time): a reused pid is somebody else. A process whose
/// start time was never learned is not signalled.
pub fn signal_if_same(p: &ProcRef, sig: libc::c_int) -> bool {
    let Some(then) = p.start_time else {
        return false;
    };
    if p.pid <= 1 || start_time(p.pid) != Some(then) {
        return false;
    }
    unsafe { libc::kill(p.pid as libc::pid_t, sig) == 0 }
}

/// Look for processes of this user carrying `CANIRUN_MANAGED_ID=<id>`.
pub fn scan_system(id: &str) -> Scan {
    marker::classify(&observe_all(), id, std::process::id())
}

/// The environment marker of every live, non-zombie process of this user.
/// A process that exits while being looked at is left out; one that cannot
/// be read is reported as such.
fn observe_all() -> Vec<Seen> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_user(UpdateKind::OnlyIfNotSet),
    );
    let me = unsafe { libc::getuid() };
    let mut out = Vec::new();
    for (pid, p) in sys.processes() {
        #[cfg(target_os = "linux")]
        if p.thread_kind().is_some() {
            continue;
        }
        if matches!(p.status(), ProcessStatus::Zombie) || p.user_id().is_none_or(|u| **u != me) {
            continue;
        }
        let pid = pid.as_u32();
        let Some(marker) = read_marker(pid) else {
            continue;
        };
        out.push(Seen {
            proc: ProcRef {
                pid,
                start_time: Some(p.start_time()),
            },
            marker,
        });
    }
    out
}

/// Did `pid` vanish since it was listed? A process that became a zombie
/// meanwhile counts: its environment is unreadable but it runs nothing.
fn vanished(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    (rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
        || start_time(pid).is_none()
}

/// `None` when the process is gone.
#[cfg(target_os = "linux")]
fn read_marker(pid: u32) -> Option<Marker> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => Some(marker::find_marker(&bytes, 0)),
        Err(_) if vanished(pid) => None,
        // The kernel withholds the environment of a process that made itself
        // non-dumpable (an agent, a sandboxed helper). A descendant of a
        // command is not one in practice; reading it as unmarked keeps one
        // such bystander from blocking every release.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(Marker::Absent),
        Err(_) => Some(Marker::Unreadable),
    }
}

/// `None` when the process is gone.
#[cfg(target_os = "macos")]
fn read_marker(pid: u32) -> Option<Marker> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut len: libc::size_t = 0;
    let sized = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let mut buf = vec![0u8; len + 4096];
    len = buf.len();
    let rc = if sized == 0 {
        unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        }
    } else {
        -1
    };
    if rc != 0 {
        let denied = std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        return if vanished(pid) {
            None
        } else if denied {
            Some(Marker::Absent)
        } else {
            Some(Marker::Unreadable)
        };
    }
    buf.truncate(len);
    // The argument count (an i32) comes first. Apple's own binaries (`sh`,
    // `sleep`, ...) hide their environment from other processes: their block
    // ends after the arguments, which reads as no marker.
    Some(marker::find_marker(&buf, 4))
}

/// No reader for this OS: nothing can be ruled out.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_marker(pid: u32) -> Option<Marker> {
    (!vanished(pid)).then_some(Marker::Unreadable)
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

    /// Runs only when re-executed by the test below: a process that
    /// blocks. The test executable is used because, on macOS, Apple's own
    /// binaries (`sleep`) do not show their environment to anyone.
    #[test]
    fn marker_sleeper_entry() {
        if std::env::var_os("CANIRUN_TEST_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[test]
    fn a_marked_process_is_found_until_it_dies() {
        let id = format!("scan-test-{}", std::process::id());
        let mut c = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::marker_sleeper_entry",
                "--nocapture",
            ])
            .env("CANIRUN_TEST_SLEEPER", "1")
            .env(marker::ENV_VAR, &id)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let found = scan_system(&id);
        assert_eq!(found.probe, Probe::Alive, "{found:?}");
        assert_eq!(found.alive.len(), 1);
        assert_eq!(found.alive[0].pid, c.id());
        assert!(found.alive[0].start_time.is_some());
        assert_eq!(scan_system(&format!("{id}-other")).probe, Probe::Gone);
        // Only a pid that is still the same process is signalled.
        let mut stale = found.alive[0];
        stale.start_time = stale.start_time.map(|t| t + 1000);
        assert!(!signal_if_same(&stale, libc::SIGKILL));
        stale.start_time = None;
        assert!(!signal_if_same(&stale, libc::SIGKILL));
        assert!(c.try_wait().unwrap().is_none());
        assert!(signal_if_same(&found.alive[0], libc::SIGKILL));
        c.wait().unwrap();
        assert_eq!(scan_system(&id).probe, Probe::Gone);
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
