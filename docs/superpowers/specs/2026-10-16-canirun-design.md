# canirun — Design Spec

- **Date:** 2026-10-16
- **Status:** Approved for implementation (2026-10-16)
- **Scope:** v0.1. A local admission controller for heavy commands:
  "Can I run?" — a command asks before it starts, waits for budget, runs,
  and reports what it cost so the next run knows its weight.

## Context & goal

A developer runs many agent sessions on one Mac (about ten at a time).
Each session reproduces the project's CI locally — lint, unit, build,
integration/e2e, mutation tests — and dispatches several units in
parallel. With 30–50 heavy processes on 12 cores, every run slows down
and the total throughput drops below running them one after another.
The same happens when a human `git push` triggers a pre-push hook that
runs mutation tests while agents are already saturating the machine.

`canirun` is a traffic controller for that situation. It is deliberately
shaped like `portool` (a passive ledger plus a thin execution boundary,
no daemon), but it differs in one way: it **waits** and it **measures**.

The two outcomes that define success:

1. Ten sessions dispatching local CI at once finish sooner in total than
   they do today, because no more work is admitted than the machine can
   actually run.
2. Nobody has to hand-tune weights. A command's weight is learned from
   its own past runs.

## Goals

- One global budget (default: number of logical CPUs) shared by every
  caller of the same OS user.
- Every admitted command has a weight; the sum of running weights never
  exceeds the budget.
- Weights are learned automatically from measured CPU usage of previous
  runs of the same command in the same repository.
- Waiting callers see progress (what is running, how long they have
  waited). A killed or interrupted caller — running *or* waiting —
  disappears from the ledger without any cleanup step.
- Interactive callers (a human at the keyboard, e.g. a pre-push hook) can
  jump the queue with `--front`.
- Nested calls (a gated command that itself calls `canirun exec`) do not
  deadlock: the inner call inherits the outer lease.
- Repositories that adopt `canirun` in git hooks keep working on machines
  that do not have it installed.

## Non-goals (v0.1)

- No daemon, no background process, no proxying.
- No killing, pausing, renicing, or reprioritising running processes.
- No memory-based admission. Memory is recorded, not enforced.
- No dynamic adjustment from live CPU load. Admission uses declared or
  learned weights only.
- No per-repository config file. One user-level config.
- No cross-user or cross-machine coordination (same scope rule as
  portool: one OS user + `XDG_STATE_HOME`).
- No wait timeout. A caller waits until admitted or interrupted; the
  human decides by looking at `canirun status`.

## Anti-goals

- It must never block a command forever because of a stale ledger entry
  left by a crashed process. If this happens the tool has failed.
- It must never let a command through *un-gated* because of a stale
  environment variable in a long-lived shell.
- It must never change the exit code, stdin/stdout/stderr, or signal
  behaviour of the wrapped command in a way the caller can notice
  (except the wait before it starts).

## Prior art

- `portool` (same author): the ledger-in-XDG-state, flock, temp+rename,
  `exec` subcommand, and public-repo conventions are copied from there.
- `make -j` jobserver, `nice`, `sem` from GNU parallel: the closest
  existing tools. None learn weights, none show a queue, and `sem`
  requires every caller to agree on a count up front.

## Constraints

- Rust, single binary, `cargo install canirun`. MSRV, CI matrix
  (ubuntu + macos), release tarballs, Keep a Changelog, and SemVer rules
  mirror `portool`.
- macOS and Linux only.
- Public repository under `t09tanaka/canirun`. The README, specs, tests,
  commit messages, and examples must not name the author's employer,
  clients, or their repositories. Examples use a bundled `examples/`
  project only.
- The admission decision and the weight derivation must be **pure
  functions in their own modules** so that coverage and mutation-test
  gates apply to them (the author's standard test policy).

## CLI surface

```
canirun exec [--weight N] [--front] [--key K] -- <command> [args...]
canirun status [--json]
canirun history [--key K] [--json]
canirun config [--path]
canirun gc
```

### `canirun exec`

1. Resolve the **command key** (see below) and the **weight** (see
   below).
2. If `CANIRUN_LEASE` is set *and* names a lease that is currently
   `running` in the ledger, this is a nested call: run the command
   immediately with no reservation and no measurement. Otherwise
   continue.
3. Register a `waiting` entry in the ledger, holding a per-entry lock
   file (see Ledger).
4. Loop: take the ledger lock, reap dead entries, evaluate admission; if
   admitted, flip the entry to `running` and break; else release the
   lock, print a progress line to stderr (rate-limited to once every 5
   seconds), sleep 500 ms, repeat. SIGINT/SIGTERM while waiting removes
   the entry and exits 130/143.
5. Spawn the command with inherited stdio and `CANIRUN_LEASE=<id>` in
   its environment, **in canirun's own process group** so that a
   terminal Ctrl-C reaches the child directly and the child stays the
   tty foreground job. canirun itself ignores SIGINT and SIGQUIT while
   the child runs (the terminal already delivered them to the child; a
   second copy would make test runners force-quit) and forwards only
   SIGTERM and SIGHUP, which arrive via `kill(2)` addressed to canirun's
   pid. Wait with `wait4` to collect rusage.
6. Remove the entry, append a history record, and exit with the child's
   exit code. If the child died from a signal, re-raise the same signal
   on ourselves so shells see the same termination.

The command is executed directly (`execvp` semantics via
`std::process::Command`), **not** through a shell. Callers that need a
shell pass `sh -c '...'` explicitly, the same as `portool exec`.

Flags:

- `--weight N` overrides the derived weight for this run (still recorded
  to history, so learning continues).
- `--front` places the entry at the front of the queue. Several `--front`
  entries are FIFO among themselves.
- `--key K` overrides the command key (for callers whose argv carries
  volatile parts like temp paths).

### `canirun status`

Lists running entries (pid, key, weight, elapsed) and waiting entries
(pid, key, weight, waited, front flag) plus `used/budget`. `--json` emits
the ledger as-is for scripts. Dead entries are reaped first, so the view
is accurate without a daemon.

### `canirun history`

Shows recent runs for a key (or all keys): wall time, CPU time, derived
parallelism, max RSS, exit code, and the weight that will be used next.

### `canirun config`

Prints the effective configuration (defaults merged with the file).
`--path` prints the config file location.

### `canirun gc`

Reaps dead ledger entries and trims history to the configured retention.
Reaping also happens on every `exec` and `status`, so `gc` is only for
tidiness.

## Admission

Pure function: `admit(ledger_state, candidate) -> Admitted | Wait`.

- `used = Σ weight of running entries`.
- The queue is ordered: all `--front` entries in enqueue order, then all
  normal entries in enqueue order.
- The candidate is admitted iff it is the **head** of the queue and
  `used + weight ≤ budget`.
- Consequence: a heavy head *reserves*. Nothing behind it is admitted
  until it fits, so heavy units cannot be starved by a stream of light
  ones. Throughput loss from this is accepted in v0.1.
- Exception: a candidate whose weight exceeds the budget is clamped to
  the budget, so it runs alone eventually rather than never.

Weight floor is 1. Weight ceiling is the budget.

## Weight derivation

Pure function: `derive_weight(config_entry, history_records) -> u32`.

Priority order:

1. `--weight N` on the command line.
2. A config pattern with a fixed `weight`.
3. Learned: `ceil(max over the last 5 *usable* records of cpu_seconds /
   wall_seconds)`, where `cpu_seconds = ru_utime + ru_stime` of the child
   tree. A record is usable when `wall_seconds ≥ 1`; unusable records
   are filtered **before** taking the last 5, so a burst of quick
   failures (a config typo, a crashing lint) cannot erase what a long
   suite has already learned. Then `max(learned, min_weight)` if the
   matching config pattern has `min_weight`.
4. A config pattern with `initial_weight` (used only when there is no
   usable history).
5. `default_weight` from config (default `2`).

Why max of the last 5, not the mean: an under-estimate causes exactly the
contention this tool exists to prevent; an over-estimate only costs a bit
of idle CPU. Why 5: enough to absorb one anomalous run, few enough that a
test suite that grew heavier is reflected within a day.

**Known blind spot (author's call):** rusage covers the child process
tree only. Work pushed into the Docker daemon / VM by integration and
e2e suites is invisible, so their learned parallelism will sit near 1 —
the heaviest units measured as the lightest. v0.1 mitigates this with
`min_weight` on the config pattern for such commands. A better signal
(system-wide CPU delta during the run) is a candidate for v0.2 and is
not in scope now.

## Command key

The key identifies "the same command in the same project" across runs
and across worktrees.

- `repo`: the main worktree path, from
  `git rev-parse --path-format=absolute --git-common-dir` resolved to its
  parent, so every linked worktree of a repository collapses to one key.
  Outside a git repository, `repo` is empty.
- `argv`: the command and its arguments joined by single spaces, after
  trimming each element. No normalisation beyond that.
- `key = sha256(repo + "\0" + argv)` stored alongside the readable
  `repo` and `argv` for `status`/`history` display.

Accepted limitation: arguments that embed paths or timestamps fragment
the history. `--key` is the escape hatch.

## Configuration

`$XDG_CONFIG_HOME/canirun/config.toml` (default
`~/.config/canirun/config.toml`). Absent file means all defaults.

```toml
budget = 12               # default: logical CPU count
default_weight = 2
history_keep = 200        # records per key
progress_interval_secs = 5

[[pattern]]
match = "*stryker*"       # glob over the argv string
min_weight = 6

[[pattern]]
match = "*test:e2e*"
min_weight = 4

[[pattern]]
match = "*eslint*"
weight = 1                # fixed; history is recorded but ignored
```

Patterns are evaluated top to bottom; the first match wins. `weight`
and `min_weight`/`initial_weight` are mutually exclusive in one entry.

## Ledger and locking

`$XDG_STATE_HOME/canirun/` (default `~/.local/state/canirun/`):

- `ledger.json` — the single source of truth: entries with `id`, `pid`,
  `state` (`waiting`/`running`), `front`, `weight`, `repo`, `argv`,
  `enqueued_at`, `started_at`. Schema carries a `version` field.
- `ledger.lock` — exclusive flock guarding reads and writes of
  `ledger.json`. Writes are temp-file + rename, as in portool.
- `leases/<id>.lock` — one file per entry. The owning `canirun` process
  holds an exclusive flock on it from registration until exit. The lock
  fd is **not** inherited by the child (`O_CLOEXEC`): if the wrapper is
  SIGKILLed while its child tree survives, that tree runs off-ledger
  and the budget under-counts until it ends. The alternative (child
  inherits the fd) would let any leaked long-lived process pin a
  `running` entry forever, which violates the first anti-goal; the
  under-count is the lesser failure. **Liveness is
  defined as "this flock cannot be taken."** Any process that holds the
  ledger lock may try each lease lock non-blockingly; a lock it obtains
  belongs to a dead entry, which is removed on the spot. This is what
  makes a Ctrl-C'd *waiter* at the head of the queue vanish instead of
  blocking everyone. pid is recorded for display only, never for
  liveness.
- `history.jsonl` — append-only measurement records, trimmed by `gc` to
  `history_keep` per key.

Nested-call validation uses the same rule: an inner call honours
`CANIRUN_LEASE` only if the ledger has a `running` entry with that id
whose lease lock is held by someone. A stale variable in a long-lived
shell therefore gates normally instead of bypassing.

## Progress output

While waiting, once every `progress_interval_secs`, to stderr:

```
canirun: waiting 0:42 — running 3 (10/12 used), ahead of you 2
```

Nothing is printed when admitted immediately. Nothing is ever printed to
stdout by `exec`.

## Integration (outside this repository, documented here only)

- **Agent local-CI skill.** The parallel-dispatch step should run each
  unit as `canirun exec -- <unit command>`. Because a unit may now wait
  for minutes before it even starts, the skill must run units in the
  background (or under a monitor) rather than inside a foreground tool
  call with a hard timeout. That change is a one-line edit in a local,
  untracked skill file and is not part of this repository.
- **Git hooks (e.g. husky pre-push).** One line that prefers `canirun`
  when present and falls back to the bare command otherwise, e.g.
  `if command -v canirun >/dev/null; then canirun exec --front -- npm
  test; else npm test; fi`. Those edits land in each adopting
  repository separately and are out of scope here.

## Module layout (suggested, builder may adjust)

```
src/
  main.rs          clap entry
  cmd/{exec,status,history,config,gc}.rs
  admit.rs         pure: queue order + admission decision
  weight.rs        pure: derive_weight
  key.rs           pure: command key
  config.rs        toml loading + defaults
  ledger.rs        schema, load/save (temp+rename)
  lock.rs          flock helpers (ledger lock, lease locks, liveness)
  runner.rs        spawn, signal forwarding, wait4 + rusage
  history.rs       jsonl append/read/trim
  paths.rs         XDG resolution
tests/             CLI + concurrency tests with a bundled sleeper binary
examples/          a tiny project with fake "lint"/"test" scripts
```

## Testing expectations

- `admit`, `weight`, `key`: unit tests with 100% line coverage
  (`cargo llvm-cov`, per-file) and a mutation gate
  (`cargo mutants --file src/admit.rs --file src/weight.rs --file
  src/key.rs`, zero survivors). Both run as **blocking CI jobs** in
  addition to the fmt/clippy/test/MSRV jobs mirrored from portool
  (portool runs mutants only locally; this repository promotes it to
  CI because the pure modules are small).
- `tests/concurrency.rs`: N processes contend for a budget of B; assert
  the running sum never exceeds B (each child writes a timestamped
  marker; the test checks overlap).
- `tests/liveness.rs`: kill a waiting head with SIGKILL; assert the next
  waiter is admitted within one poll interval.
- `tests/nested.rs`: an outer `exec` spawning an inner `exec` with a full
  budget completes; an inner call with a bogus `CANIRUN_LEASE` waits.
- `tests/signals.rs`: SIGINT to `canirun exec` reaches the child; exit
  status mirrors the child's.

## Left to the builder

- Exact JSON field names and the `status` table layout.
- Whether `wait4` is called via `libc` directly or a thin wrapper crate.
- Glob crate choice.
- How the bundled example scripts simulate CPU load portably.
- Release workflow details beyond "mirror portool".

## Decisions made by the author of this spec (for the user to check)

- Max of last 5 measurements, floor 1, ceiling = budget.
- Default weight 2 when nothing is known.
- Memory is recorded but never used for admission.
- Head-of-queue reservation (strict FIFO), accepting some idle CPU.
- `min_weight` as the v0.1 answer to Docker-hosted load being invisible
  to rusage.
- Lease liveness by flock, never by pid.
- `exec` runs the command without a shell.
- No wait timeout flag in v0.1.
- Child shares canirun's process group; canirun ignores SIGINT/SIGQUIT
  and forwards only SIGTERM/SIGHUP (builder-check question 1).
- Lease fd is close-on-exec; a SIGKILLed wrapper under-counts rather
  than a leaked child pinning the ledger (builder-check question 2).
- Coverage and mutation gates run as blocking CI jobs (question 3).
- "Last 5" means last 5 usable records, filtered first (question 4).
