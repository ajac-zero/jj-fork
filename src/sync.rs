//! check, sync, and assemble.
//!
//! A fork is a set of series (bookmarks such as `patch/*` and `tooling/*`), each rooted on
//! upstream, plus glues (`glue/<a>+<b>`) that hold only the resolution between series. The fork
//! branch is a generated merge of upstream, every series, and every glue.
//!
//! Preparation (snapshot, fetch, reconcile, track) runs through the jj CLI and may publish its
//! own operations. Then the plan freezes: the operation, the working copy, the upstream target,
//! membership, bookmarks, and the fork remote's refs. From there everything happens in one
//! unpublished jj transaction: `check` copies every stale series onto the target and checks the
//! exact copies in throwaway Git worktrees; `sync` keeps those very copies, then assembles;
//! `assemble` restacks glues and builds the fork-branch merge, checked the same way. The
//! transaction is published only if the repository is still as frozen, and only the outcome a
//! command intends: a refusal publishes nothing it planned.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use globset::GlobSet;
use jj_lib::backend::CommitId;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::transaction::Transaction;

use crate::checks::{Checker, Outcome, globs};
use crate::config::{Config, Limits};
use crate::native::{self, Native, Publication};
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
    /// The checked copy of a clean series' tip, which sync publishes as is.
    candidate: Option<CommitId>,
}

impl SeriesResult {
    fn new(status: Status, tier: Option<&str>, detail: Option<String>) -> SeriesResult {
        SeriesResult {
            status,
            tier: tier.map(str::to_string),
            detail,
            candidate: None,
        }
    }
}

/// What an assemble attempt leaves to publish.
enum Assembled {
    /// Publish the planned changes; push if asked.
    Done,
    /// Publish the validated series and glue state (and a repair head, if any) for an agent to
    /// continue from, but never push it.
    Repair,
    /// Publish nothing that was planned.
    Refused,
}

pub struct Session<'a> {
    repo: &'a Repo,
    config: &'a Config,
    options: Options,
    jj: Native,
    /// The unpublished maintenance transaction, started at the frozen operation.
    tx: Transaction,
    target: CommitId,
    candidate: Option<CommitId>,
    series: Vec<String>,
    glues: Vec<String>,
    /// The fork remote's relevant refs at the freeze, as `name commits` lines.
    remote_snapshot: Vec<String>,
    generated: GlobSet,
    results: BTreeMap<String, SeriesResult>,
    checker: Checker<'a>,
    _scratch: tempfile::TempDir,
}

impl<'a> Session<'a> {
    /// Prepares the clone with the jj CLI, then freezes the plan.
    pub fn new(repo: &'a Repo, config: &'a Config, options: Options) -> Result<Session<'a>> {
        native::check_cli(&repo.root)?;
        if repo.is_shallow()? {
            progress("shallow clone detected; running init");
            crate::init::init(repo, config)?;
        }
        repo.jj(&["util", "snapshot"])?;
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
        // Stale local bookmarks must not win over the remote, so settle them before tracking.
        crate::reconcile::reconcile(repo, config)?;
        // Series and glues that others pushed become local bookmarks, so the merge includes them.
        crate::init::track(repo, config);
        // Revisions are resolved once, here; nothing is re-resolved after checks begin.
        let target = repo.rev(options.target.as_deref().unwrap_or(&config.upstream_ref()))?;
        let candidate = options
            .candidate
            .as_deref()
            .map(|c| repo.rev(c))
            .transpose()?;

        let jj = Native::load(&repo.root)?;
        let target = native::parse_id(&target)?;
        let candidate = candidate.as_deref().map(native::parse_id).transpose()?;
        for id in std::iter::once(&target).chain(&candidate) {
            native::commit(jj.repo.as_ref(), id)
                .with_context(|| format!("{} changed while jj-fork prepared", short(&id.hex())))?;
        }
        let series = native::bookmarks_with(jj.repo.as_ref(), &config.fork.series_prefixes);
        let glues = native::bookmarks_with(
            jj.repo.as_ref(),
            std::slice::from_ref(&config.fork.glue_prefix),
        );
        let remote_snapshot = fork_refs(jj.repo.as_ref(), config);
        let generated = globs(
            &config
                .generated
                .as_ref()
                .map(|g| g.paths.clone())
                .unwrap_or_default(),
        )?;
        let tx = jj.start();
        let scratch = tempfile::Builder::new().prefix("jj-fork.").tempdir()?;
        let log_dir = std::env::temp_dir().join("jj-fork-logs").join(timestamp());
        std::fs::create_dir_all(&log_dir)?;
        let checker = Checker::new(
            repo,
            config,
            target.hex(),
            scratch.path().to_path_buf(),
            log_dir,
        );
        Ok(Session {
            repo,
            config,
            options,
            jj,
            tx,
            target,
            candidate,
            series,
            glues,
            remote_snapshot,
            generated,
            results: BTreeMap::new(),
            checker,
            _scratch: scratch,
        })
    }

    /// Reports every series. Returns EXIT_OK, EXIT_CLEAN, or EXIT_NEEDS_AGENT. Publishes nothing.
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
        let target = native::commit(self.tx.repo(), &self.target)?;
        report(&format!(
            "target {} {}",
            self.target.hex(),
            native::subject(&target)
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

    /// Checks, then moves every clean stale series to its checked copy and assembles. Publishes
    /// nothing unless every stale series is clean, and nothing it planned if assembling refuses.
    pub fn sync(&mut self) -> Result<i32> {
        let code = self.check()?;
        if code == EXIT_NEEDS_AGENT {
            report("not applying: some series need an agent");
            return Ok(code);
        }
        if code == EXIT_CLEAN {
            self.apply_clean_series()?;
        }
        let assembled = self.assemble()?;
        self.conclude(assembled, "sync")
    }

    /// Assembles and optionally pushes, as a standalone command.
    pub fn assemble_command(&mut self) -> Result<i32> {
        let assembled = self.assemble()?;
        self.conclude(assembled, "assemble")
    }

    /// Publishes what `assembled` allows, then pushes if asked and allowed.
    fn conclude(&mut self, assembled: Assembled, command: &str) -> Result<i32> {
        let code = match assembled {
            Assembled::Refused => {
                report("no planned maintenance published; no bookmarks moved");
                return Ok(EXIT_NEEDS_AGENT);
            }
            Assembled::Repair => EXIT_NEEDS_AGENT,
            Assembled::Done => EXIT_OK,
        };
        let tx = std::mem::replace(&mut self.tx, self.jj.start());
        let changed = tx.repo().has_changes();
        match self.jj.publish(tx, &format!("jj-fork {command}"))? {
            Publication::Done(op) if changed => {
                progress(&format!("published operation {}", short(&op.hex())));
            }
            Publication::Done(_) => {}
            Publication::Stale(reason) => {
                report(&format!(
                    "{reason} while jj-fork ran; no planned maintenance published. Rerun."
                ));
                return Ok(EXIT_NEEDS_AGENT);
            }
        }
        if code == EXIT_OK && self.options.push {
            return self.push();
        }
        Ok(code)
    }

    fn tip(&self, name: &str) -> Result<CommitId> {
        native::bookmark(self.tx.repo(), name)?.with_context(|| format!("bookmark {name} is gone"))
    }

    fn short_of(&self, id: &CommitId) -> String {
        short(&id.hex()).to_string()
    }

    /// Copies a stale series onto the target as new changes and checks the copy. Every copied
    /// commit must be conflict-free; the configured checks run on the copied tip.
    fn replay(&mut self, name: &str) -> Result<SeriesResult> {
        let tip = self.tip(name)?;
        if native::is_ancestor(self.tx.repo(), &self.target, &tip)? {
            return Ok(SeriesResult::new(Status::UpToDate, None, None));
        }
        let commits = native::range(self.tx.repo(), &self.target, &tip)?;
        for id in &commits {
            if native::commit(self.tx.repo(), id)?.parent_ids().len() > 1 {
                return Ok(SeriesResult::new(
                    Status::Conflict,
                    Some("high"),
                    Some("series contains merge commits".into()),
                ));
            }
        }
        let copies = native::duplicate(&mut self.tx, &commits, std::slice::from_ref(&self.target))?;
        // Parents first, so the first conflicted copy is where the replay first conflicts.
        for (i, id) in commits.iter().rev().enumerate() {
            let copy = &copies[id];
            if !copy.has_conflict() {
                continue;
            }
            let size = native::conflict_size(self.tx.repo(), &copy.tree(), &self.generated)?;
            let remaining = commits.len() - i;
            let tiers = &self.config.tiers;
            let tier = conflict_tier(
                &tiers.low_max,
                &tiers.medium_max,
                size.code_files,
                size.hunks,
                size.lines,
                remaining,
            );
            let original = native::commit(self.tx.repo(), id)?;
            let detail = format!(
                "first conflict at {} \"{}\": {} hunks, {} lines in {} non-generated of {} files ({}); {remaining} commits left to replay",
                self.short_of(id),
                native::subject(&original),
                size.hunks,
                size.lines,
                size.code_files,
                size.files.len(),
                size.files.join(","),
            );
            return Ok(SeriesResult::new(
                Status::Conflict,
                Some(tier),
                Some(detail),
            ));
        }
        let candidate = copies[&tip].id().clone();
        if self.options.checks {
            progress(&format!(
                "checking {name} on {}",
                self.short_of(&self.target)
            ));
            let worktree = self.repo.worktree(
                &self.checker.scratch,
                &name.replace('/', "_"),
                &candidate.hex(),
            )?;
            let checks = self.config.checks.patch.clone();
            if let Outcome::Fail { check, tier, log } =
                self.checker.run(&worktree.path, &checks, true, name)?
            {
                return Ok(SeriesResult::new(
                    Status::Broken,
                    Some(&tier),
                    Some(format!(
                        "replays cleanly but fails {check} checks; log: {}",
                        log.display()
                    )),
                ));
            }
        }
        Ok(SeriesResult {
            candidate: Some(candidate),
            ..SeriesResult::new(Status::Clean, None, None)
        })
    }

    /// Moves each clean stale series to its checked copy, and fast-forwards the mirror.
    fn apply_clean_series(&mut self) -> Result<()> {
        let moves: Vec<(String, CommitId)> = self
            .results
            .iter()
            .filter_map(|(name, r)| r.candidate.clone().map(|c| (name.clone(), c)))
            .collect();
        for (name, candidate) in moves {
            native::set_bookmark(&mut self.tx, &name, &candidate);
            report(&format!("{name} -> {}", self.short_of(&candidate)));
        }
        if let Some(mirror) = self.config.fork.mirror_branch.clone()
            && let Some(current) = native::bookmark(self.tx.repo(), &mirror)?
            && current != self.target
            && native::is_ancestor(self.tx.repo(), &current, &self.target)?
        {
            let target = self.target.clone();
            native::set_bookmark(&mut self.tx, &mirror, &target);
            report(&format!("{mirror} -> {}", self.short_of(&target)));
        }
        Ok(())
    }

    /// The commits a glue must merge: the tips of the series it names, reduced to heads together
    /// with every glue over a proper subset of those series.
    fn glue_parents(&self, name: &str) -> Result<std::result::Result<Vec<CommitId>, String>> {
        let fork = &self.config.fork;
        let series =
            match glue::series_of(name, &fork.glue_prefix, &fork.series_prefixes, &self.series) {
                Ok(series) => series,
                Err(reason) => return Ok(Err(reason)),
            };
        let mut tips: Vec<CommitId> = series.iter().map(|s| self.tip(s)).collect::<Result<_>>()?;
        for inner in glue::inner_glues(name, &fork.glue_prefix, &self.glues) {
            tips.push(self.tip(inner)?);
        }
        Ok(Ok(native::heads(self.tx.repo(), &tips)?))
    }

    /// Moves every glue whose parents are not the current tips of its series onto those tips,
    /// copying it so the resolution carries over. Smaller glues go first, so an outer glue lands
    /// on its inner glue's copy. Returns `Repair` when a glue now conflicts (it keeps its new
    /// position so an agent can resolve inside it) and `Refused` when a glue is invalid.
    fn restack_glues(&mut self) -> Result<Assembled> {
        let prefix = self.config.fork.glue_prefix.clone();
        let mut ordered = self.glues.clone();
        ordered.sort_by_key(|g| (glue::names(g, &prefix).len(), g.clone()));
        let mut invalid = false;
        let mut conflicted = false;
        for name in &ordered {
            let expected = match self.glue_parents(name)? {
                Ok(parents) => parents,
                Err(reason) => {
                    report(&format!("glue invalid — {reason}"));
                    invalid = true;
                    continue;
                }
            };
            let tip = self.tip(name)?;
            let mut parents = native::commit(self.tx.repo(), &tip)?.parent_ids().to_vec();
            parents.sort();
            if parents != expected {
                let copies =
                    native::duplicate(&mut self.tx, std::slice::from_ref(&tip), &expected)?;
                let new = copies[&tip].id().clone();
                native::set_bookmark(&mut self.tx, name, &new);
                report(&format!("{name} -> {} (restacked)", self.short_of(&new)));
            }
            let tip = self.tip(name)?;
            let tree = native::commit(self.tx.repo(), &tip)?.tree();
            if tree.has_conflict() {
                let files = native::conflicted_paths(&tree)?;
                report(&format!(
                    "glue conflict [tier={}] — {name} at {} has conflicts:",
                    conflict_files_tier(files.len()),
                    self.short_of(&tip)
                ));
                for file in &files {
                    report(&format!("  {}    {}-sided conflict", file.path, file.sides));
                }
                report(&format!(
                    "resolve them in the glue (jj new {}, edit, jj squash), then run: jj fork assemble --push",
                    self.short_of(&tip)
                ));
                conflicted = true;
            }
        }
        Ok(if invalid {
            Assembled::Refused
        } else if conflicted {
            Assembled::Repair
        } else {
            Assembled::Done
        })
    }

    /// Series are independent, so one inside another is a stale or renamed bookmark.
    fn nested_series(&self) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for inner in &self.series {
            let inner_tip = self.tip(inner)?;
            for outer in &self.series {
                if inner != outer
                    && native::is_ancestor(self.tx.repo(), &inner_tip, &self.tip(outer)?)?
                {
                    found.push(format!("{inner} is contained in {outer}"));
                }
            }
        }
        Ok(found)
    }

    /// Each pair of merge parents that conflicts on its own under jj's merge, with the glue that
    /// would resolve it. Pairs are ordered by name, within and across pairs, so reports do not
    /// depend on commit ids and the first pair an agent is told to glue is the same on every run.
    fn conflicting_pairs(&self, parents: &[CommitId]) -> Result<Vec<String>> {
        let fork = &self.config.fork;
        let mut prefixes = fork.series_prefixes.clone();
        prefixes.push(fork.glue_prefix.clone());
        let view = self.tx.repo().view();
        let mut named: Vec<(String, &CommitId)> = parents
            .iter()
            .map(|c| {
                let name = view
                    .local_bookmarks_for_commit(c)
                    .map(|(name, _)| name.as_str())
                    .find(|name| prefixes.iter().any(|p| name.starts_with(p.as_str())))
                    .map(str::to_string)
                    .unwrap_or_else(|| self.short_of(c));
                (name, c)
            })
            .collect();
        named.sort();
        let mut pairs = Vec::new();
        for (i, (a, a_commit)) in named.iter().enumerate() {
            for (b, b_commit) in &named[i + 1..] {
                let tree = native::merged_tree(
                    self.tx.repo(),
                    &[(*a_commit).clone(), (*b_commit).clone()],
                )?;
                if tree.has_conflict() {
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

    /// Reports a conflicted fork merge. `existing` is the commit when one exists (a candidate or
    /// the local fork branch); otherwise the merge was only computed. When a pair explains the
    /// conflict the anonymous merge is never written; otherwise it is written as a repair head
    /// and checked out, so it can be resolved and passed to --candidate.
    fn report_conflicted_merge(
        &mut self,
        existing: Option<&CommitId>,
        tree: &MergedTree,
        parents: &[CommitId],
    ) -> Result<Assembled> {
        let branch = self.config.fork.branch.clone();
        let files = native::conflicted_paths(tree)?;
        let what = match existing {
            Some(id) => format!("merge {}", self.short_of(id)),
            None => format!("merge of {} parents", parents.len()),
        };
        report(&format!(
            "{branch} conflict [tier={}] — {what} has conflicts between series:",
            conflict_files_tier(files.len())
        ));
        for file in &files {
            report(&format!("  {}    {}-sided conflict", file.path, file.sides));
        }
        let pairs = self.conflicting_pairs(parents)?;
        if pairs.is_empty() {
            let repair = match existing {
                Some(id) => native::commit(self.tx.repo(), id)?,
                None => {
                    let commit = native::block_on(
                        self.tx
                            .repo_mut()
                            .new_commit(parents.to_vec(), tree.clone())
                            .set_description(&self.config.fork.merge_message)
                            .write(),
                    )?;
                    self.jj.set_wc_commit(&mut self.tx, commit.id())?;
                    commit
                }
            };
            report(&format!(
                "no single pair conflicts; resolve the merge in that commit, then run: jj fork assemble --candidate {} --push",
                native::short_change(&repair)
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
        }
        Ok(Assembled::Repair)
    }

    /// Restacks glues, then builds, checks, and moves the fork branch, all in the transaction.
    fn assemble(&mut self) -> Result<Assembled> {
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
            return Ok(Assembled::Refused);
        }
        match self.restack_glues()? {
            Assembled::Done => {}
            stopped => return Ok(stopped),
        }
        let mut members = vec![self.target.clone()];
        for name in self.series.iter().chain(&self.glues) {
            members.push(self.tip(name)?);
        }
        let parents = native::heads(self.tx.repo(), &members)?;
        let local = native::bookmark(self.tx.repo(), &branch)?;
        let current = match &local {
            Some(id) => {
                let mut ids = native::commit(self.tx.repo(), id)?.parent_ids().to_vec();
                ids.sort();
                ids
            }
            None => Vec::new(),
        };
        let merge = match (self.candidate.clone(), local) {
            (Some(candidate), _) => {
                let mut ids = native::commit(self.tx.repo(), &candidate)?
                    .parent_ids()
                    .to_vec();
                ids.sort();
                if ids != parents {
                    report(&format!(
                        "candidate {} does not merge exactly the upstream target, every series, and every glue",
                        self.short_of(&candidate)
                    ));
                    return Ok(Assembled::Refused);
                }
                candidate
            }
            (None, Some(local)) if parents == current => {
                let remote = native::remote_bookmarks(self.tx.repo(), &self.config.fork.remote)
                    .into_iter()
                    .find(|r| r.name == branch)
                    .map(|r| r.commits);
                // A merge built locally but never pushed has not been checked yet.
                if !self.options.checks || remote.as_deref() == Some(std::slice::from_ref(&local)) {
                    report(&format!(
                        "{branch} already merges upstream and every series"
                    ));
                    return Ok(Assembled::Done);
                }
                progress(&format!(
                    "{branch} {} merges every series but is not on the remote yet; checking it",
                    self.short_of(&local)
                ));
                local
            }
            (None, _) => {
                let tree = native::merged_tree(self.tx.repo(), &parents)?;
                if tree.has_conflict() {
                    return self.report_conflicted_merge(None, &tree, &parents);
                }
                let commit = native::block_on(
                    self.tx
                        .repo_mut()
                        .new_commit(parents.clone(), tree)
                        .set_description(&self.config.fork.merge_message)
                        .write(),
                )?;
                commit.id().clone()
            }
        };
        let tree = native::commit(self.tx.repo(), &merge)?.tree();
        if tree.has_conflict() {
            return self.report_conflicted_merge(Some(&merge), &tree, &parents);
        }
        if self.options.checks {
            progress(&format!(
                "checking {branch} candidate {}",
                self.short_of(&merge)
            ));
            let worktree =
                self.repo
                    .worktree(&self.checker.scratch, "fork-candidate", &merge.hex())?;
            let checks = self.config.checks.fork.clone();
            if let Outcome::Fail { check, log, .. } =
                self.checker
                    .run(&worktree.path, &checks, false, "fork-branch")?
            {
                report(&format!(
                    "{branch} candidate {} fails {check} checks; {branch} not moved; log: {}",
                    self.short_of(&merge),
                    log.display()
                ));
                return Ok(Assembled::Refused);
            }
        }
        let dropped = dropped_from(self.tx.repo(), self.config, &merge)?;
        if !dropped.is_empty() {
            report_dropped(self.config, &dropped);
            return Ok(Assembled::Refused);
        }
        native::set_bookmark(&mut self.tx, &branch, &merge);
        // Work continues on an empty change above the fork branch; the previous working-copy
        // commit is left as it was.
        let wc = native::commit(self.tx.repo(), &self.jj.wc_commit)?;
        let merge_tree = native::commit(self.tx.repo(), &merge)?.tree();
        let already_above = wc.parent_ids() == std::slice::from_ref(&merge)
            && wc.tree().tree_ids_and_labels() == merge_tree.tree_ids_and_labels();
        if !already_above {
            let child = native::block_on(
                self.tx
                    .repo_mut()
                    .new_commit(vec![merge.clone()], merge_tree)
                    .write(),
            )?;
            self.jj.set_wc_commit(&mut self.tx, child.id())?;
        }
        report(&format!("{branch} -> {}", self.short_of(&merge)));
        Ok(Assembled::Done)
    }

    /// Publishes the mirror branch, every series and glue, and the fork branch with the pinned
    /// CLI, from the operation jj-fork last published (or froze), so the pushed targets and the
    /// remote positions they replace are the validated ones. Another run may have pushed while
    /// this one was checking, so it fetches again first and refuses if any relevant ref on the
    /// remote changed since the freeze, or if a bookmark to push changed locally.
    fn push(&mut self) -> Result<i32> {
        let remote = self.config.fork.remote.clone();
        let anchor = self.jj.repo.clone();
        self.repo
            .jj(&["git", "fetch", "--remote", &remote, "--quiet"])?;
        let now = self.jj.load_head()?;
        let changed = changed_names(&self.remote_snapshot, &fork_refs(now.as_ref(), self.config));
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
        let branch = self.config.fork.branch.clone();
        let mut bookmarks = vec![branch.clone()];
        bookmarks.extend(self.series.iter().chain(&self.glues).cloned());
        bookmarks.extend(self.config.fork.mirror_branch.clone());
        let mut targets = BTreeMap::new();
        for name in &bookmarks {
            let intended = native::bookmark(anchor.as_ref(), name)?;
            if native::bookmark(now.as_ref(), name)? != intended {
                report(&format!(
                    "{name} changed locally while jj-fork ran; nothing pushed. Rerun: jj fork assemble --push"
                ));
                return Ok(EXIT_NEEDS_AGENT);
            }
            targets.insert(name.clone(), intended);
        }
        let Some(fork_tip) = targets[&branch].clone() else {
            report(&format!("{branch} does not exist locally; nothing pushed"));
            return Ok(EXIT_NEEDS_AGENT);
        };
        let dropped = dropped_from(now.as_ref(), self.config, &fork_tip)?;
        if !dropped.is_empty() {
            report_dropped(self.config, &dropped);
            report("nothing pushed");
            return Ok(EXIT_NEEDS_AGENT);
        }
        let remote_tips: BTreeMap<String, Vec<CommitId>> =
            native::remote_bookmarks(now.as_ref(), &remote)
                .into_iter()
                .map(|r| (r.name, r.commits))
                .collect();
        let remote_tip = |name: &str| match remote_tips.get(name).map(Vec::as_slice) {
            Some([one]) => Some(one.clone()),
            _ => None,
        };
        if let Some(mirror) = &self.config.fork.mirror_branch {
            let fast_forward = match (remote_tip(mirror), &targets[mirror]) {
                (Some(theirs), Some(ours)) => native::is_ancestor(now.as_ref(), &theirs, ours)?,
                _ => false,
            };
            if !fast_forward {
                bookmarks.retain(|b| b != mirror);
            }
        }
        // Never push a bookmark backwards: a local commit that is a proper ancestor of the
        // remote's would drop commits there.
        let mut push = Vec::new();
        for b in bookmarks {
            let behind = match (&targets[&b], remote_tip(&b)) {
                (Some(ours), Some(theirs)) => {
                    *ours != theirs && native::is_ancestor(now.as_ref(), ours, &theirs)?
                }
                _ => false,
            };
            if behind {
                report(&format!("not pushing {b}: it is behind {b}@{remote}"));
            } else {
                push.push(b);
            }
        }
        let anchor_op = anchor.op_id().hex();
        let mut args = vec![
            "--at-op".to_string(),
            anchor_op,
            "git".into(),
            "push".into(),
            "--remote".into(),
            remote.clone(),
        ];
        for b in &push {
            args.extend(["-b".to_string(), b.clone()]);
        }
        progress(&format!("pushing {}", push.join(" ")));
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

/// Difficulty of a merge or glue conflict from its number of conflicted files.
fn conflict_files_tier(files: usize) -> &'static str {
    if files > 2 { "high" } else { "medium" }
}

/// The fork's branch, mirror, series, and glue bookmarks on its remote, as `name commits` lines.
fn fork_refs(repo: &dyn jj_lib::repo::Repo, config: &Config) -> Vec<String> {
    let fork = &config.fork;
    native::remote_bookmarks(repo, &fork.remote)
        .into_iter()
        .filter(|r| {
            r.name == fork.branch
                || fork.mirror_branch.as_deref() == Some(r.name.as_str())
                || r.name.starts_with(&fork.glue_prefix)
                || fork
                    .series_prefixes
                    .iter()
                    .any(|p| r.name.starts_with(p.as_str()))
        })
        .map(|r| format!("{} {}", r.name, ids_hex(&r.commits)))
        .collect()
}

fn ids_hex(ids: &[CommitId]) -> String {
    ids.iter().map(|id| id.hex()).collect::<Vec<_>>().join(",")
}

/// Series and glue bookmarks on the fork remote that publishing `merge` as the fork branch would
/// silently drop: neither merged into it nor deliberately deleted in this clone (tracked on the
/// remote, absent locally). A local bookmark is what gets pushed, so it is what must be merged;
/// the remote's commit only counts for a bookmark this clone has no local copy of.
fn dropped_from(
    repo: &dyn jj_lib::repo::Repo,
    config: &Config,
    merge: &CommitId,
) -> Result<Vec<String>> {
    let fork = &config.fork;
    let mut prefixes = fork.series_prefixes.clone();
    prefixes.push(fork.glue_prefix.clone());
    let remote: Vec<_> = native::remote_bookmarks(repo, &fork.remote)
        .into_iter()
        .filter(|r| prefixes.iter().any(|p| r.name.starts_with(p.as_str())))
        .collect();
    let local = |name: &str| -> Result<Option<CommitId>> { native::bookmark(repo, name) };
    let mut deleted = BTreeSet::new();
    let mut merged = BTreeSet::new();
    for r in &remote {
        let mine = local(&r.name)?;
        if r.tracked && mine.is_none() {
            deleted.insert(r.name.clone());
        }
        let tips = mine.map(|c| vec![c]).unwrap_or_else(|| r.commits.clone());
        let mut all = true;
        for tip in &tips {
            all &= native::is_ancestor(repo, tip, merge)?;
        }
        if all {
            merged.insert(r.name.clone());
        }
    }
    let pairs: Vec<(String, String)> = remote
        .iter()
        .map(|r| (r.name.clone(), ids_hex(&r.commits)))
        .collect();
    Ok(dropped_bookmarks(
        &pairs,
        |name| deleted.contains(name),
        |name, _| merged.contains(name),
    ))
}

fn report_dropped(config: &Config, dropped: &[String]) {
    let fork = &config.fork;
    report(&format!(
        "{} not moved: these bookmarks on {} are neither merged into it nor deleted here:",
        fork.branch, fork.remote
    ));
    for name in dropped {
        report(&format!("  {name}"));
    }
    report(
        "fetch and track them (jj bookmark track <name> --remote <remote>), or delete one deliberately: jj bookmark delete <name>",
    );
}

/// Remote `(name, commit)` bookmarks that would be lost: not an ancestor of the new fork branch
/// (`merged(name, commit)`) and not deliberately deleted here (`deleted(name)`).
fn dropped_bookmarks(
    remote: &[(String, String)],
    deleted: impl Fn(&str) -> bool,
    merged: impl Fn(&str, &str) -> bool,
) -> Vec<String> {
    let mut names: Vec<String> = remote
        .iter()
        .filter(|(name, commit)| !deleted(name) && !merged(name, commit))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
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

pub fn short(id: &str) -> &str {
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
    fn dropped_bookmarks_need_a_merge_or_a_deliberate_deletion() {
        let remote: Vec<(String, String)> = [
            ("patch/merged", "c1"),
            ("patch/deleted", "c2"),
            ("patch/untracked", "c3"),
            ("glue/new", "c4"),
        ]
        .iter()
        .map(|(n, c)| (n.to_string(), c.to_string()))
        .collect();
        let dropped = dropped_bookmarks(&remote, |n| n == "patch/deleted", |_, c| c == "c1");
        assert_eq!(dropped, vec!["glue/new", "patch/untracked"]);
        assert!(dropped_bookmarks(&remote, |_| true, |_, _| true).is_empty());
        assert!(dropped_bookmarks(&[], |_| false, |_, _| false).is_empty());
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
