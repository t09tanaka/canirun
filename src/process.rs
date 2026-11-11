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

/// When `pid` started, on a clock that setting the wall clock does not
/// move, with the boot the count belongs to (see [`lineage::StartMark`]).
/// `None` when the process or the boot id cannot be read, when the count is
/// zero, and where the OS has no such clock. The caller makes sure `pid` is
/// the process it means (its own unreaped child, or a pid checked before
/// and after). Only reads.
pub fn start_mark(pid: u32) -> Option<lineage::StartMark> {
    // Pid 0 is not a process, and a pid above `i32::MAX` is not a pid.
    if pid == 0 || i32::try_from(pid).is_err() {
        return None;
    }
    // The boot id is the machine's own, never an injected one: the count
    // means something only together with the boot it was read in.
    let boot = system_boot_id()?;
    let ticks = kernel_start_ticks(pid).filter(|t| *t != 0)?;
    Some(lineage::StartMark::new(boot, ticks))
}

/// macOS: the Mach absolute time at which `pid` was forked
/// (`ri_proc_start_abstime`). It is set once, at the fork, is kept across
/// `exec`, stands still while the machine sleeps and starts again at boot.
#[cfg(target_os = "macos")]
fn kernel_start_ticks(pid: u32) -> Option<u64> {
    let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V0,
            (&mut info as *mut libc::rusage_info_v0).cast(),
        )
    };
    (rc == 0).then_some(info.ri_proc_start_abstime)
}

/// Linux: the start of `pid` in clock ticks since boot, from
/// `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn kernel_start_ticks(pid: u32) -> Option<u64> {
    stat_start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel_start_ticks(_pid: u32) -> Option<u64> {
    None
}

/// Field 22 (`starttime`) of a `/proc/<pid>/stat` line. The second field,
/// the command name in parentheses, may itself contain spaces and
/// parentheses, so the fields are counted from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn stat_start_ticks(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` begins at field 3 (the state).
    rest.split_ascii_whitespace().nth(19)?.parse().ok()
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
    /// When the child started, on a clock the wall clock does not move
    /// ([`start_mark`]). Absent in older sidecars, where the OS has no such
    /// clock, and when it could not be read; a hidden orphan is then never
    /// cleared by the age of its session's leader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_mark: Option<lineage::StartMark>,
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
            start_mark: start_mark(pid),
        }
    }
}

impl ChildIdentity {
    /// The identity of a just-spawned session leader: the child was started
    /// with a successful `setsid` in `pre_exec` (the spawn succeeding proves
    /// it), so its session and process group ids are its pid. Nothing is read
    /// from the kernel for them, so a child that already exited (a zombie
    /// cannot be asked) is still recorded with them.
    pub fn capture_leader(pid: u32) -> Self {
        let id = pid as i32;
        ChildIdentity {
            pid,
            start_time: start_time(pid),
            pgid: Some(id),
            sid: Some(id),
            start_mark: start_mark(pid),
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
        // `getsid(0)` is the caller's own session, never pid 0's.
        if row.proc.pid == me || row.proc.pid == 0 {
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

/// What the kernel says about the session of `p`, for
/// [`lineage::standing`]. Nothing is reported unless `p` is the same process
/// (pid and start time) both before and after its session is read, so the
/// session id is one `p` was a member of throughout. The leader is the live
/// process whose pid is that id and whose own session is that id; while `p`
/// is a member the id cannot have been handed to a new process. Its
/// [`start_mark`] is read as well. Only reads: no process is signalled.
pub fn session_evidence(p: &ProcRef) -> lineage::SessionEvidence {
    let unknown = lineage::SessionEvidence::default();
    let Some(start) = p.start_time else {
        return unknown;
    };
    // `getsid(0)` is the caller's own session, and a pid above `i32::MAX`
    // is not a pid.
    if p.pid == 0 || i32::try_from(p.pid).is_err() || start_time(p.pid) != Some(start) {
        return unknown;
    }
    let Some(sid) = session_of(p.pid).and_then(|s| u32::try_from(s).ok()) else {
        return unknown;
    };
    if sid == 0 {
        return unknown;
    }
    let leader_start = if sid == p.pid {
        Some(start)
    } else {
        leader_start_time(sid)
    };
    // Read between the two checks of `p`, like the leader's start time: it
    // is the mark of the process that leads `p`'s session.
    let leader_mark = leader_start.and_then(|_| start_mark(sid));
    let same = start_time(p.pid) == Some(start)
        && session_of(p.pid).and_then(|s| u32::try_from(s).ok()) == Some(sid);
    if !same {
        return unknown;
    }
    lineage::SessionEvidence::new(Some(sid), leader_start).with_leader_mark(leader_mark)
}

/// The start time of the live process at pid `sid`, if it leads the session
/// `sid` (its own session id is its pid). A process that merely has that
/// pid is not the leader.
fn leader_start_time(sid: u32) -> Option<u64> {
    start_time(sid).filter(|_| session_of(sid).and_then(|s| u32::try_from(s).ok()) == Some(sid))
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
/// start time was never learned is not signalled, nor is pid 0 or 1, nor a
/// number above `i32::MAX` (as a `pid_t` it would be negative, which names
/// a process group or every process).
pub fn signal_if_same(p: &ProcRef, sig: libc::c_int) -> bool {
    let Some(then) = p.start_time else {
        return false;
    };
    if p.pid <= 1 || i32::try_from(p.pid).is_err() || start_time(p.pid) != Some(then) {
        return false;
    }
    unsafe { libc::kill(p.pid as libc::pid_t, sig) == 0 }
}

/// Look for processes of this user carrying `TASK_TRAFFIC_CONTROLLER_MANAGED_ID=<id>` and
/// `TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE=<scope>`.
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
            start_mark: None,
        };
        assert_eq!(probe_child(&gone), Probe::Gone);
    }

    #[test]
    fn a_session_leader_is_recorded_without_asking_the_kernel() {
        use std::os::unix::process::CommandExt;
        let mut c = unsafe {
            Command::new("true")
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        // Not waited for: it is a zombie as soon as it has exited, when
        // getsid can no longer be asked.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let id = ChildIdentity::capture_leader(c.id());
        assert_eq!(id.sid, Some(c.id() as i32));
        assert_eq!(id.pgid, Some(c.id() as i32));
        c.wait().unwrap();
    }

    #[test]
    fn session_evidence_is_given_only_for_a_process_that_is_still_the_same() {
        let me = std::process::id();
        let mine = ProcRef {
            pid: me,
            start_time: start_time(me),
        };
        let ev = session_evidence(&mine);
        let sid = u32::try_from(unsafe { libc::getsid(0) }).unwrap();
        assert_eq!(ev.sid, Some(sid));
        // The leader, when it still runs, did not start after us.
        if sid == me {
            assert_eq!(ev.leader_start, mine.start_time);
        } else if let Some(l) = ev.leader_start {
            assert!(l <= mine.start_time.unwrap(), "{ev:?}");
        }
        let unknown = lineage::SessionEvidence::default();
        // Another start time (the pid was reused), no start time, pid 0
        // (which `getsid` reads as the caller) and a pid that is not one.
        for p in [
            ProcRef {
                pid: me,
                start_time: mine.start_time.map(|t| t + 1),
            },
            ProcRef {
                pid: me,
                start_time: None,
            },
            ProcRef {
                pid: 0,
                start_time: mine.start_time,
            },
            ProcRef {
                pid: u32::MAX,
                start_time: mine.start_time,
            },
        ] {
            assert_eq!(session_evidence(&p), unknown, "{p:?}");
        }
    }

    /// A shell that leads a session of its own and has started one
    /// `/bin/sleep` in it; with `leave`, the shell then exits. The sleep's
    /// pid comes from the shell, so it is trusted only once the kernel says
    /// its session is the shell's, asked while the shell (running, or an
    /// unreaped zombie) still pins that id. It ends by itself after 20 s.
    fn session_with_member(leave: bool) -> (std::process::Child, ProcRef) {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let script = if leave {
            "/bin/sleep 20 </dev/null >/dev/null 2>&1 & echo $!"
        } else {
            "/bin/sleep 20 </dev/null >/dev/null 2>&1 & echo $!; wait"
        };
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        unsafe {
            c.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = c.spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid: u32 = line.trim().parse().expect("the member's pid");
        assert!(pid > 1 && i32::try_from(pid).is_ok(), "{pid}");
        let member = ProcRef {
            pid,
            start_time: start_time(pid),
        };
        assert!(member.start_time.is_some(), "the member is not running");
        // Ours: in the session only our own, unreaped child can lead.
        assert_eq!(session_of(pid), Some(leader.id() as i32));
        assert_eq!(start_time(pid), member.start_time);
        (leader, member)
    }

    /// Stop a member that [`session_with_member`] proved to be ours: by pid
    /// and start time, never pid 0 or 1.
    fn stop_member(member: &ProcRef) {
        assert!(member.pid > 1 && member.start_time.is_some());
        signal_if_same(member, libc::SIGKILL);
        for _ in 0..400 {
            if start_time(member.pid) != member.start_time {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the member did not end: {member:?}");
    }

    #[test]
    fn session_evidence_names_a_leader_that_is_another_process() {
        let (mut leader, member) = session_with_member(false);
        let ev = session_evidence(&member);
        let leader_pid = leader.id();
        let (leader_start, leader_mark) = (start_time(leader_pid), start_mark(leader_pid));
        stop_member(&member);
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(leader_start.is_some());
        assert_eq!(ev.sid, Some(leader_pid));
        assert_eq!(ev.leader_start, leader_start);
        assert_eq!(ev.leader_mark, leader_mark);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(ev.leader_mark.is_some(), "{ev:?}");
        }
    }

    #[test]
    fn session_evidence_has_no_leader_once_the_leader_is_dead() {
        let (mut leader, member) = session_with_member(true);
        let leader_pid = leader.id();
        // Reaped: nothing has that pid as a session leader any more. (The
        // number is not handed out again while the member is in it.)
        leader.wait().unwrap();
        let ev = session_evidence(&member);
        stop_member(&member);
        assert_eq!(ev.sid, Some(leader_pid));
        assert_eq!(ev.leader_start, None, "{ev:?}");
        assert_eq!(ev.leader_mark, None, "{ev:?}");
    }

    #[test]
    fn a_process_that_does_not_lead_the_session_of_its_pid_is_not_a_leader() {
        // The kernel never hands a live session's id to a new process, so
        // "the process at pid == sid is not that session's leader" cannot be
        // staged from end to end. What can be checked is the lookup itself:
        // asked about the pid of a live process that leads no session, it
        // names no leader; asked about one that does, it names it.
        let mut plain = Command::new("/bin/sleep").arg("20").spawn().unwrap();
        let pid = plain.id();
        let (alive, session) = (start_time(pid), session_of(pid));
        let found = leader_start_time(pid);
        plain.kill().unwrap();
        plain.wait().unwrap();
        assert!(alive.is_some());
        assert!(session.is_some_and(|s| s > 0 && s as u32 != pid));
        assert_eq!(found, None);

        let (mut leader, member) = session_with_member(false);
        let found = leader_start_time(leader.id());
        let want = start_time(leader.id());
        stop_member(&member);
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(want.is_some());
        assert_eq!(found, want);
        // And a pid nobody has.
        assert_eq!(leader_start_time(0x3fff_fff0), None);
    }

    #[test]
    fn a_start_mark_orders_processes_without_the_wall_clock() {
        let me = std::process::id();
        let mine = start_mark(me);
        // Not a process at all, and one that does not exist.
        assert_eq!(start_mark(0), None);
        assert_eq!(start_mark(u32::MAX), None);
        assert_eq!(start_mark(0x3fff_fff0), None);
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(mine, None);
            return;
        }
        let mine = mine.expect("our own start mark");
        assert_eq!(Some(&mine.boot), system_boot_id().as_ref());
        assert!(mine.ticks > 0);
        // The same answer every time, and `exec` does not change it: the
        // child below was forked from us and then became `/bin/sleep`.
        assert_eq!(start_mark(me), Some(mine.clone()));
        let mut child = Command::new("/bin/sleep").arg("20").spawn().unwrap();
        let first = start_mark(child.id());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let again = start_mark(child.id());
        let recorded = ChildIdentity::capture_leader(child.id()).start_mark;
        child.kill().unwrap();
        child.wait().unwrap();
        let first = first.expect("the child's start mark");
        assert_eq!(again, Some(first.clone()));
        assert_eq!(recorded, Some(first.clone()));
        assert_eq!(first.boot, mine.boot);
        // Later than us (the test has run for a while before it forks; on
        // Linux a tick is 10 ms, so equal is allowed there).
        assert!(first.ticks >= mine.ticks, "{first:?} {mine:?}");
        if cfg!(target_os = "macos") {
            assert!(first.ticks > mine.ticks, "{first:?} {mine:?}");
        }
        // Reaped: gone.
        assert_eq!(start_mark(child.id()), None);
    }

    #[test]
    fn the_start_ticks_are_read_after_the_command_name() {
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20 21";
        for comm in ["sleep", "a b", "a) (b c", ")", "x) R 1 2 3 4 5 6 7 8 9"] {
            let line = format!("77 ({comm}) {tail}");
            assert_eq!(stat_start_ticks(&line), Some(4242), "{comm}");
        }
        // Too short, not a number, no command name.
        assert_eq!(stat_start_ticks("77 (sleep) S 1 2 3"), None);
        assert_eq!(
            stat_start_ticks("77 (sleep) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 x 20"),
            None
        );
        assert_eq!(
            stat_start_ticks("77 (sleep) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 -5 20"),
            None
        );
        assert_eq!(stat_start_ticks(tail), None);
        assert_eq!(stat_start_ticks(""), None);
    }

    #[test]
    fn pid_zero_is_never_a_session_member() {
        // getsid(0) is the caller's own session; it must not be counted.
        let mine = unsafe { libc::getsid(0) };
        assert!(session_members(mine).0.iter().all(|p| p.pid != 0));
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
        if std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_SLEEPER").is_some() {
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
            .env("TASK_TRAFFIC_CONTROLLER_TEST_SLEEPER", "1")
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
