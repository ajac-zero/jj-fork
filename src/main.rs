//! jj-fork maintains a fork as independent jj series on top of upstream, plus glues that hold the
//! resolutions between series, combined into a generated fork branch. It keeps them current as
//! upstream moves.

mod artifact;
mod checks;
mod config;
mod glue;
mod init;
mod native;
mod reconcile;
mod repair;
mod repo;
mod run;
mod skill;
mod sync;
mod workflow;

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
    /// Start an empty, editable series on upstream. Never assembles or pushes.
    Create {
        /// Full bookmark name under a configured series prefix, e.g. patch/my-fix.
        name: String,
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        no_fetch: bool,
    },
    /// Remove a series and explicitly approved dependent glues after checking the reduced fork.
    /// Remote refs and separate PR-head bookmarks are retained.
    Retire {
        name: String,
        /// A dependent glue whose entire resolution may be removed; repeat for every dependency.
        #[arg(long)]
        remove_glue: Vec<String>,
        #[arg(long)]
        no_fetch: bool,
        #[arg(long, conflicts_with = "save_plan")]
        push: bool,
        #[arg(long)]
        save_plan: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Authenticate and apply an exact saved proposal after rerunning its required checks.
    Apply {
        file: PathBuf,
        #[arg(long)]
        push: bool,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Create isolated repair tasks or validate submissions into a successor saved plan.
    Repair {
        #[command(subcommand)]
        command: repair::Command,
    },
    /// Print or install the agent skills bundled with this version of jj-fork.
    Skill {
        /// Skill name; omit to list them (or, with --install, to install all).
        name: Option<String>,
        /// Write SKILL.md into .agents/skills/<name>/ of this repository.
        #[arg(long)]
        install: bool,
    },
    /// Add `aliases.fork` to your jj config so `jj fork` runs this tool.
    Alias,
}

#[derive(Args, Default)]
struct SyncArgs {
    /// Upstream revision to update to. Defaults to the upstream branch after fetching.
    #[arg(long)]
    target: Option<String>,
    /// Do not fetch the fork and upstream remotes first.
    #[arg(long)]
    no_fetch: bool,
    /// Skip the configured checks; detect conflicts only.
    #[arg(long, conflicts_with = "save_plan")]
    no_checks: bool,
    /// Push the fork branch, every series and glue, and a fast-forwarded mirror branch after
    /// success. Refuses if any of them changed on the remote during the run.
    #[arg(long, conflicts_with = "save_plan")]
    push: bool,
    /// Write one typed, versioned JSON report (text output remains unchanged).
    #[arg(long)]
    report: Option<PathBuf>,
    /// Build and check an executable saved plan without publishing maintenance or pushing.
    #[arg(long)]
    save_plan: Option<PathBuf>,
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
    let report_path = match &cli.command {
        Command::Check(args) | Command::Sync(args) | Command::Assemble { args, .. } => {
            args.report.clone()
        }
        Command::Apply { report, .. } | Command::Retire { report, .. } => report.clone(),
        _ => None,
    };
    let mut reported = false;
    let result = execute_inner(cli, &mut reported);
    if let Err(error) = &result
        && !reported
        && let Some(path) = report_path
    {
        artifact::save_report(
            &path,
            &workflow::Report {
                plan: None,
                diagnostics: vec![workflow::Issue {
                    id: "command_error:prepare".into(),
                    code: "command_error".into(),
                    subject: "prepare".into(),
                    candidate: None,
                    tier: None,
                    message: format!("{error:#}"),
                }],
                exit_code: 1,
                published_operation: None,
                push: None,
            },
        )?;
    }
    result
}

fn execute_inner(cli: Cli, reported: &mut bool) -> Result<i32> {
    let cwd = std::env::var_os("JJ_WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if let Command::Alias = cli.command {
        init::install_alias(&cwd)?;
        return Ok(0);
    }
    let repo = Repo::discover(&cwd)?;
    if let Command::Skill { name, install } = &cli.command {
        return skill::run(&repo.root, name.as_deref(), *install);
    }
    if let Command::Apply { file, push, report } = &cli.command {
        *reported = true; // Apply writes its own authenticated or initial-error report.
        return workflow::apply(&repo, file, cli.config.as_deref(), *push, report.as_deref());
    }
    if let Command::Init {
        upstream: Some(url),
    } = &cli.command
        && cli.config.is_none()
        && !config::resolve_path(&repo.root, None).exists()
    {
        init::write_starter_config(&repo.root, url)?;
    }
    let config = Config::load(&repo.root, cli.config.as_deref())?;
    if matches!(
        cli.command,
        Command::Check(_) | Command::Sync(_) | Command::Assemble { .. }
    ) {
        skill::warn_if_stale(&repo.root);
    }
    if let Command::Repair { command } = cli.command {
        return repair::run(&repo, &config, command);
    }
    let config_path =
        std::fs::canonicalize(config::resolve_path(&repo.root, cli.config.as_deref()))?;
    let options = |args: SyncArgs, candidate: Option<String>, command: &str| Options {
        context: workflow::Context {
            command: command.into(),
            config_path: config_path.clone(),
            target_expression: args.target.clone(),
            candidate_expression: candidate.clone(),
            fetched: !args.no_fetch,
            checks_enabled: !args.no_checks,
        },
        target: args.target,
        candidate,
        fetch: !args.no_fetch,
        checks: !args.no_checks,
        push: args.push,
        save_plan: args.save_plan,
        report_path: args.report,
    };
    let retirement = match &cli.command {
        Command::Retire {
            name, remove_glue, ..
        } => Some((name.clone(), remove_glue.clone())),
        _ => None,
    };
    let (mut session, command) = match cli.command {
        Command::Init { .. } => {
            init::init(&repo, &config)?;
            return Ok(0);
        }
        Command::Alias | Command::Skill { .. } => unreachable!(),
        Command::Check(args) => {
            anyhow::ensure!(
                args.save_plan.is_none(),
                "check supports --report, not --save-plan; use sync or assemble"
            );
            (
                Session::new(&repo, &config, options(args, None, "check"))?,
                "check",
            )
        }
        Command::Sync(args) => (
            Session::new(&repo, &config, options(args, None, "sync"))?,
            "sync",
        ),
        Command::Assemble { args, candidate } => (
            Session::new(&repo, &config, options(args, candidate, "assemble"))?,
            "assemble",
        ),
        Command::Create {
            name,
            message,
            no_fetch,
        } => {
            let mut session = Session::new(
                &repo,
                &config,
                options(
                    SyncArgs {
                        no_fetch,
                        ..Default::default()
                    },
                    None,
                    "create",
                ),
            )?;
            return session.create(&name, &message);
        }
        Command::Retire {
            no_fetch,
            push,
            save_plan,
            report,
            ..
        } => (
            Session::new(
                &repo,
                &config,
                options(
                    SyncArgs {
                        no_fetch,
                        push,
                        save_plan,
                        report,
                        ..Default::default()
                    },
                    None,
                    "retire",
                ),
            )?,
            "retire",
        ),
        Command::Apply { .. } | Command::Repair { .. } => unreachable!(),
    };
    let result = match command {
        "check" => session.check(),
        "sync" => session.sync(),
        "retire" => {
            let (name, glues) = retirement.as_ref().unwrap();
            session.retire(name, glues)
        }
        _ => session.assemble_command(),
    };
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            session.record_error(&error);
            session.write_artifacts(1)?;
            *reported = true;
            return Err(error);
        }
    };
    session.write_artifacts(code)?;
    *reported = true;
    session.finish();
    Ok(code)
}
