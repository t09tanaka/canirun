# canirun

**Can I run?** A local admission controller for heavy commands.

A command asks before it starts, waits until the machine has room, runs,
and reports what it cost so the next run of the same command knows its
weight. No daemon, no process supervision, one JSON ledger per user.

```sh
canirun exec -- npm test                 # waits for budget, then runs
canirun exec --front -- npm run lint     # a human is waiting: jump the queue
canirun status                           # who is running, who is waiting, why, and when
canirun history                          # what each command measured, next weight

id=$(canirun submit -- npm run test:e2e) # async: returns a job id and a rough ETA
canirun wait "$id"                       # keeps the job alive, prints its log, mirrors its exit
```

## What & why

When several agent sessions each reproduce CI locally at the same time —
lint, unit, build, e2e, mutation tests, times ten — a 12-core laptop ends
up running 30–50 heavy processes. Everything slows down and the total
takes longer than running them one after another. The same happens when a
`pre-push` hook starts mutation tests while the machine is already
saturated.

`canirun` is the traffic controller:

- **One budget.** A number of "slots" (default: logical CPU count) shared
  by every `canirun exec` of the same OS user. The sum of running weights
  never exceeds it.
- **Learned weights.** Every run is measured (`wait4` rusage: CPU time,
  wall time, peak RSS). A command's weight is the ceiling of the highest
  CPU-parallelism it showed in its last five usable runs, scoped to the
  repository and subdirectory it ran in. Runs that started on a saturated
  machine are marked `contended` and ignored while calmer runs exist,
  because contention makes parallelism read low. Nothing to hand-tune;
  `config.toml` holds only hints and exceptions.
- **Live brakes.** Before the head of the queue starts, canirun also
  checks the machine as it is right now: available memory must cover the
  command's learned peak RSS plus a reserve, and global CPU usage must be
  under a threshold. These delay the *next* start; a running command is
  never paused or killed. `--front` entries skip the CPU brake, and any
  head gives up on it after `cpu_guard_max_wait_secs`, so load that does
  not go through canirun cannot hold the queue indefinitely. On a
  machine that is badly overloaded (load average at least
  `front_overload_factor` x budget) a `--front` entry still waits, for at
  most `front_overload_max_wait_secs` (30 s). Likewise, measured use
  beyond the running entries' weights holds a `--front` entry for at most
  that long; after that only declared weights count for it. (The load
  average is the trigger because measured cores cannot exceed the core
  count, so "twice the budget" is only visible as runnable processes.)
- **Measured, not just declared.** While a command runs, canirun samples
  the CPU its whole process tree uses. A command declared (or learned) at
  weight 2 that is using 5 cores takes 5 from the budget until it slows
  down, so the next start waits instead of piling on. The running
  command itself is never slowed or stopped.
- **Crash-safe queue.** Each waiting or running entry holds an `flock` on
  its own lease file. A Ctrl-C'd, SIGKILLed, or crashed `canirun` — waiting
  or running — vanishes from the ledger on the next poll. No cleanup step,
  no daemon, no pid heuristics.
- **FIFO with reservation, plus backfill.** The head of the queue
  reserves until it fits. While it does not fit, a command behind it that
  is known to be short (learned wall time at most
  `backfill_max_job_secs`, default 5 min) may start first if it fits, so
  one heavy head no longer stops everything behind it. A heavy unit is
  still not starved: once a head would fit but for backfilled commands,
  nothing new passes it after it has waited `backfill_max_wait_secs`
  (default 5 min), or at once when it is `--front`. `--front` lets a human
  at the keyboard jump ahead.
- **Nesting-safe.** A gated command that itself calls `canirun exec`
  inherits the outer lease instead of deadlocking.
- **Async jobs that die with their waiter.** `submit` returns a job id as
  soon as the job is queued; `wait` collects the log and exit code later. A job nobody is
  waiting on is cancelled after a grace period, so a crashed caller never
  leaves a test suite burning CPU. `--detach` opts out.
- **Rough ETAs.** From learned wall times and the current queue, every
  waiting entry gets a "start in / done in" estimate on `submit`, in the
  progress line, and in `status` / `jobs`.

## Install

```sh
cargo install canirun
```

Requires macOS or Linux. Prebuilt binaries for macOS (Apple Silicon /
Intel) and Linux (x86_64 / arm64) are attached to each
[release](https://github.com/t09tanaka/canirun/releases/latest).

## Usage

### `canirun exec`

```
canirun exec [--weight N] [--front] [--key K] -- <command> [args...]
```

Waits until admitted, then runs `<command>` with inherited stdin/stdout/
stderr in canirun's own process group, with `CANIRUN_LEASE=<id>` in its
environment. The exit code is the child's; a child killed by a signal
makes canirun die from the same signal. The command runs directly
(`execvp` semantics), not through a shell — pass `sh -c '...'` yourself
if you need one.

- `--weight N` overrides the derived weight for this run (still measured).
- `--front` puts the entry ahead of all normal entries. Several `--front`
  entries are FIFO among themselves.
- `--key K` replaces the argv string as the history key, for commands
  whose arguments carry temp paths or timestamps. It wins over the
  automatic keys below.

Wrapped commands are keyed by the heavy command inside them, so a
one-off shell line still learns from earlier runs:

- `sh -c` / `bash -c` / `zsh -c` / `dash -c '<line>'` (also behind `env`,
  `time`, `nice`, `timeout` and `VAR=value`, and through nested `-c`
  strings): the first command in the line that runs `npm run` / `npm test`
  / `npm exec` (and the pnpm / yarn / bun equivalents), `npx`, `node`,
  `stryker`, `vitest`, `jest`, `mocha`, `playwright`, `pytest`,
  `cargo test|build|check|clippy|bench|nextest|mutants|llvm-cov`,
  `go test|build|vet`, `dart test|run|analyze` or
  `flutter test|build|analyze`. Redirections are dropped, so
  `cd api && npm run test 2>&1 | tail -5` is keyed as `npm run test` and
  shares its history with `canirun exec -- npm run test`.
- A script under a temporary directory (`/tmp`, `/private/tmp`,
  `/var/folders`, `$TMPDIR`, or any `scratchpad` directory), run directly
  or as `bash /tmp/x.sh`: its first 8 KiB are read and the same rule
  applies; if nothing is found (or the file cannot be read), it is keyed
  by its file name and arguments.

Temporary paths among the kept arguments become `<tmp>`. A line with no
heavy command keeps the full argv string as its key. `history` shows the
derived key as `key: ...` under the command.

While waiting, a progress line goes to stderr every few seconds:

```
canirun: waiting 0:42 (queue) — running 3 (10/12 used), ahead of you 2
canirun: waiting 52:10 (budget) — running 2 (4/12 used); held by 48211 w2 idle 52:30 api: make api, 48190 w2 idle 51:02 app: flutter run
canirun: waiting 1:07 (memory 1.1 GiB available, need 3.4 GiB) — running 2 (6/12 used)
canirun: waiting 0:12 (cpu 93% busy, threshold 85%) — running 2 (6/12 used)
```

A line waiting on the budget names the running entries holding it
(biggest first, at most three): pid, weight, `idle` or `using N`, and how
long they have run, so a forgotten server is easy to find and stop.

When stderr is not a terminal (a log file, an agent's tool output), a line
is printed only when the reason or the queue position changes, plus one
every `progress_interval_secs_non_tty` (default 60) as a heartbeat, so a
long wait does not fill the output with identical lines.

If the program is not found and the command after `--` is a single
argument containing spaces (`-- "npm run x"`), canirun adds a hint to pass
the words separately (`-- npm run x`) or use `sh -c '...'`.

Nothing is written to stdout by canirun itself.

### `canirun status`

```
budget 10/12 used
machine 31.2 GiB available, cpu 71% busy

running (3)
    48211  w2  (using 0.0) idle    52:30  api: make api
    48190  w4  (using 4.1)          1:03  webapp: npm run test:e2e
    48260  w2  (using 0.9)          0:20  [backfill] api: npm run test:unit
  (idle: running 5:00+ and using under 0.2 cores; a server or watch mode holds its weight until it exits)

waiting (2)
    48302  w9      0:41  [front] api: npm run test:gates
    48310  w1      0:40  webapp: npm run lint

the head needs w9, 4 of 12 free; held by the running entries above
```

`(using N)` is the cores the running command's process tree used
recently (see "How it works"); `budget X/Y used` counts each running
entry at its weight or at that measurement, whichever is more. `idle`
marks an entry that has run for 5 minutes and is using almost nothing:
usually a development server or a watch mode, which holds its weight
until it exits (see "What not to wrap"). `[backfill]` marks an entry
that started ahead of a head that did not fit. `--json`
dumps the ledger. Dead entries are reaped first, so the view is accurate
without a daemon.

### `canirun history [--key K] [--json] [--limit N] [-- <command>]`

Measured runs per command key, and the weight and memory that will be
used next:

```
webapp: npm run test:e2e
  next: weight 4 (Learned), memory 3.4 GiB
  10-16 21:04  wall   184.2s  cpu   611.0s  x3.3  rss   3.4 GiB  exit 0   w4
  10-16 20:12  wall   190.7s  cpu   640.1s  x3.4  rss   3.2 GiB  exit 0   w2
```

### `canirun submit` / `wait` / `jobs` / `cancel`

```
canirun submit [--detach] [--weight N] [--front] [--key K] -- <command> [args...]
canirun wait [--no-output] <job-id>...
canirun jobs [--json]
canirun cancel <job-id>...
```

`submit` writes a job descriptor, starts a detached runner (own session,
stdin from `/dev/null`, stdout and stderr merged into the job log,
environment and cwd inherited), waits until the runner has joined the
queue (so back-to-back submits keep their order; at most 5 s, after which
it warns and returns anyway) and prints the job id on stdout, with a
rough estimate on stderr:

```
$ id=$(canirun submit -- npm run test:e2e)
canirun: job 3f9c… start in ~1:10, done in ~4:20
```

`wait` blocks until every named job has finished, prints a header and the
log for each in the order given, and exits with the first non-zero code
(`143` for a cancelled job). While `wait` runs it holds a shared lock on
the job; **a job with no holder is cancelled once `jobs_orphan_grace_secs`
(default 30) have passed since submit**, so the usual `submit` → `wait`
pair keeps it alive and a caller that dies takes its jobs down with it.
`--detach` keeps a job running regardless. `jobs` lists every job with
its state (`queued` / `running` / `done N` / `orphaned N` / `lost`) and
ETA; `cancel` sends SIGTERM to the runner, or marks a not-yet-started job
cancelled.

The runner is the same admission path as `exec`: weights, guards, history
and liveness all apply. Job files live in `jobs/` under the state
directory; `gc` removes those finished more than `jobs_keep_days` ago.

### `canirun inspect --json` and `canirun estimate --json -- <command>`

Both only read; they never register an entry, take a lease, reap a dead
entry or write history (`status` reaps, `inspect` does not).

`inspect` prints the ledger (a `suspect_dead` flag on entries whose lease is
not held, which are kept and counted as busy), the limits, the live
machine metrics and the history the ledger entries refer to, with a
`snapshot_digest` and a `policy_digest`.

`estimate` predicts when the command would start and finish if it were
queued now, by replaying the ledger through the same admission function
`exec` uses. Times are milliseconds from now; a value that cannot be
computed is `null` with the reason in `null_reasons`. With no usable history
`runtime_source` is `fallback`: the finish time is `null` and only
`ranking` carries a number (a 60 s default, good for ordering candidate
machines, not an ETA). A running entry past its recorded runtime has an
unknown remaining time, not zero. Live memory and CPU guards are read once;
what depends on them is marked `conditional`.

The same machinery is available to Rust callers as `canirun::inspect` and
`canirun::forecast` (a multi-node `WorkloadPlan` with dependencies, external
delays and shared setup, plus an overlay of demands the ledger does not
show yet). The crate is 0.x and that API is not stable.

### `canirun resolve <id> [--json] [--force]`

A managed command (one driven through `canirun::managed` by another
program) keeps its budget while its supervisor is gone, until nothing it
started is still running. `status` lists such entries as `held, owner gone`,
and `inspect` marks them `quarantined` with a `hint`. Most are released by
themselves once their processes stop. Two kinds are not: an entry whose
sidecar cannot be read, and a launch intent whose timestamp is missing. For
those, `canirun resolve <id>` runs the same checks on demand: the owner's
lease must be gone, and the child, its process group, every process of
yours carrying `CANIRUN_MANAGED_ID=<id>` (and this state directory's
`CANIRUN_MANAGED_SCOPE`) and every recorded descendant must be gone. It prints the entry's state, what was checked and the result, and
records the outcome as `interrupted`. Exit 0 means released, 1 means kept
(still charged), 2 an error, 3 that the ledger has no entry with that id
(it prints `no such entry`, and `--json` has `"found": false` and
`"verdict": "NoEntry"`). It never kills anything.

A launch intent (the supervisor died around the spawn, so it is unknown
whether a child exists) is released by the reaper itself once the lease is
gone, at least 60 seconds have passed since the intent and no process
carries the id. An unknown scan, or no timestamp, keeps it: time alone never
releases it. `resolve` skips the 60 seconds.

`resolve --force` is for an entry kept only because something **could not be
checked**: a process that could not be read, a launch intent that a hidden
process may belong to, or a record of descendants that did not fit. It
releases the entry anyway and records `interrupted` with `forced: <reason>`
(`--json` has `"forced": true`). It never releases what is known to be
running: a held lease, a running child, group, marked process or recorded
descendant keeps the entry (exit 1), with or without `--force`. Exit codes
are the same (0 released, 1 kept, 2 error, 3 no such entry).

How descendants are found, and what this cannot see:

- **Marker.** A managed command carries `CANIRUN_MANAGED_ID` and
  `CANIRUN_MANAGED_SCOPE` (the first 16 hex digits of the SHA-256 of the
  state directory's canonical path), which its descendants inherit. Two
  state directories that use the same command id do not see, hold or stop
  each other's processes. Only the innermost managed command is tracked:
  one started inside another overwrites the outer id and scope.
- **Hidden environments.** On macOS the kernel withholds the environment of
  Apple's own binaries (`/bin/sh`, `/bin/bash`, `/usr/bin/env`, `perl`,
  `sleep`, ...) and of hardened-runtime apps. Such a process is *hidden*:
  neither marked nor proven unmarked. A hidden process alone never makes a
  scan "unknown" (on one measured Mac about two thirds of the user's
  processes are hidden; that would block every release), and an empty
  environment cannot be told from a hidden one, so it counts as hidden too.
  Processes of other users are not scanned. On Linux every process of the
  user is readable, except one that made itself non-dumpable (an agent or
  sandboxed helper), which is read as unmarked.
- **Descendants are recorded while the command runs.** The runner reads the
  process table every 250 ms and records every process below the command
  (pid plus start time, following parent links) in the sidecar's
  `descendants` (at most 4096; more are not recorded and
  `descendants_truncated` is set; processes that exited are dropped from the
  record). A recorded process that still runs keeps the budget, however it
  left the process group (`setsid`, double fork), whatever its environment
  hides, and it is stopped like a marked one (SIGTERM, then SIGKILL, after
  checking pid and start time again). If descendants were dropped, a clear
  scan cannot be believed and the entry is kept as "could not be checked".
- **A launch intent and hidden processes.** A launch intent (the supervisor
  died around the spawn) is released by the reaper once the lease is gone,
  60 seconds have passed, no marked or recorded process runs, *and* no
  hidden process that started after the intent has `launchd`/`init` or the
  dead supervisor as its parent. Any such process keeps the entry (it may be
  the child); `resolve --force` overrides that. No timestamp keeps it.
  `resolve` skips the 60 seconds.
- A scan that finds nothing is repeated once and believed only if it finds
  nothing again; an unknown one is repeated a few times first.
- On macOS a process of yours whose environment cannot be read at all is
  hidden. The owner of a process comes from the kernel (`proc_pidinfo`).

What is not found, and what follows:

- A descendant that forks and loses its parent within one sampling interval
  (250 ms), before it was recorded, and whose environment is hidden or
  cleared: not found. The marker finds it if its environment is readable.
- A hidden descendant whose parent was never recorded and which is an orphan
  of something other than `launchd` (for example reparented to a user
  subreaper): not found once the command's own record is gone.
- A descendant that cleared its environment (`env -i ...`) loses the
  marker; it is still found if it was recorded while its parent lived.
- The process group check does not compare start times, so a pid that was
  reused by an unrelated process in the same group id can keep an entry held
  for a while. That errs on the side of keeping the budget.
- A scan that could not read some process of the user is unknown and
  releases nothing, until `resolve --force`.
- **A Mac entry that stays held.** On macOS a process that `launchd`
  started (an Apple binary whose environment is hidden) can be mistaken for
  the child of a launch intent or a started command: the reaper then
  answers "could not be checked" (`ProbeUnknown`) and the entry stays held.
  Only a hidden process that started in a window counts: from the child's
  start (the launch intent when that is unknown) to two seconds after the
  last time the recorder wrote (`recorded_at` in the sidecar); with no
  record at all the window has no end. Such a hold is expected, not a bug,
  and the way out is `canirun resolve <id> --force`, after checking that
  nothing of the command still runs.
- Recording the descendants that fails is written to the sidecar too
  (`descendants_truncated`), and the supervisor that saw it fail does not
  release either: the entry stays held until `resolve --force`.
- If the kernel cannot tell the owner of a process of yours (macOS
  `proc_pidinfo` refused both ways), a process whose environment is hidden
  is unreadable, the scan is unknown, and the entry is held. The way out
  is the same: `canirun resolve <id> --force`.
- Recording the descendants that fails (the thread panics or cannot start,
  or the command cannot be read while it runs) counts as "could not be
  checked", never as "no descendants". A recorded descendant is dropped only
  after two looks in a row did not find it.
  A look at the process table that fails, comes back empty, or does not list
  a command that still runs is a recorder failure too: it is written to the
  sidecar at once, during the run, so a supervisor that dies mid-run leaves
  a hold, not a release. `recorded_at` is a heartbeat: every good look
  stamps it (written about once a second), and the spawn writes the first.
- **More than 4096 live descendants.** Only 4096 are recorded; past that the
  rest are unknown, so the record is marked truncated and the entry stays
  held after the command ends, by design. The way out is
  `canirun resolve <id> --force`, after checking that nothing of the command
  still runs.
- On macOS a process whose environment the kernel refuses with anything but
  `EINVAL`/`EPERM`, or whose owner the kernel did not confirm, is
  unreadable and makes the scan unknown.

### `canirun config [--path]` and `canirun gc`

`config` prints the effective configuration (defaults merged with the
file). `gc` reaps dead entries and trims history to `history_keep`
records per key, and keeps the newest `history_keep` of the
`measurements/*.applied` markers that `managed::record_measurement`
leaves (one per measurement); both also happen on the fly, so `gc` is
only for tidiness.

## Configuration

`~/.config/canirun/config.toml` (or `$XDG_CONFIG_HOME/canirun/config.toml`).
Absent file means all defaults.

```toml
budget = 12                 # default: logical CPU count
default_weight = 2          # used when nothing is known about a command
test_initial_weight = 4     # ...unless it is a test runner
mutation_initial_weight = 6 # ...or a mutation-testing tool
history_keep = 200          # records kept per command key
progress_interval_secs = 5  # between progress lines on a terminal
progress_interval_secs_non_tty = 60 # heartbeat when stderr is not a terminal
memory_reserve_mib = 2048   # keep this much available before admitting
cpu_threshold_percent = 85  # hold the next start while CPU is busier
cpu_guard_max_wait_secs = 120 # ...but never for longer than this
front_overload_factor = 2.0 # --front waits while load average >= budget x this (0: never)
front_overload_max_wait_secs = 30 # ...but never for longer than this
backfill_max_job_secs = 300 # a command learned to take at most this may pass a head that does not fit (0: never)
backfill_max_wait_secs = 300 # ...until that head, held up by such commands, has waited this long
jobs_orphan_grace_secs = 30 # a job nobody `wait`s on is cancelled after this
jobs_keep_days = 7          # `gc` removes finished job files older than this

[[pattern]]                 # glob over the full argv string, first match wins
match = "*stryker*"
min_weight = 6              # learned weight may not drop below this

[[pattern]]
match = "*test:e2e*"
min_weight = 4

[[pattern]]
match = "*eslint*"
weight = 1                  # fixed; measured but never learned
```

Weight priority: `--weight` > pattern `weight` > learned (then
`min_weight`) > pattern `initial_weight` > the kind's initial weight >
`default_weight`. Floor 1, ceiling `budget`.

The kind comes from the key (the command found inside a wrapper, see
`exec`): mutation testing (`stryker`, `cargo mutants`, any word containing
`mutant` / `mutation`, such as `npm run test:mutation`) starts at
`mutation_initial_weight`; a test runner (`npm test`, `npm run test` /
`test:*`, `vitest`, `jest`, `mocha`, `playwright`, `pytest`, `cargo
test` / `nextest`, `go test`, `dart test`, `flutter test`) at
`test_initial_weight`. Once a run has been measured, the learned weight
takes over.

## How it works

State lives in `~/.local/state/canirun/` (or `$XDG_STATE_HOME/canirun/`):

- `ledger.json` — the queue: id, pid, state, weight, repo, argv, times.
  Writes are temp-file + rename under `ledger.lock`. A waiting `exec` that
  cannot get the lock skips that poll and tries again; `status`, `jobs`,
  `wait`, `cancel` and `submit` read the file without the lock when it
  stays busy for a second.
- `leases/<id>.lock` — one file per entry; the owning `canirun` holds an
  exclusive `flock` on it from registration until exit. **Liveness is
  "this flock cannot be taken."** Anyone holding the ledger lock probes
  each lease non-blockingly and drops the entries whose lock it obtains.
- `history.jsonl` — append-only measurements.

Admission is a pure function of the ledger plus a live snapshot: the
candidate must be the head of the queue (`--front` entries first, then
by enqueue order), `used + weight ≤ budget`, and — when something managed
is already running — available memory must cover its predicted RSS plus
the reserve and CPU usage must be under the threshold.

A candidate behind the head may start first (backfill) when all of these
hold, and the budget and guards then apply to it as to a head:

- its learned wall time is known and at most `backfill_max_job_secs`
  (a command with no history never passes anyone);
- every entry ahead of it either does not fit the budget or cannot start
  before it anyway (it is not the head and not short itself), so entries
  that will start are never overtaken and short commands keep their order;
- no entry ahead is being held up only by backfilled entries (it would
  fit without them) while being `--front` or having waited
  `backfill_max_wait_secs`. Such an entry gets the budget as soon as the
  short commands already running end; the waiting line says `queue; the
  entry ahead has waited long enough, nothing more passes it`.

A head held up by something else — a long build, or a server that never
exits — is passed for as long as that lasts, because backfill does not
delay it. When nothing
managed is running the live guards are skipped, so a machine kept busy by
processes canirun does not manage cannot hold the queue forever.

While a command runs, a background thread in its `canirun` samples the
CPU used by the command and all of its descendants once a second and
writes the peak of the last three samples to the ledger entry as
`using`. Admission charges each running entry `max(weight,
round(using))`, so a command that uses far more than its weight holds the
next start; a new or idle one is charged its weight. When the tree cannot
be measured, the declared weight is used. Measuring does not touch the
command. CPU spent in a Docker daemon or VM is not part of the tree
(see "Known blind spot").

Estimates are a plain simulation of that queue against the budget (it
asks the same admission function who starts at each step, so backfill is
included) using each entry's learned wall time (max of its last five usable runs; 60 s and
a trailing `?` when unknown). The live guards are not modelled, so an
estimate is a lower bound.

Weights are learned per `(repository, subdirectory, key)`, where the key
is the argv string or the command found inside a wrapper (see `exec`). The
repository is the main worktree (`git rev-parse --git-common-dir`), so
every linked worktree of a project shares one history; the subdirectory
(`git rev-parse --show-prefix`) keeps `api/` and `client/` of one monorepo
apart.

### Idle entries are charged their weight

A running entry measured at nearly zero cores (a development server, a
watch mode) still takes its full weight from the budget. Charging it
only what it measures would be wrong in two common cases: an integration
suite whose work runs in a Docker daemon or VM measures near zero in its
own process tree (see below) while the machine is busy, and an idle
server can start a full rebuild at any moment. Either way the budget
would admit more than the machine can take. Instead, `status` marks such
entries `idle` and the waiting line names them, so they can be stopped
or run without canirun.

### Known blind spot

`wait4` rusage covers the child process tree only. Work pushed into a
Docker daemon or VM by integration suites is invisible, so such commands
learn a parallelism near 1. Use `min_weight` on a pattern for them; the
live CPU and memory guards still see the real load.

## Integrating

### Git hooks

Keep working on machines without canirun:

```sh
# .husky/pre-push
if command -v canirun >/dev/null 2>&1; then
  canirun exec --front -- npm test
else
  npm test
fi
```

### What not to wrap

Only commands that burn CPU for tens of seconds or more: test suites,
builds, mutation testing, integration and e2e runs. Do not wrap `sleep`,
`git`, dependency installs, formatters, or one-second checks: each wrapped
command takes a slot (default weight 2) and queues behind heavier work.
Do not wrap development servers, watch modes or anything else that does
not exit on its own: it holds its weight for as long as it runs, and a
heavy head that does not fit next to it waits until it is stopped.

### Agent / CI-runner scripts

Prefix every unit with `canirun exec --`, or `submit` the independent
units and `wait` for them all in one call. A unit may wait minutes
before it starts; with `exec` run it in the background or under a
monitor rather than inside a foreground step with a short hard timeout.
Keep the `wait` alive for as long as you care about the result: if your
process dies, so do its jobs.

## Platform support

macOS and Linux. `flock`, `wait4`, process groups and signal forwarding
are POSIX; `ru_maxrss` units (bytes on macOS, KiB on Linux) are
normalised. The ledger scope is one OS user + `XDG_STATE_HOME`; canirun
does not coordinate across users, containers with a different `HOME`, or
machines.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
./scripts/coverage-gate.sh      # 100% lines on admit / weight / key / eta
cargo install cargo-schemata --locked --version 0.1.0   # once
cargo schemata --file src/admit.rs --file src/weight.rs --file src/key.rs --file src/eta.rs --file src/forecast.rs --file src/reap.rs --file src/receipt.rs --file src/marker.rs --file src/lineage.rs -- --lib
```

On a Homebrew Rust toolchain (no `llvm-tools-preview`), point `cargo llvm-cov`
at Homebrew's LLVM: `LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov ./scripts/coverage-gate.sh`.

The admission decision, weight derivation, command key, estimate and
forecast simulation are pure modules with 100% line coverage and a zero-survivor mutation gate in CI.
`tests/cli.rs` drives the binary end-to-end with isolated XDG directories.

## License

MIT
