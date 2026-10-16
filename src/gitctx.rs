//! Repository detection for the command key: the main worktree path, so
//! every linked worktree collapses to one key.

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn main_worktree(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8(out.stdout).ok()?;
    let common = PathBuf::from(common.trim());
    // `<main>/.git` for a normal repo; a bare repo's common dir *is* the
    // repo, in which case we keep it as-is.
    let main = if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        common.parent().map(Path::to_path_buf)?
    } else {
        common
    };
    Some(main.to_string_lossy().into_owned())
}
