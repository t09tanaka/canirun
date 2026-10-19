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
  runs of the same command in the same repository; peak memory is learned
  the same way.
- Admission also looks at the machine as it is right now (available
  memory, load average) so the ledger's budget is a plan, not the only
  brake.
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
- No reaction to live conditions *after* admission: once a command is
  running it is never paused or stopped, however the machine looks.
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
parallelism, max RSS, exit code, and the weight and predicted memory
that will be used next.

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

### Live guards

The head is admitted only if, in addition to the budget check, the
machine looks able to take it *now*:

- `available_memory ≥ predicted_rss + memory_reserve`, where
  `predicted_rss` is the max `max_rss` over the last 5 usable history
  records for the key (0 when unknown) and `memory_reserve` defaults to
  2 GiB.
- `cpu_busy ≤ cpu_threshold_percent` (default 85%), where `cpu_busy` is
  the global CPU usage between two samples 300 ms apart, taken on this
  poll. The 1-minute load average was tried first and rejected: on macOS
  it runs far above the number of cores even with a third of the CPU
  idle, so a per-core threshold would hold the queue almost always.

Both are read from the OS on every poll (the `sysinfo` crate; on macOS
"available" = free + inactive pages). They are brakes, not the budget:
the budget decides how much canirun *plans* to run, the guards delay the
next start when something outside canirun (an IDE, a browser, a Docker
VM) is already using the machine.

**Starvation rule:** if no canirun-managed entry is currently `running`,
the live guards are skipped and the head is admitted on the budget check
alone. Otherwise a machine kept busy by processes canirun does not
manage would hold the queue forever, violating the first anti-goal.

The reason the head is waiting (`budget`, `memory`, `cpu`) is part of
the admission result so `status` and the progress line can show it.

Pure function signature:
`admit(ledger_state, candidate, live: Option<LiveSnapshot>) -> Admitted | Wait(reason)`.
The starvation rule is applied inside `admit` (so it is unit-tested); `live`
is `None` only when the OS could not be read, and the guards are then
skipped.

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
memory_reserve_mib = 2048 # keep this much available before admitting
cpu_threshold_percent = 85 # global CPU busy above this: hold

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
canirun: waiting 0:42 (budget) — running 3 (10/12 used), ahead of you 2
canirun: waiting 1:07 (memory 1.1 GiB available, need 3.4 GiB) — running 2 (6/12 used)
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
- `admit` unit tests cover each live-guard reason and the starvation
  rule with synthetic `LiveSnapshot`s (the OS is never read in tests).

## Left to the builder

- Exact JSON field names and the `status` table layout.
- Whether `wait4` is called via `libc` directly or a thin wrapper crate.
- Glob crate choice.
- How the bundled example scripts simulate CPU load portably.
- Release workflow details beyond "mirror portool".

## Decisions made by the author of this spec (for the user to check)

- Max of last 5 measurements, floor 1, ceiling = budget.
- Default weight 2 when nothing is known.
- Memory and CPU usage are live brakes on the next start, never a reason
  to touch a running process. Defaults: 2 GiB reserve, CPU ≤ 85% busy.
- Live guards are skipped when canirun has nothing running (starvation
  rule).
- Head-of-queue reservation (strict FIFO), accepting some idle CPU.
  (Relaxed by backfill of short commands, see the 2026-10-19 addendum.)
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

---

# v0.2 addendum — asynchronous jobs (2026-10-16)

- **Status:** Approved in chat, implementing.
- **Scope:** `submit` / `wait` / `jobs` / `cancel`. A thin layer over the
  v0.1 admission path; the ledger, weights, guards and liveness are
  unchanged. `exec` stays the right tool for interactive callers.

## Why

An agent dispatching several CI units wants to throw them at the machine
and collect results later, without one worker (or one foreground tool
call with a hard timeout) pinned to each unit while it waits in the
queue. `exec` already queues and runs; what it cannot do is outlive the
caller or be collected in a batch.

## Commands

```
canirun submit [--weight N] [--front] [--key K] -- <command> [args...]
canirun wait [--no-output] <job-id>...
canirun jobs [--json]
canirun cancel <job-id>...
```

### `submit`

Creates a job and returns immediately, printing the job id on stdout.

1. `id` = a fresh lease id (the job id **is** the ledger entry id).
2. Writes `jobs/<id>.json`: id, argv, cwd (the submitter's), weight
   flags, `submitted_at`, `submitter_pid`.
3. Spawns a detached **runner**: `canirun __run-job <id>` in its own
   session (`setsid`), stdin from `/dev/null`, stdout and stderr both
   appended to `jobs/<id>.log`, environment inherited from the
   submitter, cwd = the job's cwd.
4. Exits 0 without waiting.

The runner performs exactly the v0.1 `exec` path (register, wait, run,
measure, record history), then writes `jobs/<id>.exit` containing the
shell-style exit code (128+N for a signal death) and `finished_at` into
the job file. A runner that dies by signal while waiting or running
writes the exit file before re-raising, so `wait` never hangs on a
cancelled job.

### `wait`

Polls (500 ms) until every named job has an exit file. For each job, in
the order given, prints a header line and the log (unless
`--no-output`), then exits with the first non-zero exit code, or 0. A job
whose runner has died without writing an exit file (no ledger entry, no
lease held, no exit file for 2 polls) is reported as `lost` with exit
code 2.

### `jobs`

Lists jobs from `jobs/*.json` newest first: id, state
(`queued` / `running` / `done <code>` / `lost`), submitted and finished
times, repo:argv. State is derived from the exit file and the ledger.

### `cancel`

For each id: if an exit file exists, nothing to do; otherwise send
SIGTERM to the runner pid from the ledger entry (the runner forwards it
to its child or, if still waiting, removes itself). If no ledger entry
exists and no exit file, mark the job `lost`.

### `gc`

Also removes job files (`.json`, `.log`, `.exit`) finished more than
`jobs_keep_days` (default 7) ago.

## Jobs die with their waiter (user decision, 2026-10-16)

A load-control tool must not leave work running after the party that
wanted it is gone. `wait` therefore holds a shared `flock` on
`jobs/<id>.waiter` for every job it waits on. The runner checks once a
second: when `jobs_orphan_grace_secs` (default 30) have passed since
submit and nobody holds the waiter lock, it marks the job `orphaned` and
sends itself SIGTERM, which the wait-phase handler or the run-phase
forwarding turns into exit 143 with the exit file written. The grace
covers the gap between `submit` and the first `wait`. `submit --detach`
opts out for genuinely fire-and-forget work.

`cancel` on a job whose runner has not registered yet writes the exit
file (143); the runner treats an existing exit file as a cancellation
request on start and on every poll. `jobs` reports "queued" rather than
"lost" for the first 5 s after submit, when the runner may still be
starting.

## Estimates (user request, 2026-10-16)

Each ledger entry carries `expected_secs`, the max wall time over the
command's last five usable runs (None when unknown). `eta::estimate` is
a pure event simulation: running entries finish at
`max(0, expected − elapsed)`, waiting entries are taken in admission
order, the head reserves until it fits, and each waiting entry starts
when the budget allows and finishes `expected` later. Unknown entries
count as 60 s and mark the result `rough`, shown as a trailing `?`. Live
guards are not modelled, so the estimate is a lower bound. It is printed
by `submit` (stderr), in every `exec` progress line, and per entry in
`status` and `jobs`.

## Not in v0.2

- No dependency between jobs, no priorities beyond `--front`.
- No streaming of a running job's log by `wait`; use `tail -f jobs/<id>.log`.

## Integration

The local-CI skill's Phase 2 worker may `submit` the independent steps
of its unit and `wait` for them in one call, instead of one background
`exec` per step. Collapsing the N workers into a parent that submits
everything is a larger skill redesign (the workers also interpret the
workflow YAML and services) and is left for later.


---

# v0.2.1 addendum — first-day feedback (2026-10-16)

Another session reported, after a day of mixed use (most load still not
going through canirun):

1. **Unmanaged load starved managed `--front` entries.** The budget was
   2/12 used, CPU 100% from processes outside canirun, and a human's
   pre-push waited over a minute on the CPU guard. Decision: `--front`
   skips the CPU guard (memory guard stays), and every head gives up on
   the CPU guard after `cpu_guard_max_wait_secs` (default 120). Both are
   pure-function inputs (`waited_secs`, `front`) covered by unit tests.
2. **Weights learned under contention read low** (a 6-worker mutation run
   measured x1.8). Decision: one CPU sample right before the start marks
   the record `contended` when busy > threshold; `usable_window` drops
   contended records whenever a calm one exists, before taking the last
   five. Config `min_weight` remains the floor for Docker-hosted load.
3. Hooks outside canirun: addressed by the six pre-push PRs.
4. Light commands wrapped (`sleep`, `npm ci`, `git commit`): README and
   the user's global instructions now list what not to wrap; no tool
   change.
5. Records with 0 CPU over 2–4 s were the rollout agents' sleeping `npm`
   shims, not a measurement gap.

A further finding from the rollout: `api/`, `client/` and `admin/` of one
monorepo shared a key because the key ignored cwd. Keys now include the
subdirectory relative to the worktree root; root-level keys hash as
before so existing history carries over.


---

# Addendum — head-of-line blocking (2026-10-19)

## What happened

`status` showed budget 4/12 used. Running: two weight-2 development
servers (a Node.js server and a Flutter app in `run` mode), both resident,
both measured at 0.0 cores. Waiting: a `--front` pre-push check of weight
9 at the head, and three commands of weight 2–4 behind it. Each of the
three fit the 8 free slots alone, but strict head reservation held all of
them for over 50 minutes, until the two servers were stopped. Finding
which processes held the budget took effort.

## Decisions

1. **Backfill of short commands.** A waiting entry behind the head may
   start first when it fits, its learned wall time (`expected_secs`, the
   max of its last five usable runs) is known and at most
   `backfill_max_job_secs` (default 300, 0 disables), and every entry
   ahead of it either does not fit the budget or cannot start before it
   anyway (a non-head that is not short). Entries ahead that will start
   are never overtaken, so short commands keep their order among
   themselves. The budget and the live guards apply to a backfilled
   entry as to a head. Entries started this way carry `backfilled` in the
   ledger.
2. **Starvation bound.** The cap is tied to whether backfill is what
   delays the entry ahead, not to how long it has waited alone: an entry
   ahead that would fit without the backfilled entries is not passed again
   once it is `--front` (at once: a human is waiting) or has waited
   `backfill_max_wait_secs` (default 300). It then starts as soon as the
   short commands already running end, so backfill delays a normal head by
   at most about `backfill_max_wait_secs + backfill_max_job_secs`, and a
   `--front` head by at most one short command. A head held up by
   something else (a resident server, a long build) is passed for as long
   as that lasts, because backfill does not delay it. A cap on the head's
   own waiting time alone was rejected: it would have closed backfill
   five minutes into the incident and reproduced it.
3. **Not EASY backfill.** Passing only commands that end before the
   head's projected start was rejected: a resident entry is long past its
   expected wall time, so the projection is always "now" and nothing
   would ever pass.
4. **Idle entries keep their weight.** Charging a running entry only what
   it measures (as low as 0.0 for a server) was considered and rejected.
   Integration suites whose work runs in a Docker daemon or VM measure
   near zero in their own tree (the known blind spot) while the machine
   is busy, and an idle server can start a full rebuild at any time;
   either would let the budget admit more than the machine can take.
   Measurement keeps only raising the charge (`max(weight, round(using))`).
   Visibility handles the resident case instead (decision 5), together
   with the existing advice not to wrap commands that do not exit.
5. **Who holds the budget.** A waiting line whose reason is `budget` (or
   the backfill cap) ends with `; held by <pid> w<weight> idle|using N
   <running time> <label>, ...` for the biggest three running entries.
   `status` marks running entries `idle` (running at least 5 minutes,
   measured under 0.2 cores) and `[backfill]`, and, when the head waits on
   the budget, prints how much it needs and how much is free.
6. **Estimates follow admission.** `eta::estimate` now asks `admit` (no
   live snapshot) who starts at each simulated step, so backfill and its
   cap are modelled exactly as admission applies them.

