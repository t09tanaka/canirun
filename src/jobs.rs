//! Asynchronous jobs (spec "v0.2 addendum"): a detached runner per job,
//! a JSON descriptor, a merged log, and an exit file.

use crate::error::{Error, Result};
use crate::paths;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub weight: Option<u32>,
    pub front: bool,
    pub key: Option<String>,
    pub submitted_at: DateTime<Utc>,
    pub submitter_pid: u32,
    /// Keep running even when nobody is waiting.
    #[serde(default)]
    pub detach: bool,
    /// Cancelled by the runner because its waiter went away.
    #[serde(default)]
    pub orphaned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done(i32),
    Lost,
}

impl JobState {
    pub fn label(self) -> String {
        match self {
            JobState::Queued => "queued".into(),
            JobState::Running => "running".into(),
            JobState::Done(c) => format!("done {c}"),
            JobState::Lost => "lost".into(),
        }
    }
}

pub fn save(job: &Job) -> Result<()> {
    let path = paths::job_json(&job.id);
    fs::create_dir_all(paths::jobs_dir())?;
    let mut tmp = tempfile::NamedTempFile::new_in(paths::jobs_dir())?;
    serde_json::to_writer_pretty(&mut tmp, job)?;
    tmp.write_all(b"\n")?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn load(id: &str) -> Result<Job> {
    let bytes =
        fs::read(paths::job_json(id)).map_err(|e| Error::Ledger(format!("job {id}: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// All jobs, newest first. Unreadable descriptors are skipped.
pub fn list() -> Result<Vec<Job>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(paths::jobs_dir()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Ok(job) = load(id) {
            out.push(job);
        }
    }
    out.sort_by_key(|j| std::cmp::Reverse(j.submitted_at));
    Ok(out)
}

pub fn read_exit(id: &str) -> Option<i32> {
    fs::read_to_string(paths::job_exit(id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Persist the outcome. Written once; later writers lose (the first
/// outcome is the real one, e.g. the runner's over a late `cancel`).
pub fn write_exit(id: &str, code: i32) {
    let path = paths::job_exit(id);
    if path.exists() {
        return;
    }
    let _ = fs::create_dir_all(paths::jobs_dir());
    if let Ok(mut tmp) = tempfile::NamedTempFile::new_in(paths::jobs_dir()) {
        let _ = writeln!(tmp, "{code}");
        let _ = tmp.persist(&path);
    }
    if let Ok(mut job) = load(id) {
        job.finished_at = Some(Utc::now());
        let _ = save(&job);
    }
}

/// Spawn the detached runner for `job`: own session, stdin from
/// /dev/null, stdout+stderr appended to the job log, env inherited.
pub fn spawn_runner(job: &Job) -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::job_log(&job.id))?;
    let log_err = log.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__run-job")
        .arg(&job.id)
        .current_dir(&job.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // SAFETY: setsid is async-signal-safe and only detaches the child from
    // our session so a terminal hangup or our exit never reaches it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| Error::Spawn(format!("cannot start job runner: {e}")))
}

/// Hold this while waiting on a job; the runner treats "nobody holds it"
/// as "nobody cares" once the grace period is over.
pub fn take_waiter(id: &str) -> Result<crate::lock::Lock> {
    crate::lock::take_shared(&paths::job_waiter(id))
}

pub fn has_waiter(id: &str) -> bool {
    crate::lock::is_held(&paths::job_waiter(id))
}

pub fn mark_orphaned(id: &str) {
    if let Ok(mut job) = load(id) {
        job.orphaned = true;
        let _ = save(&job);
    }
}

pub fn remove_files(id: &str) {
    for p in [
        paths::job_json(id),
        paths::job_log(id),
        paths::job_exit(id),
        paths::job_waiter(id),
    ] {
        let _ = fs::remove_file(p);
    }
}
