//! task-traffic-controller: a local admission controller for heavy commands.

mod cmd;
mod env;
mod signals;

use clap::{Parser, Subcommand};
use task_traffic_controller::{config::Config, error, exec::Opts};

#[derive(Parser)]
#[command(
    name = "task-traffic-controller",
    version,
    about = "Gate heavy commands behind a shared budget"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wait for budget, then run the command and record what it cost
    Exec {
        /// Override the derived weight for this run
        #[arg(long)]
        weight: Option<u32>,
        /// Jump to the front of the queue (for a human at the keyboard)
        #[arg(long)]
        front: bool,
        /// Use K instead of the argv string as the history key
        #[arg(long)]
        key: Option<String>,
        /// Command and arguments (after `--`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show running and waiting entries
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show measured runs and the weight that will be used next
    History {
        /// Only this explicit key (as given to `exec --key`)
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        json: bool,
        /// Records to show per key
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Only this command (same resolution as `exec`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Read-only snapshot of the ledger, policy and live metrics (JSON)
    Inspect {
        /// Accepted for symmetry with `status`; the output is always JSON
        #[arg(long)]
        json: bool,
    },
    /// Predict when a command would start and finish, without queueing it
    Estimate {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        weight: Option<u32>,
        #[arg(long)]
        front: bool,
        #[arg(long)]
        key: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Release a ledger entry whose owner is gone, after checking that
    /// nothing it started is still running
    Resolve {
        id: String,
        #[arg(long)]
        json: bool,
        /// Release an entry kept only because a process could not be
        /// checked (never one that is known to be running)
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration
    Config {
        /// Print only the config file path
        #[arg(long)]
        path: bool,
    },
    /// Reap dead ledger entries and trim history
    Gc,
    /// Start a job in the background and print its id
    Submit {
        /// Keep running even when nobody `wait`s on the job
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        weight: Option<u32>,
        #[arg(long)]
        front: bool,
        #[arg(long)]
        key: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Wait for jobs, print their logs, exit with the first failure
    Wait {
        /// Print only the result line per job, not the log
        #[arg(long)]
        no_output: bool,
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// List submitted jobs
    Jobs {
        #[arg(long)]
        json: bool,
    },
    /// Stop queued or running jobs
    Cancel {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    #[command(hide = true, name = "__run-job")]
    RunJob { id: String },
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("task-traffic-controller: {e}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> error::Result<i32> {
    let ctx = env::context();
    let cfg = Config::load(ctx.paths.config_file())?;
    match cli.cmd {
        Cmd::Exec {
            weight,
            front,
            key,
            command,
        } => cmd::exec::run(&ctx, &cfg, opts(weight, front, key, command)?),
        Cmd::Status { json } => cmd::status::run(&ctx, &cfg, json),
        Cmd::History {
            key,
            json,
            limit,
            command,
        } => cmd::history::run(
            &ctx,
            &cfg,
            cmd::history::Opts {
                key,
                argv: command,
                json,
                limit,
            },
        ),
        Cmd::Inspect { json: _ } => cmd::inspect::run(&ctx, &cfg),
        Cmd::Estimate {
            json,
            weight,
            front,
            key,
            command,
        } => cmd::estimate::run(&ctx, &cfg, opts(weight, front, key, command)?, json),
        Cmd::Resolve { id, json, force } => cmd::resolve::run(&ctx, &id, json, force),
        Cmd::Config { path } => cmd::config::run(&ctx, &cfg, path),
        Cmd::Gc => cmd::gc::run(&ctx, &cfg),
        Cmd::Submit {
            detach,
            weight,
            front,
            key,
            command,
        } => cmd::job::submit(&ctx, &cfg, opts(weight, front, key, command)?, detach),
        Cmd::Wait { no_output, ids } => cmd::job::wait(&ctx, &ids, no_output),
        Cmd::Jobs { json } => cmd::job::list(&ctx, &cfg, json),
        Cmd::Cancel { ids } => cmd::job::cancel(&ctx, &ids),
        Cmd::RunJob { id } => cmd::job::run_job(&ctx, &cfg, &id),
    }
}

/// The command to run, from the current directory.
fn opts(
    weight: Option<u32>,
    front: bool,
    key: Option<String>,
    argv: Vec<String>,
) -> error::Result<Opts> {
    Ok(Opts {
        weight,
        front,
        key,
        argv,
        cwd: std::env::current_dir()?,
        ..Opts::default()
    })
}
