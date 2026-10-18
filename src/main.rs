//! canirun — "Can I run?" A local admission controller for heavy commands.

mod admit;
mod cmd;
mod config;
mod display;
mod error;
mod eta;
mod gitctx;
mod history;
mod jobs;
mod key;
mod ledger;
mod live;
mod lock;
mod paths;
mod runner;
mod usage;
mod weight;

use clap::{Parser, Subcommand};
use config::Config;

#[derive(Parser)]
#[command(
    name = "canirun",
    version,
    about = "Can I run? Gate heavy commands behind a shared budget"
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
            eprintln!("canirun: {e}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> error::Result<i32> {
    let cfg = Config::load(&paths::config_file())?;
    match cli.cmd {
        Cmd::Exec {
            weight,
            front,
            key,
            command,
        } => cmd::exec::run(
            &cfg,
            cmd::exec::Opts {
                weight,
                front,
                key,
                argv: command,
            },
        ),
        Cmd::Status { json } => cmd::status::run(&cfg, json),
        Cmd::History {
            key,
            json,
            limit,
            command,
        } => cmd::history::run(
            &cfg,
            cmd::history::Opts {
                key,
                argv: command,
                json,
                limit,
            },
        ),
        Cmd::Config { path } => cmd::config::run(&cfg, path),
        Cmd::Gc => cmd::gc::run(&cfg),
        Cmd::Submit {
            detach,
            weight,
            front,
            key,
            command,
        } => cmd::job::submit(
            &cfg,
            cmd::exec::Opts {
                weight,
                front,
                key,
                argv: command,
            },
            detach,
        ),
        Cmd::Wait { no_output, ids } => cmd::job::wait(&ids, no_output),
        Cmd::Jobs { json } => cmd::job::list(&cfg, json),
        Cmd::Cancel { ids } => cmd::job::cancel(&ids),
        Cmd::RunJob { id } => cmd::job::run_job(&cfg, &id),
    }
}
