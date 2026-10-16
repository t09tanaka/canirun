//! canirun — "Can I run?" A local admission controller for heavy commands.

mod admit;
mod cmd;
mod config;
mod display;
mod error;
mod gitctx;
mod history;
mod key;
mod ledger;
mod live;
mod lock;
mod paths;
mod runner;
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
    }
}
