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
  repository it ran in. Nothing to hand-tune; `config.toml` holds only
  hints and exceptions.
- **Live brakes.** Before the head of the queue starts, canirun also
  checks the machine as it is right now: available memory must cover the
  command's learned peak RSS plus a reserve, and global CPU usage must be
  under a threshold. These delay the *next* start; a running command is
  never paused or killed.
- **Crash-safe queue.** Each waiting or running entry holds an `flock` on
  its own lease file. A Ctrl-C'd, SIGKILLed, or crashed `canirun` — waiting
  or running — vanishes from the ledger on the next poll. No cleanup step,
  no daemon, no pid heuristics.
- **FIFO with reservation.** The head of the queue reserves until it fits,
  so a heavy unit is never starved by a stream of light ones. `--front`
  lets a human at the keyboard jump ahead.
- **Nesting-safe.** A gated command that itself calls `canirun exec`
  inherits the outer lease instead of deadlocking.
- **Async jobs that die with their waiter.** `submit` returns a job id at
  once; `wait` collects the log and exit code later. A job nobody is
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
  whose arguments carry temp paths or timestamps.

While waiting, a progress line goes to stderr every few seconds:

```
canirun: waiting 0:42 (budget) — running 3 (10/12 used), ahead of you 2
canirun: waiting 1:07 (memory 1.1 GiB available, need 3.4 GiB) — running 2 (6/12 used)
canirun: waiting 0:12 (cpu 93% busy, threshold 85%) — running 2 (6/12 used)
```

Nothing is written to stdout by canirun itself.

### `canirun status`

```
budget 10/12 used
machine 31.2 GiB available, cpu 71% busy

running (3)
    48211  w4      2:14  webapp: npm run test:e2e
    48190  w4      1:03  webapp: npm run test:e2e
    48260  w2      0:20  api: npm run test:unit

waiting (2)
    48302  w6      0:41  [front] api: npx stryker run
    48310  w1      0:40  webapp: npm run lint
```

`--json` dumps the ledger. Dead entries are reaped first, so the view is
accurate without a daemon.

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
environment and cwd inherited) and prints the job id on stdout, with a
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

### `canirun config [--path]` and `canirun gc`

`config` prints the effective configuration (defaults merged with the
file). `gc` reaps dead entries and trims history to `history_keep`
records per key; both also happen on the fly, so `gc` is only for
tidiness.

## Configuration

`~/.config/canirun/config.toml` (or `$XDG_CONFIG_HOME/canirun/config.toml`).
Absent file means all defaults.

```toml
budget = 12                 # default: logical CPU count
default_weight = 2          # used when nothing is known about a command
history_keep = 200          # records kept per command key
progress_interval_secs = 5
memory_reserve_mib = 2048   # keep this much available before admitting
cpu_threshold_percent = 85  # hold the next start while CPU is busier
jobs_orphan_grace_secs = 30 # a job nobody `wait`s on is cancelled after this
jobs_keep_days = 7          # `gc` removes finished job files older than this

[[pattern]]                 # glob over the argv string, first match wins
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
`min_weight`) > pattern `initial_weight` > `default_weight`. Floor 1,
ceiling `budget`.

## How it works

State lives in `~/.local/state/canirun/` (or `$XDG_STATE_HOME/canirun/`):

- `ledger.json` — the queue: id, pid, state, weight, repo, argv, times.
  Writes are temp-file + rename under `ledger.lock`.
- `leases/<id>.lock` — one file per entry; the owning `canirun` holds an
  exclusive `flock` on it from registration until exit. **Liveness is
  "this flock cannot be taken."** Anyone holding the ledger lock probes
  each lease non-blockingly and drops the entries whose lock it obtains.
- `history.jsonl` — append-only measurements.

Admission is a pure function of the ledger plus a live snapshot: the
candidate must be the head of the queue (`--front` entries first, then
by enqueue order), `used + weight ≤ budget`, and — when something managed
is already running — available memory must cover its predicted RSS plus
the reserve and CPU usage must be under the threshold. When nothing
managed is running the live guards are skipped, so a machine kept busy by
processes canirun does not manage cannot hold the queue forever.

Estimates are a plain simulation of that queue against the budget using
each entry's learned wall time (max of its last five usable runs; 60 s and
a trailing `?` when unknown). The live guards are not modelled, so an
estimate is a lower bound.

Weights are learned per `(repository, argv)`. The repository is the main
worktree (`git rev-parse --git-common-dir`), so every linked worktree of a
project shares one history.

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
cargo mutants --file src/admit.rs --file src/weight.rs --file src/key.rs --file src/eta.rs
```

On a Homebrew Rust toolchain (no `llvm-tools-preview`), point `cargo llvm-cov`
at Homebrew's LLVM: `LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov ./scripts/coverage-gate.sh`.

The admission decision, weight derivation, command key and estimate are
pure modules with 100% line coverage and a zero-survivor mutation gate in CI.
`tests/cli.rs` drives the binary end-to-end with isolated XDG directories.

## License

MIT
