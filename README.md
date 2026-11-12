# task-traffic-controller

A local admission controller for heavy commands.

A command asks before it starts, waits until the machine has room, runs,
and reports what it cost so the next run of the same command knows its
weight. No daemon, no process supervision, one JSON ledger per user.

```sh
task-traffic-controller exec -- npm test                 # waits for budget, then runs
task-traffic-controller exec --front -- npm run lint     # a human is waiting: jump the queue
task-traffic-controller status                           # who is running, who is waiting, why, and when
task-traffic-controller top                              # the same screen, redrawn every 2 s until Ctrl-C
task-traffic-controller history                          # what each command measured, next weight

id=$(task-traffic-controller submit -- npm run test:e2e) # async: returns a job id and a rough ETA
task-traffic-controller wait "$id"                       # keeps the job alive, prints its log, mirrors its exit
```

## What & why

When several agent sessions each reproduce CI locally at the same time —
lint, unit, build, e2e, mutation tests, times ten — a 12-core laptop ends
up running 30–50 heavy processes. Everything slows down and the total
takes longer than running them one after another. The same happens when a
`pre-push` hook starts mutation tests while the machine is already
saturated.

`task-traffic-controller` is the traffic controller:

- **One budget.** A number of "slots" (default: logical CPU count) shared
  by every `task-traffic-controller exec` of the same OS user. The sum of running weights
  never exceeds it.
- **Learned weights.** Every run is measured (`wait4` rusage: CPU time,
  wall time, peak RSS). A command's weight is the ceiling of the highest
  CPU-parallelism it showed in its last five usable runs, scoped to the
  repository and subdirectory it ran in. Runs that started on a saturated
  machine are marked `contended` and ignored while calmer runs exist,
  because contention makes parallelism read low. Nothing to hand-tune;
  `config.toml` holds only hints and exceptions.
- **Live brakes.** Before the head of the queue starts, task-traffic-controller also
  checks the machine as it is right now: available memory must cover the
  command's learned peak RSS plus a reserve, and global CPU usage must be
  under a threshold. These delay the *next* start; a running command is
  never paused or killed. `--front` entries skip the CPU brake, and any
  head gives up on it after `cpu_guard_max_wait_secs`, so load that does
  not go through task-traffic-controller cannot hold the queue indefinitely. On a
  machine that is badly overloaded (load average at least
  `front_overload_factor` x budget) a `--front` entry still waits, for at
  most `front_overload_max_wait_secs` (30 s). Likewise, measured use
  beyond the running entries' weights holds a `--front` entry for at most
  that long; after that only declared weights count for it. (The load
  average is the trigger because measured cores cannot exceed the core
  count, so "twice the budget" is only visible as runnable processes.)
- **Measured, not just declared.** While a command runs, task-traffic-controller samples
  the CPU its whole process tree uses. A command declared (or learned) at
  weight 2 that is using 5 cores takes 5 from the budget until it slows
  down, so the next start waits instead of piling on. The running
  command itself is never slowed or stopped.
- **Crash-safe queue.** Each waiting or running entry holds an `flock` on
  its own lease file. A Ctrl-C'd, SIGKILLed, or crashed `task-traffic-controller` — waiting
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
- **Nesting-safe.** A gated command that itself calls `task-traffic-controller exec`
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
cargo install task-traffic-controller
```

Coming from `canirun` (before 0.5.0)? Run `cargo uninstall canirun` first.
The old state and config directories are not read; delete
`~/.local/state/canirun` and `~/.config/canirun` when you are done with them.

Requires macOS or Linux. Prebuilt binaries for macOS (Apple Silicon /
Intel) and Linux (x86_64 / arm64) are attached to each
[release](https://github.com/t09tanaka/task-traffic-controller/releases/latest).

## Usage

### `task-traffic-controller exec`

```
task-traffic-controller exec [--weight N] [--front] [--key K] -- <command> [args...]
```

Waits until admitted, then runs `<command>` with inherited stdin/stdout/
stderr in task-traffic-controller's own process group, with `TASK_TRAFFIC_CONTROLLER_LEASE=<id>` in its
environment. The exit code is the child's; a child killed by a signal
makes task-traffic-controller die from the same signal. The command runs directly
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
  shares its history with `task-traffic-controller exec -- npm run test`.
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
task-traffic-controller: waiting 0:42 (queue) — running 3 (10/12 used), ahead of you 2
task-traffic-controller: waiting 52:10 (budget) — running 2 (4/12 used); held by 48211 w2 idle 52:30 api: make api, 48190 w2 idle 51:02 app: flutter run
task-traffic-controller: waiting 1:07 (memory 1.1 GiB available, need 3.4 GiB) — running 2 (6/12 used)
task-traffic-controller: waiting 0:12 (cpu 93% busy, threshold 85%) — running 2 (6/12 used)
```

A line waiting on the budget names the running entries holding it
(biggest first, at most three): pid, weight, `idle` or `using N`, and how
long they have run, so a forgotten server is easy to find and stop.

When stderr is not a terminal (a log file, an agent's tool output), a line
is printed only when the reason or the queue position changes, plus one
every `progress_interval_secs_non_tty` (default 60) as a heartbeat, so a
long wait does not fill the output with identical lines.

If the program is not found and the command after `--` is a single
argument containing spaces (`-- "npm run x"`), task-traffic-controller adds a hint to pass
the words separately (`-- npm run x`) or use `sh -c '...'`.

Nothing is written to stdout by task-traffic-controller itself.

### `task-traffic-controller top`

Keeps the `status` screen on the terminal and redraws it in place every
`--interval` seconds (default 2), with the time of the last redraw on top.
It only reads and reaps like `status`; quit with Ctrl-C.

### `task-traffic-controller status`

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

### `task-traffic-controller history [--key K] [--json] [--limit N] [-- <command>]`

Measured runs per command key, and the weight and memory that will be
used next:

```
webapp: npm run test:e2e
  next: weight 4 (Learned), memory 3.4 GiB
  10-16 21:04  wall   184.2s  cpu   611.0s  x3.3  rss   3.4 GiB  exit 0   w4
  10-16 20:12  wall   190.7s  cpu   640.1s  x3.4  rss   3.2 GiB  exit 0   w2
```

### `task-traffic-controller submit` / `wait` / `jobs` / `cancel`

```
task-traffic-controller submit [--detach] [--weight N] [--front] [--key K] -- <command> [args...]
task-traffic-controller wait [--no-output] <job-id>...
task-traffic-controller jobs [--json]
task-traffic-controller cancel <job-id>...
```

`submit` writes a job descriptor, starts a detached runner (own session,
stdin from `/dev/null`, stdout and stderr merged into the job log,
environment and cwd inherited), waits until the runner has joined the
queue (so back-to-back submits keep their order; at most 5 s, after which
it warns and returns anyway) and prints the job id on stdout, with a
rough estimate on stderr:

```
$ id=$(task-traffic-controller submit -- npm run test:e2e)
task-traffic-controller: job 3f9c… start in ~1:10, done in ~4:20
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

### `task-traffic-controller inspect --json` and `task-traffic-controller estimate --json -- <command>`

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

The same machinery is available to Rust callers as `task_traffic_controller::inspect` and
`task_traffic_controller::forecast` (a multi-node `WorkloadPlan` with dependencies, external
delays and shared setup, plus an overlay of demands the ledger does not
show yet). The crate is 0.x and that API is not stable.

### `task-traffic-controller resolve <id> [--json] [--force]`

A managed command (one driven through `task_traffic_controller::managed` by another
program) needs every `task-traffic-controller` that shares the state directory to be
upgraded: a ledger that holds a managed entry is written in a new shape
(`entries_v2`), and an older binary refuses to load it rather than reap
entries it cannot recognise (every command then exits 2). To go back to an
older binary, settle every managed entry (`task-traffic-controller resolve <id>`, or
`--force`); the next save writes the old shape (`version` 1) and older
binaries work again. `task-traffic-controller status` shows the format (`ledger_format` in
`--json`). It keeps its budget while its supervisor is gone, until nothing it
started is still running. `status` lists such entries as `held, owner gone`,
and `inspect` marks them `quarantined` with a `hint`. Most are released by
themselves once their processes stop. Two kinds are not: an entry whose
sidecar cannot be read, and a launch intent whose timestamp is missing. For
those, `task-traffic-controller resolve <id>` runs the same checks on demand: the owner's
lease must be gone, and the child, its process group, every process of
yours carrying `TASK_TRAFFIC_CONTROLLER_MANAGED_ID=<id>` (and this state directory's
`TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE`) and every recorded descendant must be gone. It prints the entry's state, what was checked and the result, and
records the outcome as `interrupted`. An id whose sidecar is not terminal
but has no ledger entry (stranded, for example by an older binary) is settled
the same way. Exit 0 means released, 1 means kept
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
running: a held lease, a running child, a marked process or a recorded
descendant keeps the entry (exit 1), with or without `--force`. A live
process group alone does not count as running when the group number was
reused, and only positive evidence shows that: a process whose pid is the
recorded group number is alive and started at a different time than the
recorded child (a new leader), the child is gone, and no process carries
the run's id or is a recorded descendant. Then `--force` releases the entry
(`forced: the recorded process group number now belongs to a different
process group`). A live group with no such leader, or one whose start time
cannot be compared, may be the original group (members without the marker)
and stays held. Exit codes
are the same (0 released, 1 kept, 2 error, 3 no such entry).

How descendants are found, and what this cannot see:

- **Marker.** A managed command carries `TASK_TRAFFIC_CONTROLLER_MANAGED_ID` and
  `TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE` (the first 16 hex digits of the SHA-256 of the
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
- A hidden orphan that is *not* a descendant is ruled out when its session
  proves it: the session is `launchd`'s own (id 1), or its leader still runs
  and started before the command (a managed command leads its own session,
  and a session cannot be joined). One that leads its own session, or whose
  session leader is gone or started after the command, cannot be told from
  a real escapee and keeps the entry; `resolve` names it in `checked`.
  "Started before the command" is not read off the wall clock, which can be
  set back: the command's start is recorded as clock ticks since boot (Linux)
  or Mach absolute time (macOS) together with the boot id, and compared with
  the leader's. A leader is older only on the same boot and with a strictly
  smaller count; an entry recorded by an earlier version has no such count
  and is never released this way.
- The session rule draws a line at `launchd` / `init`: a process counts as a
  descendant only if it was forked below the command. Work the command
  *asks* `launchd` or an already-running daemon to start (`open`,
  `launchctl submit`, an XPC request; on Linux `systemd-run`, D-Bus
  activation) is not counted, and does not keep the entry.
- A descendant that runs as another user (it ran a setuid program) is not in
  the scan at all, hidden or not: not found once its parent is gone.
- A hidden descendant that starts after the wall clock was set back to
  before the command's start looks older than the command: not found.
- A descendant that cleared its environment (`env -i ...`) loses the
  marker; it is still found if it was recorded while its parent lived, and
  in any case while it stays in the run's **session**.
- **Session.** A managed command is started in its own session (`setsid`;
  its session id and process group id are the child's pid, recorded in the
  sidecar as `sid`). `env -i`, a double fork, a process born after the
  supervisor died or between two samples, and a job-control shell that
  moves its jobs to new process groups all stay in it, so the reaper and
  `resolve --force` see them as members and keep the entry. A session id is
  a pid, so it counts only while it is still the child's session: a process
  that leads that number now and started at a different time than the
  recorded child is a different session (reuse), and its members are not
  counted. If a process's session cannot be read the scan is unknown, as
  for an unreadable environment. A sidecar written before this has no `sid`
  and is judged as before. A consequence: the command has no controlling
  terminal of its own.
- **Left the session.** A daemon that calls `setsid` deliberately gets out
  of the run's session (`setpgid` cannot: POSIX does not let a process move
  into a group of another session), and
  with an empty environment, born after the tracker last looked, nothing
  finds it: the entry can be freed while it runs. This is the residual
  limitation. Another: the session check lists processes through the same
  process table as everything else, so a process that table does not list is
  missed; the marker and the recorded descendants still catch it in the
  common cases.
- The process group check does not compare start times, so a group number
  that was reused by an unrelated group keeps an entry held for as long as
  that group lives. That errs on the side of keeping the budget; the way out
  is `task-traffic-controller resolve <id> --force`, which releases it only when a new
  leader (the process whose pid is the group number, started at a different
  time than the recorded child) proves the number was reused, nothing in the
  group carries the run's id or is a recorded descendant, and the child is
  gone. Without that proof the group may be the original one (a member that
  cleared its environment, or was born after tracking stopped) and stays.
- A scan that could not read some process of the user is unknown and
  releases nothing, until `resolve --force`.
- **A Mac entry that stays held.** On macOS a process that `launchd`
  started (an Apple binary whose environment is hidden) can be mistaken for
  the child of a launch intent or a started command: the reaper then
  answers "could not be checked" (`ProbeUnknown`) and the entry stays held.
  Only a hidden process that started in a window counts: from the child's
  start (the launch intent when that is unknown) to two seconds after the
  last time the recorder wrote (`recorded_at` in the sidecar). That end
  applies only when the supervisor saw the command exit, took the last
  look and confirmed everything it left stopped (`tracked_to_exit` in the
  sidecar, stamped after that stop); with no record, or when the
  supervisor died mid-run (the command may have forked later), the window
  has no end. Such a hold is expected, not a bug,
  and the way out is `task-traffic-controller resolve <id> --force`, after checking that
  nothing of the command still runs.
- Recording the descendants that fails is written to the sidecar too
  (`descendants_truncated`), and the supervisor that saw it fail does not
  release either: the entry stays held until `resolve --force`.
- If the kernel cannot tell the owner of a process of yours (macOS
  `proc_pidinfo` refused both ways), a process whose environment is hidden
  is unreadable, the scan is unknown, and the entry is held. The way out
  is the same: `task-traffic-controller resolve <id> --force`.
- Recording the descendants that fails (the thread panics or cannot start,
  or the command cannot be read while it runs) counts as "could not be
  checked", never as "no descendants". A recorded descendant is dropped only
  after two looks in a row did not find it.
  A look at the process table that fails, comes back empty, or does not list
  a command that still runs is a recorder failure too: it is written to the
  sidecar at once, during the run, so a supervisor that dies mid-run leaves
  a hold, not a release. A write that fails is retried every tick until one
  succeeds. `recorded_at` is a heartbeat: every good look stamps it (written
  about once a second; a failed look never stamps it), and the spawn writes
  the first.
- A single failed look is sticky on purpose: even a transient read failure
  of the process table marks the record truncated, and the run is held until
  `task-traffic-controller resolve <id> --force`. The last look is told the command was
  reaped, so it does not ask the kernel about a pid that may be reused.
- **More than 4096 live descendants.** Only 4096 are recorded; past that the
  rest are unknown, so the record is marked truncated and the entry stays
  held after the command ends, by design. The way out is
  `task-traffic-controller resolve <id> --force`, after checking that nothing of the command
  still runs.
- On macOS a process whose environment the kernel refuses with anything but
  `EINVAL`/`EPERM`, or whose owner the kernel did not confirm, is
  unreadable and makes the scan unknown.

### `task-traffic-controller config [--path]` and `task-traffic-controller gc`

`config` prints the effective configuration (defaults merged with the
file). `gc` reaps dead entries and trims history to `history_keep`
records per key (under the history lock), removes the
`measurements/*.applied` markers that `managed::record_measurement` leaves
once their record is trimmed away (a marker is kept as long as its
record), and removes the receipts of managed commands (`managed/<id>.json`)
that ended more than `managed_keep_days` (default 7; 1 to 3650, anything else
is a configuration error) ago and have no ledger
entry, together with leftover temporary files of that age. A command that
has not ended is never removed. Reaping also happens on the fly, so `gc` is
only for tidiness.

## Configuration

`~/.config/task-traffic-controller/config.toml` (or `$XDG_CONFIG_HOME/task-traffic-controller/config.toml`).
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
managed_keep_days = 7       # ...and receipts of managed commands that ended longer ago (1..3650)

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

State lives in `~/.local/state/task-traffic-controller/` (or `$XDG_STATE_HOME/task-traffic-controller/`):

- `ledger.json` — the queue: id, pid, state, weight, repo, argv, times.
  Writes are temp-file + rename under `ledger.lock`. A waiting `exec` that
  cannot get the lock skips that poll and tries again; `status`, `jobs`,
  `wait`, `cancel` and `submit` read the file without the lock when it
  stays busy for a second.
- `leases/<id>.lock` — one file per entry; the owning `task-traffic-controller` holds an
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
processes task-traffic-controller does not manage cannot hold the queue forever.

While a command runs, a background thread in its `task-traffic-controller` samples the
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
or run without task-traffic-controller.

### Known blind spot

`wait4` rusage covers the child process tree only. Work pushed into a
Docker daemon or VM by integration suites is invisible, so such commands
learn a parallelism near 1. Use `min_weight` on a pattern for them; the
live CPU and memory guards still see the real load.

## Integrating

### Git hooks

Keep working on machines without task-traffic-controller:

```sh
# .husky/pre-push
if command -v task-traffic-controller >/dev/null 2>&1; then
  task-traffic-controller exec --front -- npm test
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

Prefix every unit with `task-traffic-controller exec --`, or `submit` the independent
units and `wait` for them all in one call. A unit may wait minutes
before it starts; with `exec` run it in the background or under a
monitor rather than inside a foreground step with a short hard timeout.
Keep the `wait` alive for as long as you care about the result: if your
process dies, so do its jobs.

## Platform support

macOS and Linux. `flock`, `wait4`, process groups and signal forwarding
are POSIX; `ru_maxrss` units (bytes on macOS, KiB on Linux) are
normalised. The ledger scope is one OS user + `XDG_STATE_HOME`; task-traffic-controller
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
