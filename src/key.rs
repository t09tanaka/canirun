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
    /// What the history is learned against: `argv`, or the heavy command
    /// found inside a shell string or a temporary script (see
    /// [`normalize`]).
    pub key_argv: String,
    /// sha256 hex of `repo + "\0" + key_argv` (plus the subdir).
    pub hash: String,
}

pub fn argv_string(argv: &[String]) -> String {
    argv.iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `normalized` is what [`normalize`] returned for `argv`; `None` keys the
/// command by its argv string, exactly as before normalization existed.
pub fn make_key(
    repo: Option<&str>,
    subdir: &str,
    argv: &[String],
    normalized: Option<String>,
) -> CommandKey {
    let repo = repo.unwrap_or("").trim().to_string();
    let subdir = subdir.trim().trim_matches('/').to_string();
    let argv = argv_string(argv);
    let key_argv = normalized.unwrap_or_else(|| argv.clone());
    CommandKey {
        hash: hash_of(&repo, &subdir, &key_argv),
        repo,
        subdir,
        argv,
        key_argv,
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
    let key_argv = format!("key:{}", key.trim());
    CommandKey {
        hash: hash_of(&repo, &subdir, &key_argv),
        repo,
        subdir,
        argv,
        key_argv,
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

/// Directories whose contents are throwaway: a script or argument under
/// one of these never names the same thing twice. The caller adds
/// `$TMPDIR`.
pub const TMP_ROOTS: &[&str] = &[
    "/tmp/",
    "/private/tmp/",
    "/var/folders/",
    "/private/var/folders/",
];

/// How many bytes of a temporary script are read to find its command.
pub const SCRIPT_HEAD_BYTES: usize = 8192;

/// Shells whose `-c` string (or script argument) is looked into.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// What makes a command "heavy" enough to be the key.
enum Heavy {
    /// The program alone (`npx`, `vitest`, ...).
    Any,
    /// The program with one of these subcommands among its first two
    /// non-flag arguments (`npm --prefix api run test`).
    Sub(&'static [&'static str]),
}

const HEAVY: &[(&str, Heavy)] = &[
    (
        "npm",
        Heavy::Sub(&["run", "run-script", "test", "t", "exec", "x"]),
    ),
    ("pnpm", Heavy::Sub(&["run", "test", "t", "exec", "dlx"])),
    ("yarn", Heavy::Sub(&["run", "test", "exec", "dlx"])),
    ("bun", Heavy::Sub(&["run", "test", "x"])),
    ("npx", Heavy::Any),
    ("pnpx", Heavy::Any),
    ("bunx", Heavy::Any),
    ("node", Heavy::Any),
    ("stryker", Heavy::Any),
    ("vitest", Heavy::Any),
    ("jest", Heavy::Any),
    ("mocha", Heavy::Any),
    ("playwright", Heavy::Any),
    ("pytest", Heavy::Any),
    (
        "cargo",
        Heavy::Sub(&[
            "test", "build", "check", "clippy", "bench", "nextest", "mutants", "llvm-cov",
        ]),
    ),
    ("go", Heavy::Sub(&["test", "build", "vet"])),
    ("dart", Heavy::Sub(&["test", "run", "analyze"])),
    ("flutter", Heavy::Sub(&["test", "build", "analyze"])),
];

/// Shell words that precede the real command of a segment.
const PREFIX_WORDS: &[&str] = &[
    "!", "{", "do", "then", "else", "elif", "if", "while", "until", "exec", "command", "builtin",
    "nohup",
];

/// How deep nested `bash -c '... sh -c "..."'` strings are followed.
const MAX_DEPTH: usize = 4;

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// True for a path under a temporary directory: [`TMP_ROOTS`], one of
/// `extra_roots` (normally `$TMPDIR`), or any `scratchpad` directory.
pub fn is_temp_path(word: &str, extra_roots: &[String]) -> bool {
    if !word.starts_with('/') {
        return false;
    }
    TMP_ROOTS.iter().any(|r| word.starts_with(r))
        || word.contains("/scratchpad/")
        || extra_roots.iter().any(|r| {
            let r = r.trim_end_matches('/');
            !r.is_empty()
                && word
                    .strip_prefix(r)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// `NAME=value` with a shell variable name.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            let mut chars = name.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Drop what runs *around* a command: assignments, `env`, `time`, `nice`,
/// `timeout` and shell keywords.
fn strip_prefix(words: &[String]) -> &[String] {
    let mut w = words;
    loop {
        let Some(first) = w.first() else {
            return w;
        };
        let prog = basename(first);
        if is_assignment(first) || PREFIX_WORDS.contains(&first.as_str()) {
            w = &w[1..];
        } else if prog == "env" {
            w = &w[1..];
            while let Some(a) = w.first() {
                if a == "-u" || a == "--unset" {
                    w = &w[w.len().min(2)..];
                } else if a.starts_with('-') || is_assignment(a) {
                    w = &w[1..];
                } else {
                    break;
                }
            }
        } else if prog == "time" || prog == "nice" || prog == "timeout" {
            w = &w[1..];
            while let Some(a) = w.first() {
                if a == "-n" || a == "-s" || a == "-k" {
                    w = &w[w.len().min(2)..];
                } else if a.starts_with('-') {
                    w = &w[1..];
                } else {
                    break;
                }
            }
            if prog == "timeout" && !w.is_empty() {
                // The duration.
                w = &w[1..];
            }
        } else {
            return w;
        }
    }
}

/// For a shell invocation, where its options end: `(saw -c, index of the
/// first operand)`.
fn shell_operand(words: &[String]) -> Option<(bool, usize)> {
    if !SHELLS.contains(&basename(words.first()?)) {
        return None;
    }
    let mut saw_c = false;
    let mut rest = &words[1..];
    while let Some((a, tail)) = rest.split_first() {
        if a == "--" {
            rest = tail;
            break;
        }
        if a == "-o" || a == "+o" {
            rest = tail.get(1..).unwrap_or_default();
            continue;
        }
        if !(a.len() > 1 && (a.starts_with('-') || a.starts_with('+'))) {
            break;
        }
        if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
            saw_c = true;
        }
        rest = tail;
    }
    Some((saw_c, words.len() - rest.len()))
}

fn is_heavy(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    let prog = basename(first);
    HEAVY.iter().any(|(name, kind)| {
        *name == prog
            && match kind {
                Heavy::Any => true,
                Heavy::Sub(subs) => words[1..]
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .take(2)
                    .any(|a| subs.contains(&a.as_str())),
            }
    })
}

/// Words joined by spaces, with temporary paths (also as `--flag=/tmp/x`)
/// replaced by `<tmp>`.
fn render(words: &[String], extra_roots: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if is_temp_path(w, extra_roots) {
                return "<tmp>".to_string();
            }
            match w.split_once('=') {
                Some((k, v)) if is_temp_path(v, extra_roots) => format!("{k}=<tmp>"),
                _ => w.clone(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A rough shell lexer: the words of each simple command, split at `;`,
/// `&&`, `||`, `|`, `&`, newlines and parentheses. Quotes, backslashes and
/// comments are honoured; redirection targets (`> f`, `2>&1`) are dropped
/// so they do not become part of a key. Expansions stay literal text.
fn lex(text: &str) -> Vec<Vec<String>> {
    struct Lexer {
        out: Vec<Vec<String>>,
        cur: String,
        in_word: bool,
        /// The next word is a redirection target.
        redirect: bool,
    }
    impl Lexer {
        fn flush(&mut self) {
            if self.in_word {
                let word = std::mem::take(&mut self.cur);
                if self.redirect {
                    self.redirect = false;
                } else if let Some(seg) = self.out.last_mut() {
                    seg.push(word);
                }
            }
            self.in_word = false;
        }
        fn sep(&mut self) {
            self.flush();
            self.redirect = false;
            self.out.push(Vec::new());
        }
    }
    let mut lx = Lexer {
        out: vec![Vec::new()],
        cur: String::new(),
        in_word: false,
        redirect: false,
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => lx.flush(),
            '\n' | ';' | '(' | ')' => lx.sep(),
            '#' if !lx.in_word => {
                while chars.peek().is_some_and(|&n| n != '\n') {
                    chars.next();
                }
            }
            '\'' => {
                lx.in_word = true;
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    lx.cur.push(n);
                }
            }
            '"' => {
                lx.in_word = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' => {
                            if let Some(e) = chars.next() {
                                if !matches!(e, '"' | '\\' | '$' | '`') {
                                    lx.cur.push('\\');
                                }
                                lx.cur.push(e);
                            }
                        }
                        _ => lx.cur.push(n),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(e) => {
                    lx.in_word = true;
                    lx.cur.push(e);
                }
            },
            '>' | '<' => {
                if lx.in_word && lx.cur.chars().all(|d| d.is_ascii_digit()) {
                    // `2>`: the fd number is part of the redirection.
                    lx.cur.clear();
                    lx.in_word = false;
                }
                lx.flush();
                while chars
                    .peek()
                    .is_some_and(|&n| matches!(n, '>' | '<' | '&' | '|'))
                {
                    chars.next();
                }
                lx.redirect = true;
            }
            // `&>file`: the `>` that follows is the redirection.
            '&' if chars.peek() == Some(&'>') => lx.flush(),
            // `&&` / `||` just make an empty segment in between.
            '&' | '|' => lx.sep(),
            _ => {
                lx.in_word = true;
                lx.cur.push(c);
            }
        }
    }
    lx.flush();
    lx.out
}

/// The first heavy command in a shell string, rendered as a key.
fn heavy_in_text(text: &str, extra_roots: &[String], depth: usize) -> Option<String> {
    lex(text)
        .iter()
        .find_map(|words| heavy_in_words(words, extra_roots, depth))
}

fn heavy_in_words(words: &[String], extra_roots: &[String], depth: usize) -> Option<String> {
    let words = strip_prefix(words);
    if let Some((true, i)) = shell_operand(words) {
        if depth >= MAX_DEPTH {
            return None;
        }
        return heavy_in_text(words.get(i)?, extra_roots, depth + 1);
    }
    is_heavy(words).then(|| render(words, extra_roots))
}

/// The temporary script `argv` runs, if any: `/tmp/x.sh ...` or
/// `bash /tmp/x.sh ...`. The caller reads its head for [`normalize`].
pub fn temp_script<'a>(argv: &'a [String], extra_roots: &[String]) -> Option<&'a str> {
    let words = strip_prefix(argv);
    let first = words.first()?;
    if is_temp_path(first, extra_roots) {
        return Some(first);
    }
    match shell_operand(words) {
        Some((false, i)) => words
            .get(i)
            .map(String::as_str)
            .filter(|w| is_temp_path(w, extra_roots)),
        _ => None,
    }
}

/// A stable key for commands whose argv changes every time although the
/// work does not:
///
/// - `bash -c '<long line>'` (also `sh`, `zsh`, `dash`, behind `env` /
///   `time` / assignments): the first heavy command in the string, e.g.
///   `npm run test` out of `cd x && npm run test 2>&1 | tail -5`.
/// - a script under a temporary directory: the first heavy command in
///   `script_head` (its first [`SCRIPT_HEAD_BYTES`]), else its file name
///   and arguments.
///
/// Temporary paths among the kept arguments become `<tmp>`. `None` when
/// neither applies, so the argv string stays the key.
pub fn normalize(
    argv: &[String],
    extra_roots: &[String],
    script_head: Option<&str>,
) -> Option<String> {
    let words = strip_prefix(argv);
    if let Some((true, _)) = shell_operand(words) {
        return heavy_in_words(words, extra_roots, 0);
    }
    let script = temp_script(argv, extra_roots)?;
    if let Some(found) = script_head.and_then(|t| heavy_in_text(t, extra_roots, 0)) {
        return Some(found);
    }
    let pos = words.iter().position(|w| w == script).unwrap_or(0);
    let mut shown: Vec<String> = words[..pos].to_vec();
    shown.push(basename(script).to_string());
    shown.extend_from_slice(&words[pos + 1..]);
    Some(render(&shown, extra_roots))
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
        let a = make_key(Some("/r"), "", &v(&["npm", "test"]), None);
        let b = make_key(Some(" /r "), "", &v(&["npm", " test"]), None);
        assert_eq!(a, b);
        assert_eq!(a.repo, "/r");
        assert_eq!(a.hash.len(), 64);
    }

    #[test]
    fn repo_scopes_the_key() {
        let a = make_key(Some("/r1"), "", &v(&["npm", "test"]), None);
        let b = make_key(Some("/r2"), "", &v(&["npm", "test"]), None);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn no_repo_is_empty_string() {
        let a = make_key(None, "", &v(&["x"]), None);
        assert_eq!(a.repo, "");
        assert_eq!(a, make_key(Some(""), "", &v(&["x"]), None));
    }

    #[test]
    fn subdir_scopes_the_key_but_root_hash_is_stable() {
        let root = make_key(Some("/r"), "", &v(&["npm", "test"]), None);
        let api = make_key(Some("/r"), "api/", &v(&["npm", "test"]), None);
        let client = make_key(Some("/r"), "client", &v(&["npm", "test"]), None);
        assert_ne!(root.hash, api.hash);
        assert_ne!(api.hash, client.hash);
        assert_eq!(api.subdir, "api");
        // The v0.1 layout: sha256(repo + "\0" + argv).
        let mut h = Sha256::new();
        h.update(b"/r\0npm test");
        assert_eq!(root.hash, format!("{:x}", h.finalize()));
        // A subdir that merely looks like part of argv does not collide.
        let a = make_key(Some("/r"), "x", &v(&["y"]), None);
        let b = make_key(Some("/r"), "", &v(&["x", "y"]), None);
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
        let a = make_key(Some("ab"), "", &v(&["c"]), None);
        let b = make_key(Some("a"), "", &v(&["bc"]), None);
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
        assert_ne!(a.hash, make_key(Some("/r"), "", &v(&["e2e"]), None).hash);
    }

    fn norm(argv: &[&str]) -> Option<String> {
        normalize(&v(argv), &[], None)
    }
    fn sh(text: &str) -> Option<String> {
        norm(&["sh", "-c", text])
    }

    #[test]
    fn plain_argv_is_not_normalized_and_keeps_its_hash() {
        assert_eq!(norm(&["npm", "run", "test"]), None);
        assert_eq!(norm(&["env", "-u", "X", "npm", "test"]), None);
        assert_eq!(norm(&[]), None);
        let a = make_key(Some("/r"), "", &v(&["npm", "run", "test"]), None);
        assert_eq!(a.key_argv, "npm run test");
        let mut h = Sha256::new();
        h.update(b"/r\0npm run test");
        assert_eq!(a.hash, format!("{:x}", h.finalize()));
    }

    #[test]
    fn shell_string_shares_the_key_of_its_heavy_command() {
        let argv = v(&["bash", "-c", "cd /x && npm run test 2>&1 | tail -5"]);
        let n = normalize(&argv, &[], None);
        assert_eq!(n.as_deref(), Some("npm run test"));
        let wrapped = make_key(Some("/r"), "", &argv, n);
        let direct = make_key(Some("/r"), "", &v(&["npm", "run", "test"]), None);
        assert_eq!(wrapped.hash, direct.hash);
        assert_eq!(wrapped.argv, argv_string(&argv));
        assert_eq!(wrapped.key_argv, "npm run test");
    }

    #[test]
    fn every_heavy_program_is_recognised() {
        for (prog, kind) in HEAVY {
            match kind {
                Heavy::Any => {
                    let line = format!("{prog} a");
                    assert_eq!(sh(&line), Some(line.clone()), "{prog}");
                }
                Heavy::Sub(subs) => {
                    for sub in *subs {
                        let line = format!("{prog} {sub}");
                        assert_eq!(sh(&format!("echo; {line}")), Some(line.clone()), "{line}");
                    }
                    assert_eq!(sh(&format!("{prog} install foo")), None, "{prog}");
                    assert_eq!(sh(&format!("{prog} --{} foo", subs[0])), None, "{prog}");
                }
            }
        }
        assert_eq!(sh("make test"), None);
        assert_eq!(sh(""), None);
    }

    #[test]
    fn subcommand_may_follow_flags_and_one_operand() {
        assert_eq!(
            sh("npm --prefix api run test").as_deref(),
            Some("npm --prefix api run test")
        );
        assert_eq!(sh("npm run -s lint").as_deref(), Some("npm run -s lint"));
        assert_eq!(sh("npm i y run"), None);
        assert_eq!(
            sh("/usr/local/bin/npx vitest").as_deref(),
            Some("/usr/local/bin/npx vitest")
        );
    }

    #[test]
    fn first_heavy_command_wins() {
        assert_eq!(
            sh("npm ci; npm run lint && npm run test").as_deref(),
            Some("npm run lint")
        );
        assert_eq!(sh("npm run a || npm run b").as_deref(), Some("npm run a"));
        assert_eq!(sh("(cd x; npm test)").as_deref(), Some("npm test"));
        assert_eq!(sh("set -e\nuptime\nnpm test").as_deref(), Some("npm test"));
    }

    #[test]
    fn wrappers_and_keywords_are_skipped() {
        assert_eq!(
            sh("FOO=1 env -u BAR -i X=2 /usr/bin/time -p nice -n 5 timeout -s KILL -k 3 60 npx vitest run")
                .as_deref(),
            Some("npx vitest run")
        );
        assert_eq!(sh("env --unset BAR npm test").as_deref(), Some("npm test"));
        assert_eq!(sh("time npm test").as_deref(), Some("npm test"));
        let got: Vec<(&str, Option<String>)> = PREFIX_WORDS
            .iter()
            .map(|w| (*w, sh(&format!("{w} npm test"))))
            .collect();
        let want: Vec<(&str, Option<String>)> = PREFIX_WORDS
            .iter()
            .map(|w| (*w, Some("npm test".to_string())))
            .collect();
        assert_eq!(got, want);
        assert_eq!(sh("env -u"), None);
        assert_eq!(sh("nice -n"), None);
        assert_eq!(sh("timeout"), None);
        assert_eq!(sh("timeout 10"), None);
        assert_eq!(sh("env"), None);
    }

    #[test]
    fn assignments_need_a_variable_name() {
        assert_eq!(sh("_a1=x npm test").as_deref(), Some("npm test"));
        assert_eq!(sh("1A=x npm test"), None);
        assert_eq!(sh("A-B=x npm test"), None);
        assert_eq!(sh("=x npm test"), None);
    }

    #[test]
    fn every_shell_and_option_form_is_followed() {
        let got: Vec<(&str, Option<String>)> = SHELLS
            .iter()
            .map(|s| (*s, norm(&[s, "-c", "npm test"])))
            .collect();
        let want: Vec<(&str, Option<String>)> = SHELLS
            .iter()
            .map(|s| (*s, Some("npm test".to_string())))
            .collect();
        assert_eq!(got, want);
        let forms: [&[&str]; 5] = [
            &["/bin/bash", "-lc", "npm test"],
            &["bash", "-e", "-o", "pipefail", "-c", "npm test"],
            &["bash", "+o", "pipefail", "-c", "npm test"],
            &["bash", "+x", "-c", "npm test"],
            &["env", "A=1", "bash", "-c", "npm test", "argv0"],
        ];
        for f in forms {
            assert_eq!(norm(f).as_deref(), Some("npm test"), "{f:?}");
        }
        // `--norc` is a long option, not `-c`; `-` alone is an operand.
        assert_eq!(norm(&["bash", "--norc", "npm test"]), None);
        assert_eq!(norm(&["bash", "-", "-c", "npm test"]), None);
        assert_eq!(norm(&["bash", "--", "-c"]), None);
        assert_eq!(norm(&["bash", "-o"]), None);
        assert_eq!(norm(&["bash", "-c"]), None);
        assert_eq!(norm(&["bash", "-c", "echo hi"]), None);
        assert_eq!(norm(&["python", "-c", "npm test"]), None);
    }

    fn nest(text: &str, levels: usize) -> String {
        let mut t = text.to_string();
        for _ in 0..levels {
            t = format!(
                "sh -c \"{}\" > /dev/null",
                t.replace('\\', "\\\\").replace('"', "\\\"")
            );
        }
        t
    }

    #[test]
    fn nested_shells_are_followed_to_a_limit() {
        assert_eq!(
            sh("uptime > /tmp/a; /usr/bin/time -p bash -c 'npm run x -- --affected' > /tmp/o 2> /tmp/e")
                .as_deref(),
            Some("npm run x -- --affected")
        );
        assert_eq!(
            sh(&nest("npm \"run\" t", MAX_DEPTH - 1)).as_deref(),
            Some("npm run t")
        );
        assert_eq!(sh(&nest("npm run t", MAX_DEPTH)), None);
    }

    #[test]
    fn lexer_handles_quotes_escapes_and_comments() {
        assert_eq!(
            sh("# npm run comment\nnpx vitest -t \"cannot follow\" 'a b' a#b").as_deref(),
            Some("npx vitest -t cannot follow a b a#b")
        );
        assert_eq!(sh("npx vitest a\\ b").as_deref(), Some("npx vitest a b"));
        assert_eq!(sh("npm run \\\n test").as_deref(), Some("npm run test"));
        assert_eq!(
            sh(r#"npx vitest "a\"b" "c\\d" "e\$f" "g\`h" "i\nj""#).as_deref(),
            Some(r#"npx vitest a"b c\d e$f g`h i\nj"#)
        );
        assert_eq!(sh("npx vitest ''").as_deref(), Some("npx vitest "));
        assert_eq!(
            sh("echo hi # ; npm run x\nnpm test").as_deref(),
            Some("npm test")
        );
        assert_eq!(sh("npx\tvitest\r").as_deref(), Some("npx vitest"));
        assert_eq!(sh("npx vitest \\").as_deref(), Some("npx vitest"));
        assert_eq!(sh("npx vitest \"open").as_deref(), Some("npx vitest open"));
        assert_eq!(sh("npx vitest \"x\\").as_deref(), Some("npx vitest x"));
    }

    #[test]
    fn redirections_are_dropped() {
        for line in [
            "npm test > /tmp/x 2>&1",
            "npm test &> log",
            "npm test >>log",
            "npm test 2>/dev/null",
            "npm test >| f",
            "npm test < in",
            "npm test 2> e",
        ] {
            assert_eq!(sh(line).as_deref(), Some("npm test"), "{line}");
        }
        assert_eq!(sh("npm test|tail").as_deref(), Some("npm test"));
        assert_eq!(sh("npx vitest &> log a").as_deref(), Some("npx vitest a"));
        assert_eq!(sh("npx vitest |& tail").as_deref(), Some("npx vitest"));
        assert_eq!(sh("cat < f | npm test & wait").as_deref(), Some("npm test"));
        // A redirection without a target does not eat the next command.
        assert_eq!(sh("echo hi >; npm test").as_deref(), Some("npm test"));
        // A digit word is only an fd when it touches the `>`.
        assert_eq!(sh("npx vitest 2 > out").as_deref(), Some("npx vitest 2"));
        assert_eq!(sh("npx vitest a2>out").as_deref(), Some("npx vitest a2"));
    }

    #[test]
    fn temporary_paths_are_detected() {
        let roots = v(&["/custom/t/", ""]);
        for p in [
            "/tmp/x",
            "/private/tmp/x",
            "/var/folders/ab/x",
            "/private/var/folders/x",
            "/home/u/scratchpad/x.sh",
            "/custom/t/x",
        ] {
            assert!(is_temp_path(p, &roots), "{p}");
        }
        for p in ["tmp/x", "/custom/tx", "/custom/t", "/home/u/x", "/tmpx"] {
            assert!(!is_temp_path(p, &roots), "{p}");
        }
    }

    #[test]
    fn temporary_paths_in_kept_arguments_are_masked() {
        assert_eq!(
            normalize(
                &v(&[
                    "sh",
                    "-c",
                    "npx vitest /tmp/a --out=/tmp/b --x=rel /custom/t/c"
                ]),
                &v(&["/custom/t"]),
                None
            )
            .as_deref(),
            Some("npx vitest <tmp> --out=<tmp> --x=rel <tmp>")
        );
    }

    #[test]
    fn temporary_script_is_found() {
        let r: [String; 0] = [];
        let a = v(&["/tmp/s/x.sh", "1"]);
        assert_eq!(temp_script(&a, &r), Some("/tmp/s/x.sh"));
        let a = v(&["env", "A=1", "bash", "-x", "/tmp/s/x.sh"]);
        assert_eq!(temp_script(&a, &r), Some("/tmp/s/x.sh"));
        assert_eq!(temp_script(&v(&["bash", "./x.sh"]), &r), None);
        assert_eq!(
            temp_script(&v(&["bash", "--", "/tmp/x.sh"]), &r),
            Some("/tmp/x.sh")
        );
        assert_eq!(temp_script(&v(&["bash", "-c", "/tmp/x.sh"]), &r), None);
        assert_eq!(temp_script(&v(&["bash"]), &r), None);
        assert_eq!(temp_script(&v(&["npm", "/tmp/x"]), &r), None);
        assert_eq!(temp_script(&v(&[]), &r), None);
    }

    #[test]
    fn temporary_script_is_keyed_by_its_command_or_its_name() {
        let head = "#!/bin/sh\nset -e\ncd /tmp/w\nnpm run build >/tmp/log\n";
        assert_eq!(
            normalize(&v(&["/tmp/s/x.sh", "1"]), &[], Some(head)).as_deref(),
            Some("npm run build")
        );
        assert_eq!(
            normalize(&v(&["/tmp/s/x.sh", "1"]), &[], None).as_deref(),
            Some("x.sh 1")
        );
        assert_eq!(
            normalize(&v(&["/tmp/s/x.sh"]), &[], Some("echo hi\n")).as_deref(),
            Some("x.sh")
        );
        assert_eq!(
            normalize(
                &v(&[
                    "A=1",
                    "bash",
                    "-x",
                    "/private/tmp/s/run.sh",
                    "/tmp/out",
                    "4"
                ]),
                &[],
                None
            )
            .as_deref(),
            Some("bash -x run.sh <tmp> 4")
        );
        assert_eq!(normalize(&v(&["bash", "./x.sh"]), &[], Some(head)), None);
    }
}
