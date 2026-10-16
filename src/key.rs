//! Pure command key (spec "Command key").

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandKey {
    /// Main worktree path, or empty outside a git repository.
    pub repo: String,
    /// cwd relative to the worktree root ("" at the root).
    pub subdir: String,
    /// argv joined by single spaces after trimming each element.
    pub argv: String,
    /// sha256 hex of `repo + "\0" + argv`.
    pub hash: String,
}

pub fn argv_string(argv: &[String]) -> String {
    argv.iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn make_key(repo: Option<&str>, subdir: &str, argv: &[String]) -> CommandKey {
    let repo = repo.unwrap_or("").trim().to_string();
    let subdir = subdir.trim().trim_matches('/').to_string();
    let argv = argv_string(argv);
    CommandKey {
        hash: hash_of(&repo, &subdir, &argv),
        repo,
        subdir,
        argv,
    }
}

/// Key for an explicit `--key K` override: the repo and subdir still scope it.
pub fn make_override_key(
    repo: Option<&str>,
    subdir: &str,
    key: &str,
    argv: &[String],
) -> CommandKey {
    let repo = repo.unwrap_or("").trim().to_string();
    let subdir = subdir.trim().trim_matches('/').to_string();
    let argv = argv_string(argv);
    CommandKey {
        hash: hash_of(&repo, &subdir, &format!("key:{}", key.trim())),
        repo,
        subdir,
        argv,
    }
}

/// Root-level commands hash exactly as in v0.1/v0.2 so their history
/// carries over; a non-empty subdir adds one more segment.
fn hash_of(repo: &str, subdir: &str, argv: &str) -> String {
    let mut h = Sha256::new();
    h.update(repo.as_bytes());
    h.update(b"\0");
    if !subdir.is_empty() {
        h.update(subdir.as_bytes());
        h.update(b"\0");
    }
    h.update(argv.as_bytes());
    format!("{:x}", h.finalize())
}

/// "repo/subdir" for display, or just the repo name at the root.
pub fn project_label(repo: &str, subdir: &str) -> String {
    let name = std::path::Path::new(repo)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.to_string());
    if subdir.is_empty() {
        name
    } else {
        format!("{name}/{subdir}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn argv_is_trimmed_and_joined() {
        assert_eq!(
            argv_string(&v(&[" npm", "run ", "", "test"])),
            "npm run test"
        );
    }

    #[test]
    fn same_repo_same_argv_same_hash() {
        let a = make_key(Some("/r"), "", &v(&["npm", "test"]));
        let b = make_key(Some(" /r "), "", &v(&["npm", " test"]));
        assert_eq!(a, b);
        assert_eq!(a.repo, "/r");
        assert_eq!(a.hash.len(), 64);
    }

    #[test]
    fn repo_scopes_the_key() {
        let a = make_key(Some("/r1"), "", &v(&["npm", "test"]));
        let b = make_key(Some("/r2"), "", &v(&["npm", "test"]));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn no_repo_is_empty_string() {
        let a = make_key(None, "", &v(&["x"]));
        assert_eq!(a.repo, "");
        assert_eq!(a, make_key(Some(""), "", &v(&["x"])));
    }

    #[test]
    fn subdir_scopes_the_key_but_root_hash_is_stable() {
        let root = make_key(Some("/r"), "", &v(&["npm", "test"]));
        let api = make_key(Some("/r"), "api/", &v(&["npm", "test"]));
        let client = make_key(Some("/r"), "client", &v(&["npm", "test"]));
        assert_ne!(root.hash, api.hash);
        assert_ne!(api.hash, client.hash);
        assert_eq!(api.subdir, "api");
        // The v0.1 layout: sha256(repo + "\0" + argv).
        let mut h = Sha256::new();
        h.update(b"/r\0npm test");
        assert_eq!(root.hash, format!("{:x}", h.finalize()));
        // A subdir that merely looks like part of argv does not collide.
        let a = make_key(Some("/r"), "x", &v(&["y"]));
        let b = make_key(Some("/r"), "", &v(&["x", "y"]));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn project_labels() {
        assert_eq!(project_label("/p/mono", ""), "mono");
        assert_eq!(project_label("/p/mono", "api"), "mono/api");
        assert_eq!(project_label("", ""), "");
    }

    #[test]
    fn separator_prevents_boundary_collisions() {
        let a = make_key(Some("ab"), "", &v(&["c"]));
        let b = make_key(Some("a"), "", &v(&["bc"]));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn override_key_ignores_argv_for_hash_but_keeps_it_for_display() {
        let a = make_override_key(Some("/r"), "", "e2e", &v(&["npm", "test", "--", "/tmp/1"]));
        let b = make_override_key(
            Some("/r"),
            "",
            " e2e ",
            &v(&["npm", "test", "--", "/tmp/2"]),
        );
        assert_eq!(a.hash, b.hash);
        assert_ne!(a.argv, b.argv);
        assert_ne!(a.hash, make_key(Some("/r"), "", &v(&["e2e"])).hash);
    }
}
