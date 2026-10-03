//! jj-fork maintains a fork as independent jj series on top of upstream, plus glues that hold the
//! resolutions between series, combined into a generated fork branch. It keeps them current as
//! upstream moves.

mod checks;
mod config;
mod glue;
mod init;
mod native;
mod reconcile;
mod repo;
mod run;
mod sync;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::config::Config;
use crate::repo::Repo;
use crate::sync::{Options, Session};

#[derive(Parser)]
#[command(name = "jj-fork", version, about)]
struct Cli {
    /// Config file to use instead of the repository's .jj-fork.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prepare this clone: remotes, full history, jj, bookmark tracking, and revset aliases.
    Init {
        /// Upstream repository URL. Writes a starter .jj-fork.toml when none exists.
        #[arg(long)]
        upstream: Option<String>,
    },
    /// Report whether each series is up-to-date, clean, conflicted, or broken on upstream.
    /// Exits 0 when nothing is stale, 10 when every stale series is clean, 20 when an agent is needed.
    Check(SyncArgs),
    /// Rebase clean series onto upstream and assemble the fork branch. Changes nothing unless
    /// every stale series is clean.
    Sync(SyncArgs),
    /// Restack glues, then build, check, and move the fork branch to a merge of upstream, every
    /// series, and every glue.
    Assemble {
        #[command(flatten)]
        args: SyncArgs,
        /// Check and publish an existing merge, such as one whose conflicts were resolved by hand.
        #[arg(long)]
        candidate: Option<String>,
    },
    /// Add `aliases.fork` to your jj config so `jj fork` runs this tool.
    Alias,
}

#[derive(Args)]
struct SyncArgs {
    /// Upstream revision to update to. Defaults to the upstream branch after fetching.
    #[arg(long)]
    target: Option<String>,
    /// Do not fetch the fork and upstream remotes first.
    #[arg(long)]
    no_fetch: bool,
    /// Skip the configured checks; detect conflicts only.
    #[arg(long)]
    no_checks: bool,
    /// Push the fork branch, every series and glue, and a fast-forwarded mirror branch after
    /// success. Refuses if any of them changed on the remote during the run.
    #[arg(long)]
    push: bool,
}

pub fn progress(message: &str) {
    eprintln!("jj-fork: {message}");
}

pub fn report(line: &str) {
    println!("{line}");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("jj-fork: error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn execute(cli: Cli) -> Result<i32> {
    let cwd = std::env::var_os("JJ_WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if let Command::Alias = cli.command {
        init::install_alias(&cwd)?;
        return Ok(0);
    }
    let repo = Repo::discover(&cwd)?;
    if let Command::Init {
        upstream: Some(url),
    } = &cli.command
        && cli.config.is_none()
        && !repo.root.join(config::FILE_NAME).exists()
    {
        init::write_starter_config(&repo.root, url)?;
    }
    let config = Config::load(&repo.root, cli.config.as_deref())?;
    let options = |args: SyncArgs, candidate: Option<String>| Options {
        target: args.target,
        candidate,
        fetch: !args.no_fetch,
        checks: !args.no_checks,
        push: args.push,
    };
    let (mut session, command) = match cli.command {
        Command::Init { .. } => {
            init::init(&repo, &config)?;
            return Ok(0);
        }
        Command::Alias => unreachable!(),
        Command::Check(args) => (Session::new(&repo, &config, options(args, None))?, "check"),
        Command::Sync(args) => (Session::new(&repo, &config, options(args, None))?, "sync"),
        Command::Assemble { args, candidate } => (
            Session::new(&repo, &config, options(args, candidate))?,
            "assemble",
        ),
    };
    let code = match command {
        "check" => session.check()?,
        "sync" => session.sync()?,
        _ => session.assemble_command()?,
    };
    session.finish();
    Ok(code)
}
