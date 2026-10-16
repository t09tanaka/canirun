//! Repository detection for the command key: the main worktree path, so
//! every linked worktree collapses to one key.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `cwd` relative to the worktree root ("" at the root, "api/" below it),
/// or "" outside git. Lets subprojects of one repository learn separately.
pub fn subdir(cwd: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--show-prefix"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_default()
}

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
