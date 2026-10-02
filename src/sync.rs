//! check, sync, and assemble.
//!
//! A fork is a set of series (bookmarks such as `patch/*` and `tooling/*`), each rooted on
//! upstream, plus glues (`glue/<a>+<b>`) that hold only the resolution between series. The fork
//! branch is a generated merge of upstream, every series, and every glue.
//!
//! `check` replays every stale series onto the upstream target in a throwaway worktree and runs
//! its checks. `sync` rebases them with jj only when every series is clean, then assembles.
//! `assemble` restacks stale glues, builds a fresh fork-branch merge, and moves the branch only
//! after its checks pass.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::checks::{Checker, Outcome, globs};
use crate::config::{Config, Limits};
use crate::repo::Repo;
use crate::{glue, progress, report};

/// Exit codes shared by every command.
pub const EXIT_OK: i32 = 0;
pub const EXIT_CLEAN: i32 = 10;
pub const EXIT_NEEDS_AGENT: i32 = 20;

pub struct Options {
    pub target: Option<String>,
    pub candidate: Option<String>,
    pub fetch: bool,
    pub checks: bool,
    pub push: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    UpToDate,
    Clean,
    Conflict,
    Broken,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::UpToDate => "up-to-date",
            Status::Clean => "clean",
            Status::Conflict => "conflict",
            Status::Broken => "broken",
        }
    }
}

struct SeriesResult {
    status: Status,
    tier: Option<String>,
    detail: Option<String>,
    replay_tree: Option<String>,
}

/// How an assemble attempt ended.
enum Assembled {
    Done,
    /// Stopped with work left for an agent in the local repo (a glue or merge conflict), so a
    /// sync must not undo its rebases.
    NeedsAgent,
    /// Failed without leaving work behind.
    Failed,
}

pub struct Session<'a> {
    repo: &'a Repo,
    config: &'a Config,
    options: Options,
    target: String,
    series: Vec<String>,
    glues: Vec<String>,
    origin_snapshot: Vec<String>,
    results: BTreeMap<String, SeriesResult>,
    checker: Checker<'a>,
    _scratch: tempfile::TempDir,
}

impl<'a> Session<'a> {
    pub fn new(repo: &'a Repo, config: &'a Config, options: Options) -> Result<Session<'a>> {
        if repo.is_shallow()? {
            progress("shallow clone detected; running init");
            crate::init::init(repo, config)?;
        }
        let fork = &config.fork.remote;
        if options.fetch {
            progress(&format!("fetching {fork} and {}", config.upstream.remote));
            repo.jj(&["git", "fetch", "--remote", fork, "--quiet"])?;
            repo.jj(&[
                "git",
                "fetch",
                "--remote",
                &config.upstream.remote,
                "--quiet",
            ])?;
        }
        // Series and glues that others pushed become local bookmarks, so the merge includes them.
        crate::init::track(repo, config);
        let origin_snapshot = fork_refs(repo, config)?;
        let target = repo.rev(options.target.as_deref().unwrap_or(&config.upstream_ref()))?;
        let series = repo.local_bookmarks(&config.fork.series_prefixes)?;
        let glues = repo.local_bookmarks(std::slice::from_ref(&config.fork.glue_prefix))?;
        let scratch = tempfile::Builder::new().prefix("jj-fork.").tempdir()?;
        let log_dir = std::env::temp_dir().join("jj-fork-logs").join(timestamp());
        std::fs::create_dir_all(&log_dir)?;
        let checker = Checker::new(
            repo,
            config,
            target.clone(),
            scratch.path().to_path_buf(),
            log_dir,
        );
        Ok(Session {
            repo,
            config,
            options,
            target,
            series,
            glues,
            origin_snapshot,
            results: BTreeMap::new(),
            checker,
            _scratch: scratch,
        })
    }

    /// Reports every series. Returns EXIT_OK, EXIT_CLEAN, or EXIT_NEEDS_AGENT.
    pub fn check(&mut self) -> Result<i32> {
        anyhow::ensure!(
            !self.series.is_empty(),
            "no series bookmarks matching {}",
            self.config
                .fork
                .series_prefixes
                .iter()
                .map(|p| format!("{p}*"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        report(&format!(
            "target {} {}",
            self.target,
            self.repo.subject(&self.target)?
        ));
        let mut stale = false;
        let mut needs_agent = false;
        for name in self.series.clone() {
            let result = self.replay(&name)?;
            let mut line = format!("{name} {}", result.status.label());
            if let Some(tier) = &result.tier {
                line.push_str(&format!(" [tier={tier}]"));
            }
            if let Some(detail) = &result.detail {
                line.push_str(&format!(" — {detail}"));
            }
            report(&line);
            stale |= result.status != Status::UpToDate;
            needs_agent |= matches!(result.status, Status::Conflict | Status::Broken);
            self.results.insert(name, result);
        }
        Ok(if needs_agent {
            EXIT_NEEDS_AGENT
        } else if stale {
            EXIT_CLEAN
        } else {
            EXIT_OK
        })
    }

    /// Checks, then rebases every clean stale series and assembles. Mutates nothing unless every
    /// stale series is clean, and undoes everything if the fork-branch checks fail.
    pub fn sync(&mut self) -> Result<i32> {
        let code = self.check()?;
        if code == EXIT_NEEDS_AGENT {
            report("not applying: some series need an agent");
            return Ok(code);
        }
        let op = self.repo.op_id()?;
        if code == EXIT_CLEAN
            && let Err(err) = self.rebase_clean_series()
        {
            self.repo.op_restore(&op)?;
            return Err(err);
        }
        match self.assemble()? {
            Assembled::Done if self.options.push => self.push(),
            Assembled::Done => Ok(EXIT_OK),
            Assembled::NeedsAgent => Ok(EXIT_NEEDS_AGENT),
            Assembled::Failed => {
                report(&format!(
                    "restoring operation {}; no bookmarks moved",
                    short(&op)
                ));
                self.repo.op_restore(&op)?;
                Ok(EXIT_NEEDS_AGENT)
            }
        }
    }

    /// Assembles and optionally pushes, as a standalone command.
    pub fn assemble_command(&mut self) -> Result<i32> {
        match self.assemble()? {
            Assembled::Done if self.options.push => self.push(),
            Assembled::Done => Ok(EXIT_OK),
            Assembled::NeedsAgent | Assembled::Failed => Ok(EXIT_NEEDS_AGENT),
        }
    }

    fn replay(&mut self, name: &str) -> Result<SeriesResult> {
        let repo = self.repo;
        let tip = repo.rev(&Repo::bookmark_revset(name))?;
        if repo.is_ancestor(&self.target, &tip)? {
            return Ok(SeriesResult {
                status: Status::UpToDate,
                tier: None,
                detail: None,
                replay_tree: None,
            });
        }
        let base = repo.merge_base(&self.target, &tip)?;
        if !repo
            .git(&["rev-list", "--merges", &format!("{base}..{tip}")])?
            .is_empty()
        {
            return Ok(SeriesResult {
                status: Status::Conflict,
                tier: Some("high".into()),
                detail: Some("series contains merge commits".into()),
                replay_tree: None,
            });
        }
        let commits: Vec<String> = repo
            .git(&["rev-list", "--reverse", &format!("{base}..{tip}")])?
            .lines()
            .map(str::to_string)
            .collect();
        let worktree =
            repo.worktree(&self.checker.scratch, &name.replace('/', "_"), &self.target)?;
        let generated = globs(
            &self
                .config
                .generated
                .as_ref()
                .map(|g| g.paths.clone())
                .unwrap_or_default(),
        )?;
        for (i, commit) in commits.iter().enumerate() {
            let picked = worktree.git(&[
                "-c",
                "user.name=jj-fork",
                "-c",
                "user.email=jj-fork@localhost",
                "-c",
                "merge.conflictStyle=merge",
                "cherry-pick",
                "--allow-empty",
                "--empty=keep",
                commit,
            ]);
            if picked.is_ok() {
                continue;
            }
            let files: Vec<String> = worktree
                .git(&["diff", "--name-only", "--diff-filter=U"])?
                .lines()
                .map(str::to_string)
                .collect();
            let mut code_files = 0;
            let (mut hunks, mut lines) = (0, 0);
            for file in &files {
                if generated.is_match(file) {
                    continue;
                }
                code_files += 1;
                let text = std::fs::read_to_string(worktree.path.join(file)).unwrap_or_default();
                let (h, l) = conflict_size(&text);
                hunks += h;
                lines += l;
            }
            let _ = worktree.git(&["cherry-pick", "--abort"]);
            let remaining = commits.len() - i;
            let tiers = &self.config.tiers;
            let tier = conflict_tier(
                &tiers.low_max,
                &tiers.medium_max,
                code_files,
                hunks,
                lines,
                remaining,
            );
            let detail = format!(
                "first conflict at {} \"{}\": {hunks} hunks, {lines} lines in {code_files} non-generated of {} files ({}); {remaining} commits left to replay",
                short(commit),
                repo.subject(commit)?,
                files.len(),
                files.join(","),
            );
            return Ok(SeriesResult {
                status: Status::Conflict,
                tier: Some(tier.into()),
                detail: Some(detail),
                replay_tree: None,
            });
        }
        let tree = worktree.git(&["rev-parse", "HEAD^{tree}"])?;
        if self.options.checks {
            progress(&format!("checking {name} on {}", short(&self.target)));
            let checks = self.config.checks.patch.clone();
            if let Outcome::Fail { check, tier, log } =
                self.checker.run(&worktree.path, &checks, true, name)?
            {
                return Ok(SeriesResult {
                    status: Status::Broken,
                    tier: Some(tier),
                    detail: Some(format!(
                        "replays cleanly but fails {check} checks; log: {}",
                        log.display()
                    )),
                    replay_tree: None,
                });
            }
        }
        Ok(SeriesResult {
            status: Status::Clean,
            tier: None,
            detail: None,
            replay_tree: Some(tree),
        })
    }

    /// Rebases each clean stale series with jj and verifies the result matches the checked replay.
    fn rebase_clean_series(&mut self) -> Result<()> {
        let repo = self.repo;
        for (name, result) in &self.results {
            if result.status != Status::Clean {
                continue;
            }
            let tip = repo.rev(&Repo::bookmark_revset(name))?;
            let base = repo.merge_base(&self.target, &tip)?;
            let new_tip = self.duplicate(
                &format!("{base}..{tip}"),
                &tip,
                std::slice::from_ref(&self.target),
            )?;
            let conflicted = repo.has_conflicts(&format!("{}..{new_tip}", self.target))?;
            if conflicted || Some(repo.tree(&new_tip)?) != result.replay_tree {
                anyhow::bail!("{name}: the jj rebase differs from the checked replay");
            }
            repo.jj(&[
                "bookmark",
                "set",
                name,
                "-r",
                &new_tip,
                "--allow-backwards",
                "--quiet",
            ])?;
            report(&format!("{name} -> {}", short(&new_tip)));
        }
        if let Some(mirror) = &self.config.fork.mirror_branch
            && let Ok(current) = repo.rev(&Repo::bookmark_revset(mirror))
            && current != self.target
            && repo.is_ancestor(&current, &self.target)?
        {
            repo.jj(&["bookmark", "set", mirror, "-r", &self.target, "--quiet"])?;
            report(&format!("{mirror} -> {}", short(&self.target)));
        }
        Ok(())
    }

    /// Copies `revset` onto `onto` with `jj duplicate` and returns the copy of `tip`.
    fn duplicate(&self, revset: &str, tip: &str, onto: &[String]) -> Result<String> {
        let mut args = vec![
            "duplicate".to_string(),
            "--color=never".into(),
            revset.to_string(),
        ];
        for commit in onto {
            args.extend(["-o".to_string(), commit.clone()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = crate::run::output_all(&self.repo.root, "jj", &args)?;
        let new_short = out
            .lines()
            .find_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                (parts.len() >= 5 && parts[0] == "Duplicated" && tip.starts_with(parts[1]))
                    .then(|| parts[4].to_string())
            })
            .with_context(|| format!("could not find the copy of {}:\n{out}", short(tip)))?;
        self.repo.rev(&new_short)
    }

    /// The commits a glue must merge: the tips of the series it names, reduced to heads together
    /// with every glue over a proper subset of those series.
    fn glue_parents(&self, name: &str) -> Result<std::result::Result<Vec<String>, String>> {
        let fork = &self.config.fork;
        let series =
            match glue::series_of(name, &fork.glue_prefix, &fork.series_prefixes, &self.series) {
                Ok(series) => series,
                Err(reason) => return Ok(Err(reason)),
            };
        let mut revsets: Vec<String> = series.iter().map(|s| Repo::bookmark_revset(s)).collect();
        revsets.extend(
            glue::inner_glues(name, &fork.glue_prefix, &self.glues)
                .into_iter()
                .map(|g| Repo::bookmark_revset(g)),
        );
        Ok(Ok(self
            .repo
            .revs(&format!("heads({})", revsets.join(" | ")))?))
    }

    /// Moves every glue whose parents are not the current tips of its series onto those tips.
    /// jj duplicate carries the resolution. Smaller glues go first, so an outer glue lands on its
    /// inner glue's new commit. Returns false if a glue is invalid or now conflicts; a conflicted
    /// glue keeps its new position so an agent can resolve inside it.
    fn restack_glues(&self) -> Result<bool> {
        let prefix = &self.config.fork.glue_prefix;
        let mut ordered = self.glues.clone();
        ordered.sort_by_key(|g| (glue::names(g, prefix).len(), g.clone()));
        let mut ok = true;
        for name in &ordered {
            let expected = match self.glue_parents(name)? {
                Ok(parents) => parents,
                Err(reason) => {
                    report(&format!("glue invalid — {reason}"));
                    ok = false;
                    continue;
                }
            };
            let revset = Repo::bookmark_revset(name);
            if self.repo.revs(&format!("parents({revset})"))? != expected {
                let tip = self.repo.rev(&revset)?;
                let new = self.duplicate(&revset, &tip, &expected)?;
                self.repo.jj(&[
                    "bookmark",
                    "set",
                    name,
                    "-r",
                    &new,
                    "--allow-backwards",
                    "--quiet",
                ])?;
                report(&format!("{name} -> {} (restacked)", short(&new)));
            }
            let tip = self.repo.rev(&revset)?;
            if self.repo.has_conflicts(&tip)? {
                let files = self
                    .repo
                    .jj(&["resolve", "--list", "-r", &tip])
                    .unwrap_or_default();
                let tier = if files.lines().count() > 2 {
                    "high"
                } else {
                    "medium"
                };
                report(&format!(
                    "glue conflict [tier={tier}] — {name} at {} has conflicts:",
                    short(&tip)
                ));
                for line in files.lines() {
                    report(&format!("  {line}"));
                }
                report(&format!(
                    "resolve them in the glue (jj new {}, edit, jj squash), then run: jj fork assemble --push",
                    short(&tip)
                ));
                ok = false;
            }
        }
        Ok(ok)
    }

    /// Series are independent, so one inside another is a stale or renamed bookmark.
    fn nested_series(&self) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for inner in &self.series {
            let inner_tip = self.repo.rev(&Repo::bookmark_revset(inner))?;
            for outer in &self.series {
                if inner != outer
                    && self
                        .repo
                        .is_ancestor(&inner_tip, &self.repo.rev(&Repo::bookmark_revset(outer))?)?
                {
                    found.push(format!("{inner} is contained in {outer}"));
                }
            }
        }
        Ok(found)
    }

    /// Each pair of merge parents that conflicts on its own, with the glue that would resolve it.
    fn conflicting_pairs(&self, parents: &[String]) -> Result<Vec<String>> {
        let fork = &self.config.fork;
        let mut prefixes = fork.series_prefixes.clone();
        prefixes.push(fork.glue_prefix.clone());
        let names: Vec<String> = parents
            .iter()
            .map(|c| {
                Ok(self
                    .repo
                    .bookmark_on(c, &prefixes)?
                    .unwrap_or_else(|| short(c).to_string()))
            })
            .collect::<Result<_>>()?;
        let mut pairs = Vec::new();
        for i in 0..parents.len() {
            for j in i + 1..parents.len() {
                let clean = crate::run::succeeds(
                    &self.repo.root,
                    "git",
                    &[
                        "merge-tree",
                        "--write-tree",
                        "--name-only",
                        "--no-messages",
                        &parents[i],
                        &parents[j],
                    ],
                )?;
                if !clean {
                    // Order each pair by name so reports do not depend on commit ids.
                    let (a, b) = if names[i] <= names[j] {
                        (&names[i], &names[j])
                    } else {
                        (&names[j], &names[i])
                    };
                    let glue = glue::suggested(a, b, &fork.glue_prefix, &fork.series_prefixes);
                    pairs.push(format!(
                        "{a} + {b}: add {glue} with: jj new '{}' '{}'",
                        Repo::bookmark_revset(a),
                        Repo::bookmark_revset(b)
                    ));
                }
            }
        }
        Ok(pairs)
    }

    /// Restacks glues, then builds, checks, and moves the fork branch.
    fn assemble(&mut self) -> Result<Assembled> {
        let repo = self.repo;
        let branch = self.config.fork.branch.clone();
        let nested = self.nested_series()?;
        if !nested.is_empty() {
            report(&format!(
                "a series contains another series; {branch} not moved:"
            ));
            for line in &nested {
                report(&format!("  {line}"));
            }
            report(
                "if the inner one is obsolete (renamed or merged), delete it: jj bookmark delete <name>",
            );
            return Ok(Assembled::Failed);
        }
        if !self.restack_glues()? {
            return Ok(Assembled::NeedsAgent);
        }
        let mut members: Vec<String> = vec![self.target.clone()];
        members.extend(
            self.series
                .iter()
                .chain(&self.glues)
                .map(|b| Repo::bookmark_revset(b)),
        );
        let parents = repo.revs(&format!("heads({})", members.join(" | ")))?;
        let current = repo
            .revs(&format!("parents({})", Repo::bookmark_revset(&branch)))
            .unwrap_or_default();
        let op = repo.op_id()?;
        let merge = match &self.options.candidate {
            Some(candidate) => {
                let merge = repo.rev(candidate)?;
                if repo.revs(&format!("parents({merge})"))? != parents {
                    report(&format!(
                        "candidate {} does not merge exactly the upstream target, every series, and every glue",
                        short(&merge)
                    ));
                    return Ok(Assembled::Failed);
                }
                merge
            }
            None if parents == current => {
                let local = repo.rev(&Repo::bookmark_revset(&branch))?;
                let remote = repo
                    .rev(&format!("{branch}@{}", self.config.fork.remote))
                    .ok();
                // A merge built locally but never pushed has not been checked yet.
                if !self.options.checks || remote.as_deref() == Some(local.as_str()) {
                    report(&format!(
                        "{branch} already merges upstream and every series"
                    ));
                    return Ok(Assembled::Done);
                }
                progress(&format!(
                    "{branch} {} merges every series but is not on the remote yet; checking it",
                    short(&local)
                ));
                local
            }
            None => {
                let mut args = vec!["new", "--quiet"];
                args.extend(parents.iter().map(String::as_str));
                args.extend(["-m", &self.config.fork.merge_message]);
                repo.jj(&args)?;
                repo.rev("@")?
            }
        };
        if repo.has_conflicts(&merge)? {
            let files = repo
                .jj(&["resolve", "--list", "-r", &merge])
                .unwrap_or_default();
            let tier = if files.lines().count() > 2 {
                "high"
            } else {
                "medium"
            };
            report(&format!(
                "{branch} conflict [tier={tier}] — merge {} has conflicts between series:",
                short(&merge)
            ));
            for line in files.lines() {
                report(&format!("  {line}"));
            }
            let pairs = self.conflicting_pairs(&parents)?;
            if pairs.is_empty() {
                report(&format!(
                    "no single pair conflicts; resolve the merge in that commit, then run: jj fork assemble --candidate {} --push",
                    short(&merge)
                ));
            } else {
                for pair in &pairs {
                    report(&format!("  conflicting pair: {pair}"));
                }
                report(
                    "add the first pair's glue: run its jj new command, resolve the conflicts, and create the named",
                );
                report(
                    "glue bookmark on the result. Then rerun: jj fork assemble --push. Later pairs often resolve once it exists.",
                );
                if self.options.candidate.is_none() {
                    repo.op_restore(&op)?;
                }
            }
            return Ok(Assembled::NeedsAgent);
        }
        if self.options.checks {
            progress(&format!("checking {branch} candidate {}", short(&merge)));
            let worktree = repo.worktree(&self.checker.scratch, "fork-candidate", &merge)?;
            let checks = self.config.checks.fork.clone();
            if let Outcome::Fail { check, log, .. } =
                self.checker
                    .run(&worktree.path, &checks, false, "fork-branch")?
            {
                report(&format!(
                    "{branch} candidate {} fails {check} checks; {branch} not moved; log: {}",
                    short(&merge),
                    log.display()
                ));
                if self.options.candidate.is_none() {
                    repo.op_restore(&op)?;
                }
                return Ok(Assembled::Failed);
            }
        }
        repo.jj(&[
            "bookmark",
            "set",
            &branch,
            "-r",
            &merge,
            "--allow-backwards",
            "--quiet",
        ])?;
        repo.jj(&["new", "--quiet", &Repo::bookmark_revset(&branch)])?;
        report(&format!("{branch} -> {}", short(&merge)));
        Ok(Assembled::Done)
    }

    /// Publishes the mirror branch, every series and glue, and the fork branch. Another run may
    /// have pushed while this one was checking, so it fetches again first and refuses to push
    /// if any of those bookmarks changed on the remote: pushing would drop that work.
    fn push(&self) -> Result<i32> {
        let remote = &self.config.fork.remote;
        self.repo
            .jj(&["git", "fetch", "--remote", remote, "--quiet"])?;
        let now = fork_refs(self.repo, self.config)?;
        let changed: Vec<String> = changed_names(&self.origin_snapshot, &now);
        if !changed.is_empty() {
            report(&format!(
                "{remote} changed while jj-fork ran; nothing pushed. Changed on {remote}:"
            ));
            for name in &changed {
                report(&format!("  {name}"));
            }
            report("rerun: jj fork assemble --push");
            return Ok(EXIT_NEEDS_AGENT);
        }
        let mut bookmarks = vec![self.config.fork.branch.clone()];
        bookmarks.extend(self.series.iter().chain(&self.glues).cloned());
        if let Some(mirror) = &self.config.fork.mirror_branch {
            let fast_forward = match (
                self.repo.rev(&format!("{mirror}@{remote}")),
                self.repo.rev(&Repo::bookmark_revset(mirror)),
            ) {
                (Ok(theirs), Ok(ours)) => self.repo.is_ancestor(&theirs, &ours)?,
                _ => false,
            };
            if fast_forward {
                bookmarks.push(mirror.clone());
            }
        }
        let mut args = vec![
            "git".to_string(),
            "push".into(),
            "--remote".into(),
            remote.clone(),
        ];
        for b in &bookmarks {
            args.extend(["-b".to_string(), b.clone()]);
        }
        progress(&format!("pushing {}", bookmarks.join(" ")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::run::run(&self.repo.root, "jj", &args)?;
        Ok(EXIT_OK)
    }

    pub fn finish(&self) {
        for failure in &self.checker.upstream_failures {
            report(&format!("note: also fails on upstream, ignored: {failure}"));
        }
    }
}

/// The fork's branch, series, and glue refs on its remote, as `name commit` lines.
fn fork_refs(repo: &Repo, config: &Config) -> Result<Vec<String>> {
    let fork = &config.fork;
    Ok(repo
        .remote_refs(&fork.remote)?
        .into_iter()
        .filter(|line| {
            let name = line.split(' ').next().unwrap_or_default();
            name == fork.branch
                || name.starts_with(&fork.glue_prefix)
                || fork
                    .series_prefixes
                    .iter()
                    .any(|p| name.starts_with(p.as_str()))
        })
        .collect())
}

/// Names whose commit differs between two `name commit` snapshots.
fn changed_names(before: &[String], after: &[String]) -> Vec<String> {
    let before: std::collections::BTreeSet<&String> = before.iter().collect();
    let after: std::collections::BTreeSet<&String> = after.iter().collect();
    let mut names: Vec<String> = before
        .symmetric_difference(&after)
        .map(|line| line.split(' ').next().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Counts conflict hunks and conflicted lines in a file with Git conflict markers.
pub fn conflict_size(text: &str) -> (usize, usize) {
    let (mut hunks, mut lines, mut inside) = (0, 0, false);
    for line in text.lines() {
        if line.starts_with("<<<<<<< ") {
            inside = true;
            hunks += 1;
        } else if line.starts_with(">>>>>>> ") {
            inside = false;
        } else if inside && line != "=======" && !line.starts_with("||||||| ") {
            lines += 1;
        }
    }
    (hunks, lines)
}

/// Rates a conflict. Only generated files conflicting is always low.
pub fn conflict_tier(
    low: &Limits,
    medium: &Limits,
    files: usize,
    hunks: usize,
    lines: usize,
    commits: usize,
) -> &'static str {
    let within = |l: &Limits| {
        files <= l.files && hunks <= l.hunks && lines <= l.lines && commits <= l.commits
    };
    if files == 0 || within(low) {
        "low"
    } else if within(medium) {
        "medium"
    } else {
        "high"
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: Limits = Limits {
        files: 2,
        hunks: 3,
        lines: 60,
        commits: 3,
    };
    const MEDIUM: Limits = Limits {
        files: 12,
        hunks: 20,
        lines: 400,
        commits: 10,
    };

    #[test]
    fn tiers_follow_the_documented_thresholds() {
        assert_eq!(
            conflict_tier(&LOW, &MEDIUM, 0, 9, 900, 30),
            "low",
            "generated-only conflicts are low"
        );
        assert_eq!(conflict_tier(&LOW, &MEDIUM, 2, 2, 41, 1), "low");
        assert_eq!(
            conflict_tier(&LOW, &MEDIUM, 1, 1, 9, 4),
            "medium",
            "one commit too many for low"
        );
        assert_eq!(conflict_tier(&LOW, &MEDIUM, 11, 17, 229, 1), "medium");
        assert_eq!(
            conflict_tier(&LOW, &MEDIUM, 1, 1, 2, 19),
            "high",
            "a long series is high"
        );
        assert_eq!(conflict_tier(&LOW, &MEDIUM, 13, 1, 1, 1), "high");
    }

    #[test]
    fn conflict_size_counts_both_sides_and_ignores_markers() {
        let text = "a\n<<<<<<< HEAD\nours1\nours2\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> abc\nb\n<<<<<<< HEAD\nx\n=======\n>>>>>>> def\n";
        assert_eq!(conflict_size(text), (2, 5));
    }

    #[test]
    fn changed_names_reports_moved_added_and_deleted_refs() {
        let before = vec![
            "fork/main 1".to_string(),
            "patch/a 2".into(),
            "patch/b 3".into(),
        ];
        let after = vec![
            "fork/main 1".to_string(),
            "patch/a 9".into(),
            "glue/a+c 4".into(),
        ];
        assert_eq!(
            changed_names(&before, &after),
            vec!["glue/a+c", "patch/a", "patch/b"]
        );
    }
}
