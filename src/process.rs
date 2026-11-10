//! Process identity and the probes the reaper uses on a managed command's
//! recorded child. A pid alone is not an identity (pids are reused), so a
//! child is recognised by pid plus start time.

use crate::lineage::{self, Row};
use crate::marker::{self, Marker, ProcRef, Scan, Seen};
use crate::reap::{self, Probe};
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
    let Some(proc) = sys.process(p) else {
        // sysinfo may not list a process that exists: ask the kernel.
        return kernel_start_time(pid);
    };
    if matches!(proc.status(), ProcessStatus::Zombie) {
        return None;
    }
    Some(proc.start_time())
}

/// The start time of a live, non-zombie `pid` from the kernel (macOS:
/// `proc_pidinfo`, whose short form has no start time, so only the full
/// one is asked), in seconds since the epoch.
#[cfg(target_os = "macos")]
fn kernel_start_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size && info.pbi_status != libc::SZOMB && info.pbi_start_tvsec != 0)
        .then_some(info.pbi_start_tvsec)
}

#[cfg(not(target_os = "macos"))]
fn kernel_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Has our child `pid` ended without being reaped yet (a zombie)? Asked of
/// the kernel without reaping it: the process table may not list a zombie.
pub fn has_ended(pid: u32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    let who = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let who = info.si_pid;
    who as u32 == pid
}

/// Is our child `pid` gone: a zombie, or already reaped (no such process)?
/// False while it runs, so a process table that does not list it is then a
/// failed read, not an exit.
pub fn root_gone(pid: u32) -> bool {
    has_ended(pid)
        || (unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

/// How to recognise a spawned child again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildIdentity {
    pub pid: u32,
    pub start_time: Option<u64>,
    /// The child's own process group (a managed child leads one).
    pub pgid: Option<i32>,
    /// The child's own session (a managed child leads one, started with
    /// `setsid`), so its id is the child's pid. `env -i` cannot leave a
    /// session; only a deliberate `setsid` can. Absent in older sidecars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<i32>,
}

impl ChildIdentity {
    /// The identity of a just-spawned `pid`.
    pub fn capture(pid: u32) -> Self {
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        ChildIdentity {
            pid,
            start_time: start_time(pid),
            pgid: (pgid > 0).then_some(pgid),
            sid: session_of(pid).filter(|s| *s > 0),
        }
    }
}

/// The session id of `pid`, or `None` when the kernel will not say.
fn session_of(pid: u32) -> Option<i32> {
    session_lookup(pid).ok()
}

/// `getsid(pid)`: `Err(true)` when the process is gone, `Err(false)` for any
/// other failure (`EPERM`, ...).
fn session_lookup(pid: u32) -> Result<i32, bool> {
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    if sid >= 0 {
        return Ok(sid);
    }
    Err(std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

/// Every live, non-zombie process (other than this one) whose session is
/// `sid`, and whether some process's session could not be read.
pub fn session_members(sid: i32) -> (Vec<ProcRef>, bool) {
    let me = std::process::id();
    let mut found = Vec::new();
    let mut failed = false;
    for row in TableReader::new().read() {
        if row.proc.pid == me {
            continue;
        }
        match session_lookup(row.proc.pid) {
            Ok(s) if s == sid => found.push(row.proc),
            Ok(_) => {}
            Err(true) => {}
            Err(false) => failed = true,
        }
    }
    (found, failed)
}

/// Does anyone still belong to the run's session? A session id is a pid, so
/// it only counts while it is still the child's session (see
/// [`reap::session_member`]). A child recorded without a session (an older
/// sidecar) has none to look at: gone.
pub fn probe_session(c: &ChildIdentity) -> Probe {
    let Some(sid) = c.sid else {
        return Probe::Gone;
    };
    let leader = u32::try_from(sid).ok().and_then(start_time);
    if !reap::session_member(leader, c.start_time) {
        return Probe::Gone;
    }
    let (found, failed) = session_members(sid);
    reap::session_probe(found.len(), failed)
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

/// Look for processes of this user carrying `CANIRUN_MANAGED_ID=<id>` and
/// `CANIRUN_MANAGED_SCOPE=<scope>`.
pub fn scan_system(id: &str, scope: &str) -> Scan {
    marker::classify(&observe_all(), id, scope, std::process::id())
}

/// The process table: pid, start time and parent of every live,
/// non-zombie process. Reuse the reader to keep the allocations.
pub struct TableReader {
    sys: System,
}

impl Default for TableReader {
    fn default() -> Self {
        Self::new()
    }
}

impl TableReader {
    pub fn new() -> Self {
        TableReader { sys: System::new() }
    }

    pub fn read(&mut self) -> Vec<Row> {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        rows(&self.sys)
    }
}

fn rows(sys: &System) -> Vec<Row> {
    sys.processes()
        .iter()
        .filter(|(_, p)| !matches!(p.status(), ProcessStatus::Zombie) && !is_thread(p))
        .map(|(pid, p)| Row {
            proc: ProcRef {
                pid: pid.as_u32(),
                start_time: Some(p.start_time()),
            },
            parent: p.parent().map(|x| x.as_u32()),
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn is_thread(p: &sysinfo::Process) -> bool {
    p.thread_kind().is_some()
}

#[cfg(not(target_os = "linux"))]
fn is_thread(_: &sysinfo::Process) -> bool {
    false
}

/// Which of `recorded` are still running (same pid and start time).
pub fn alive_among(recorded: &[ProcRef]) -> Vec<ProcRef> {
    let pids: Vec<Pid> = recorded.iter().map(|p| Pid::from_u32(p.pid)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing(),
    );
    lineage::alive_in(&rows(&sys), recorded)
}

/// The owner of `pid`, from the kernel (macOS: `proc_pidinfo`).
#[cfg(target_os = "macos")]
fn kernel_uid(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if got == size {
        return Some(info.pbi_uid);
    }
    // The short form is readable for processes the full one refuses (root
    // daemons), and is enough for the owner.
    let mut short: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut short as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    (got == size).then_some(short.pbsi_uid)
}

#[cfg(not(target_os = "macos"))]
fn kernel_uid(_pid: u32) -> Option<u32> {
    None
}

/// The environment marker of every live, non-zombie process of this user.
/// A process of another user is left out (it cannot be ours); one that
/// exits while being looked at is left out too; one of ours whose
/// environment is withheld is reported as hidden.
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
        if is_thread(p) || matches!(p.status(), ProcessStatus::Zombie) {
            continue;
        }
        let pid = pid.as_u32();
        // The kernel's answer first (sysinfo reports no owner for another
        // user's process on macOS, which would make it look like ours).
        // A process whose owner is not known is read anyway.
        let kernel = kernel_uid(pid);
        match kernel.or_else(|| p.user_id().map(|u| **u)) {
            Some(u) if u != me => continue,
            _ => {}
        }
        let Some(marker) = read_marker(pid, kernel == Some(me)) else {
            continue;
        };
        out.push(Seen {
            proc: ProcRef {
                pid,
                start_time: Some(p.start_time()),
            },
            parent: p.parent().map(|x| x.as_u32()),
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
fn read_marker(pid: u32, _owned: bool) -> Option<Marker> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => Some(marker::find_marker(&bytes)),
        Err(_) if vanished(pid) => None,
        // The kernel withholds the environment of a process that made itself
        // non-dumpable (an agent, a sandboxed helper). A descendant of a
        // command is not one in practice; reading it as unmarked keeps one
        // such bystander from blocking every release.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(Marker::Absent),
        Err(_) => Some(Marker::Unreadable),
    }
}

/// `None` when the process is gone. A process of ours whose environment the
/// kernel will not give (an Apple platform binary, a hardened-runtime app)
/// is [`Marker::Hidden`], as is one whose environment is empty; any other
/// failure to read is [`Marker::Unreadable`] (see
/// [`marker::procargs_failure`]). `owned`: the kernel says it is ours.
#[cfg(target_os = "macos")]
fn read_marker(pid: u32, owned: bool) -> Option<Marker> {
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
    let sized_errno = std::io::Error::last_os_error().raw_os_error();
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
        let errno = if sized == 0 {
            std::io::Error::last_os_error().raw_os_error()
        } else {
            sized_errno
        };
        return if vanished(pid) {
            None
        } else {
            Some(marker::procargs_failure(errno, owned))
        };
    }
    buf.truncate(len);
    Some(marker::parse_procargs(&buf))
}

/// No reader for this OS: nothing can be ruled out.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_marker(pid: u32, _owned: bool) -> Option<Marker> {
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

    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_start_time_agrees_with_the_process_table_and_skips_a_zombie() {
        let me = std::process::id();
        assert_eq!(kernel_start_time(me), start_time(me));
        assert!(kernel_start_time(me).is_some());
        assert_eq!(kernel_start_time(0x3fff_fff0), None);
        let mut child = Command::new("true").spawn().unwrap();
        while !has_ended(child.id()) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(kernel_start_time(child.id()), None);
        child.wait().unwrap();
    }

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
            sid: None,
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

    /// Scan until `probe` is `Gone`, for a few seconds: a bystander that
    /// exits while being read makes one scan `Unknown`.
    fn gone_soon(id: &str, scope: &str) -> Probe {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let probe = scan_system(id, scope).probe;
            if probe == Probe::Gone || std::time::Instant::now() >= end {
                return probe;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn a_marked_process_is_found_until_it_dies() {
        let id = format!("scan-test-{}", std::process::id());
        let scope = "0123456789abcdef";
        let mut c = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::marker_sleeper_entry",
                "--nocapture",
            ])
            .env("CANIRUN_TEST_SLEEPER", "1")
            .env(marker::ENV_VAR, &id)
            .env(marker::SCOPE_VAR, scope)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let found = scan_system(&id, scope);
        assert_eq!(found.probe, Probe::Alive, "{found:?}");
        assert_eq!(found.alive.len(), 1);
        assert_eq!(found.alive[0].pid, c.id());
        assert!(found.alive[0].start_time.is_some());
        assert_eq!(gone_soon(&format!("{id}-other"), scope), Probe::Gone);
        // The same id under another scope is not this command.
        assert_eq!(gone_soon(&id, "fedcba9876543210"), Probe::Gone);
        // Only a pid that is still the same process is signalled.
        let mut stale = found.alive[0];
        stale.start_time = stale.start_time.map(|t| t + 1000);
        assert!(!signal_if_same(&stale, libc::SIGKILL));
        stale.start_time = None;
        assert!(!signal_if_same(&stale, libc::SIGKILL));
        assert!(c.try_wait().unwrap().is_none());
        assert!(signal_if_same(&found.alive[0], libc::SIGKILL));
        c.wait().unwrap();
        assert_eq!(gone_soon(&id, scope), Probe::Gone);
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
