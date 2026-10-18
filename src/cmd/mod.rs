pub mod config;
pub mod exec;
pub mod gc;
pub mod history;
pub mod job;
pub mod status;

use crate::error::Result;
use crate::gitctx;
use crate::key::{self, CommandKey};
use std::io::Read;

/// The history key for `argv` run from the current directory, as `exec`,
/// `submit` and `history` all derive it.
pub fn command_key(key_override: Option<&str>, argv: &[String]) -> Result<CommandKey> {
    let cwd = std::env::current_dir()?;
    let repo = gitctx::main_worktree(&cwd);
    let subdir = if repo.is_some() {
        gitctx::subdir(&cwd)
    } else {
        String::new()
    };
    if let Some(k) = key_override {
        return Ok(key::make_override_key(repo.as_deref(), &subdir, k, argv));
    }
    let roots: Vec<String> = std::env::var("TMPDIR").into_iter().collect();
    let head = key::temp_script(argv, &roots).and_then(read_head);
    let normalized = key::normalize(argv, &roots, head.as_deref());
    Ok(key::make_key(repo.as_deref(), &subdir, argv, normalized))
}

/// The first `SCRIPT_HEAD_BYTES` of a file, or `None` when unreadable.
fn read_head(path: &str) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(key::SCRIPT_HEAD_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}
