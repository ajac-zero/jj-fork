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
use std::path::PathBuf;

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
use crate::workflow::{
    self, CheckTarget, FrozenInputs, Issue, Mapping, Plan, PlanOutcome, Proposal,
};
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
    pub save_plan: Option<PathBuf>,
    pub report_path: Option<PathBuf>,
    pub context: workflow::Context,
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
    frozen: FrozenInputs,
    mappings: Vec<Mapping>,
    check_targets: Vec<CheckTarget>,
    issues: Vec<Issue>,
    outcome: PlanOutcome,
    proposal: Option<Proposal>,
    published: Option<String>,
    push_report: Option<native::PushReport>,
    repaired_glues: BTreeSet<String>,
    _scratch: tempfile::TempDir,
}

impl<'a> Session<'a> {
    /// Prepares the clone natively, then freezes concrete inputs and the maintenance plan.
    pub fn new(repo: &'a Repo, config: &'a Config, options: Options) -> Result<Session<'a>> {
        if repo.is_shallow()? {
            progress("shallow clone detected; running init");
            crate::init::init(repo, config)?;
        }
        let jj = Native::prepare(repo, config, options.fetch)?;
        // Revisions are resolved once, here; nothing is re-resolved after checks begin.
        let target =
            jj.resolve_single(options.target.as_deref().unwrap_or(&config.upstream_ref()))?;
        let candidate = options
            .candidate
            .as_deref()
            .map(|c| jj.resolve_single(c))
            .transpose()?;
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
        let frozen = FrozenInputs::capture(
            repo,
            &jj,
            config,
            &options.context,
            &target,
            candidate.as_ref(),
        )?;
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
            frozen,
            mappings: Vec::new(),
            check_targets: Vec::new(),
            issues: Vec::new(),
            outcome: PlanOutcome::Ready,
            proposal: None,
            published: None,
            push_report: None,
            repaired_glues: BTreeSet::new(),
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
            if matches!(result.status, Status::Conflict | Status::Broken) {
                let code = if result.status == Status::Conflict {
                    "series-conflict"
                } else {
                    "series-check"
                };
                self.issues.push(Issue {
                    id: format!("{code}:{name}"),
                    code: code.into(),
                    subject: name.clone(),
                    candidate: result.candidate.as_ref().map(|id| id.hex()),
                    tier: result.tier.clone(),
                    message: result.detail.clone().unwrap_or_default(),
                });
            }
            self.results.insert(name, result);
        }
        if needs_agent {
            self.outcome = PlanOutcome::Repair;
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
        self.outcome = match assembled {
            Assembled::Done => PlanOutcome::Ready,
            Assembled::Repair => PlanOutcome::Repair,
            Assembled::Refused => PlanOutcome::Refused,
        };
        if self.options.save_plan.is_some() {
            let validation = (|| -> Result<()> {
                let mut fresh = Native::load(&self.repo.root)?;
                let config =
                    Config::load(&self.repo.root, Some(&self.options.context.config_path))?;
                self.frozen
                    .revalidate(self.repo, &mut fresh, &config, &self.options.context)?;
                workflow::probe_remotes(&fresh, &config, &self.frozen.view)
            })();
            if let Err(error) = validation {
                self.outcome = PlanOutcome::Refused;
                self.issues.push(Issue {
                    id: format!("stale_source:{command}"),
                    code: "stale_source".into(),
                    subject: command.into(),
                    candidate: None,
                    tier: None,
                    message: format!("{error:#}"),
                });
            }
            self.capture_proposal()?;
            report("saved proposal only; no planned maintenance published or pushed");
            return Ok(if self.outcome == PlanOutcome::Ready {
                EXIT_OK
            } else {
                EXIT_NEEDS_AGENT
            });
        }
        let code = match assembled {
            Assembled::Refused => {
                report("no planned maintenance published; no bookmarks moved");
                return Ok(EXIT_NEEDS_AGENT);
            }
            Assembled::Repair => EXIT_NEEDS_AGENT,
            Assembled::Done => EXIT_OK,
        };
        if self.options.report_path.is_some() {
            self.capture_proposal()?;
            self.write_artifacts(code)?;
        }
        let tx = std::mem::replace(&mut self.tx, self.jj.start());
        let changed = tx.repo().has_changes();
        let publication = if let Some(proposal) = &self.proposal {
            let op =
                jj_lib::op_store::OperationId::try_from_hex(proposal.operation.as_deref().unwrap())
                    .context("invalid prepared operation id")?;
            self.jj.publish_prepared(
                self.jj.load_prepared_operation(&op)?,
                &format!("jj-fork {command}"),
            )
        } else {
            self.jj.publish(tx, &format!("jj-fork {command}"))
        };
        match publication? {
            Publication::Done(op) if changed => {
                self.published = Some(op.hex());
                progress(&format!("published operation {}", short(&op.hex())));
            }
            Publication::Done(op) => {
                self.published = Some(op.hex());
            }
            Publication::Stale(reason) => {
                self.outcome = PlanOutcome::Refused;
                self.issues.push(Issue {
                    id: format!("stale_source:{command}"),
                    code: "stale_source".into(),
                    subject: command.into(),
                    candidate: None,
                    tier: None,
                    message: reason.clone(),
                });
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
        for original in commits.iter().rev() {
            self.mappings.push(Mapping {
                original: original.hex(),
                copy: copies[original].id().hex(),
                subject: name.into(),
            });
        }
        let candidate = copies[&tip].id().clone();
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
            return Ok(SeriesResult {
                candidate: Some(candidate),
                ..SeriesResult::new(Status::Conflict, Some(tier), Some(detail))
            });
        }
        self.check_targets.push(CheckTarget {
            subject: name.into(),
            candidate: candidate.hex(),
            patch: true,
        });
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
                return Ok(SeriesResult {
                    candidate: Some(candidate),
                    ..SeriesResult::new(
                        Status::Broken,
                        Some(&tier),
                        Some(format!(
                            "replays cleanly but fails {check} checks; log: {}",
                            log.display()
                        )),
                    )
                });
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
            .filter(|(_, r)| r.status == Status::Clean)
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
                    self.issues.push(Issue {
                        id: format!("invalid_glue:{name}"),
                        code: "invalid_glue".into(),
                        subject: name.clone(),
                        candidate: None,
                        tier: None,
                        message: reason,
                    });
                    invalid = true;
                    continue;
                }
            };
            let tip = self.tip(name)?;
            let mut parents = native::commit(self.tx.repo(), &tip)?.parent_ids().to_vec();
            parents.sort();
            anyhow::ensure!(
                !self.repaired_glues.contains(name) || parents == expected,
                "repaired glue {name} no longer has its exact required parents"
            );
            if parents != expected {
                let copies =
                    native::duplicate(&mut self.tx, std::slice::from_ref(&tip), &expected)?;
                let new = copies[&tip].id().clone();
                self.mappings.push(Mapping {
                    original: tip.hex(),
                    copy: new.hex(),
                    subject: name.clone(),
                });
                native::set_bookmark(&mut self.tx, name, &new);
                report(&format!("{name} -> {} (restacked)", self.short_of(&new)));
            }
            let tip = self.tip(name)?;
            let tree = native::commit(self.tx.repo(), &tip)?.tree();
            if tree.has_conflict() {
                let files = native::conflicted_paths(&tree)?;
                self.issues.push(Issue {
                    id: format!("glue-conflict:{name}"),
                    code: "glue-conflict".into(),
                    subject: name.clone(),
                    candidate: Some(tip.hex()),
                    tier: Some(conflict_files_tier(files.len()).into()),
                    message: "restacked glue has conflicts".into(),
                });
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
    fn conflicting_pairs(&self, parents: &[CommitId]) -> Result<Vec<(String, String, String)>> {
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
                    pairs.push((a.clone(), b.clone(), glue));
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
        let saved_conflict = if existing.is_none()
            && (self.options.save_plan.is_some() || self.options.report_path.is_some())
            && !pairs.is_empty()
        {
            Some(
                native::block_on(
                    self.tx
                        .repo_mut()
                        .new_commit(parents.to_vec(), tree.clone())
                        .set_description(&self.config.fork.merge_message)
                        .write(),
                )?
                .id()
                .hex(),
            )
        } else {
            existing.map(|id| id.hex())
        };
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
            self.issues.push(Issue {
                id: format!("fork-conflict:{branch}"),
                code: "fork-conflict".into(),
                subject: branch.clone(),
                candidate: Some(repair.id().hex()),
                tier: Some(conflict_files_tier(files.len()).into()),
                message: "fork merge has conflicts between series without a conflicting pair"
                    .into(),
            });
            report(&format!(
                "no single pair conflicts; resolve the merge in that commit, then run: jj fork assemble --candidate {} --push",
                native::short_change(&repair)
            ));
        } else {
            for (a, b, glue) in &pairs {
                self.issues.push(Issue {
                    id: format!("glue-needed:{glue}"),
                    code: "glue-needed".into(),
                    subject: glue.clone(),
                    candidate: saved_conflict.clone(),
                    tier: Some(conflict_files_tier(files.len()).into()),
                    message: format!("add {glue} to resolve {a} + {b}"),
                });
                report(&format!(
                    "  conflicting pair: {a} + {b}: add {glue} with: jj new '{}' '{}'",
                    Repo::bookmark_revset(a),
                    Repo::bookmark_revset(b)
                ));
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
                self.issues.push(Issue {
                    id: format!("nested_series:{line}"),
                    code: "nested_series".into(),
                    subject: branch.clone(),
                    candidate: None,
                    tier: None,
                    message: line.clone(),
                });
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
                    self.issues.push(Issue {
                        id: format!("candidate_membership:{branch}"),
                        code: "candidate_membership".into(),
                        subject: branch.clone(),
                        candidate: Some(candidate.hex()),
                        tier: None,
                        message: "candidate does not merge exactly the resolved members".into(),
                    });
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
                if self.options.save_plan.is_none()
                    && (!self.options.checks
                        || remote.as_deref() == Some(std::slice::from_ref(&local)))
                {
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
        self.check_targets.push(CheckTarget {
            subject: branch.clone(),
            candidate: merge.hex(),
            patch: false,
        });
        if self.options.checks {
            progress(&format!(
                "checking {branch} candidate {}",
                self.short_of(&merge)
            ));
            let worktree =
                self.repo
                    .worktree(&self.checker.scratch, "fork-candidate", &merge.hex())?;
            let checks = self.config.checks.fork.clone();
            let first_record = self.checker.records.len();
            let checked = self
                .checker
                .run(&worktree.path, &checks, false, "fork-branch")?;
            for record in &mut self.checker.records[first_record..] {
                record.subject.clone_from(&branch);
            }
            if let Outcome::Fail { check, tier, log } = checked {
                self.issues.push(Issue {
                    id: format!("fork-check:{branch}"),
                    code: "fork-check".into(),
                    subject: branch.clone(),
                    candidate: Some(merge.hex()),
                    tier: Some(tier),
                    message: format!("fails {check} checks; log: {}", log.display()),
                });
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
            for name in &dropped {
                self.issues.push(Issue {
                    id: format!("remote_membership:{name}"),
                    code: "remote_membership".into(),
                    subject: name.clone(),
                    candidate: Some(merge.hex()),
                    tier: None,
                    message: "remote bookmark is neither merged nor deliberately deleted".into(),
                });
            }
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

    /// Push only exact locally published targets with native explicit remote leases.
    fn push(&mut self) -> Result<i32> {
        self.push_report = push_checked(
            &mut self.jj,
            self.config,
            &self.remote_snapshot,
            &self.series,
            &self.glues,
        )?;
        Ok(match &self.push_report {
            Some(r) if r.all_accepted() => EXIT_OK,
            Some(_) => 1,
            None => EXIT_NEEDS_AGENT,
        })
    }

    pub fn finish(&self) {
        for failure in &self.checker.upstream_failures {
            report(&format!("note: also fails on upstream, ignored: {failure}"));
        }
    }

    fn capture_proposal(&mut self) -> Result<()> {
        if self.proposal.is_some() {
            return Ok(());
        }
        let tx = std::mem::replace(&mut self.tx, self.jj.start());
        let prepared =
            native::block_on(tx.write("jj-fork: saved unpublished proposal"))?.leave_unpublished();
        self.proposal = Some(workflow::proposal(
            self.jj.repo.as_ref(),
            prepared.as_ref(),
            &self.frozen.workspace_name,
            self.mappings.clone(),
            self.check_targets.clone(),
            Some(prepared.op_id().hex()),
        )?);
        Ok(())
    }

    pub fn record_error(&mut self, error: &anyhow::Error) {
        self.outcome = PlanOutcome::Refused;
        if self.jj.repo.op_id().hex() != self.frozen.base_operation {
            self.published = Some(self.jj.repo.op_id().hex());
        }
        self.issues.push(Issue {
            id: format!("engine_error:{}", self.options.context.command),
            code: "engine_error".into(),
            subject: self.options.context.command.clone(),
            candidate: None,
            tier: None,
            message: format!("{error:#}"),
        });
    }

    pub fn write_artifacts(&mut self, code: i32) -> Result<()> {
        if self.options.save_plan.is_none() && self.options.report_path.is_none() {
            return Ok(());
        }
        self.capture_proposal()?;
        let plan = Plan {
            context: self.options.context.clone(),
            frozen: self.frozen.clone(),
            proposal: self.proposal.clone().unwrap(),
            outcome: self.outcome.clone(),
            issues: self.issues.clone(),
            checks: self.checker.records.clone(),
        };
        if let Some(path) = &self.options.save_plan {
            crate::artifact::save_plan(path, self.jj.repository_path(), &plan)?;
        }
        if let Some(path) = &self.options.report_path {
            crate::artifact::save_report(
                path,
                &workflow::Report {
                    diagnostics: plan.issues.clone(),
                    plan: Some(plan),
                    exit_code: code,
                    published_operation: self.published.clone(),
                    push: self.push_report.clone(),
                },
            )?;
        }
        Ok(())
    }
}

pub fn push_saved(
    _repo: &Repo,
    jj: &mut Native,
    config: &Config,
    plan: &Plan,
) -> Result<Option<native::PushReport>> {
    let snapshot: Vec<_> = plan
        .frozen
        .view
        .remotes
        .iter()
        .filter(|r| {
            r.remote == config.fork.remote
                && !r.tag
                && (r.name == config.fork.branch
                    || config.fork.mirror_branch.as_deref() == Some(&r.name)
                    || r.name.starts_with(&config.fork.glue_prefix)
                    || config
                        .fork
                        .series_prefixes
                        .iter()
                        .any(|p| r.name.starts_with(p)))
        })
        .map(|r| {
            format!(
                "{} {}",
                r.name,
                r.target
                    .terms
                    .iter()
                    .step_by(2)
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect();
    push_checked(
        jj,
        config,
        &snapshot,
        &plan.frozen.series,
        &plan
            .frozen
            .glues
            .iter()
            .chain(&plan.proposal.approved_new_glues)
            .cloned()
            .collect::<Vec<_>>(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairKind {
    Series,
    Glue,
    NewGlue,
    Fork,
}

pub struct RepairReplacement {
    pub issue: String,
    pub subject: String,
    pub kind: RepairKind,
    /// Whole repaired series, parents first; other kinds contain exactly one commit.
    pub commits: Vec<CommitId>,
}

/// Rebuild a successor from approved imported objects, never by preparing a different base.
/// Task verification/import owns path scope; this boundary independently validates graph shape,
/// change identities, named issue scope, downstream membership, and every required check.
pub fn rebuild_repaired(
    repo: &Repo,
    config: &Config,
    original: &Plan,
    replacements: &[RepairReplacement],
) -> Result<Plan> {
    let prepared = workflow::load_proposal(repo, config, original)?;
    let mut jj = Native::load(&repo.root)?;
    original
        .frozen
        .revalidate(repo, &mut jj, config, &original.context)?;
    anyhow::ensure!(
        original.outcome != PlanOutcome::Ready && !replacements.is_empty(),
        "repair requires a not-ready plan and at least one replacement"
    );
    let scratch = tempfile::Builder::new()
        .prefix("jj-fork-rebuild.")
        .tempdir()?;
    let logs = std::env::temp_dir().join("jj-fork-logs").join(format!(
        "repair-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&logs)?;
    let checker = Checker::new(
        repo,
        config,
        original.frozen.target.clone(),
        scratch.path().into(),
        logs,
    );
    let generated = globs(
        &config
            .generated
            .as_ref()
            .map(|g| g.paths.clone())
            .unwrap_or_default(),
    )?;
    let mut tx = jj.start();
    tx.repo_mut().merge_index(&prepared)?;
    let mut session = Session {
        repo,
        config,
        options: Options {
            target: None,
            candidate: None,
            fetch: false,
            checks: true,
            push: false,
            save_plan: None,
            report_path: None,
            context: original.context.clone(),
        },
        target: native::parse_id(&original.frozen.target)?,
        candidate: None,
        series: original.frozen.series.clone(),
        glues: original
            .frozen
            .glues
            .iter()
            .chain(&original.proposal.approved_new_glues)
            .cloned()
            .collect(),
        remote_snapshot: fork_refs(jj.repo.as_ref(), config),
        jj,
        tx,
        generated,
        results: BTreeMap::new(),
        checker,
        frozen: original.frozen.clone(),
        mappings: original.proposal.mappings.clone(),
        check_targets: Vec::new(),
        issues: Vec::new(),
        outcome: PlanOutcome::Ready,
        proposal: None,
        published: None,
        push_report: None,
        repaired_glues: BTreeSet::new(),
        _scratch: scratch,
    };
    // Carry planned series and restacked glue tips, not the old proposal's conflicted graph
    // heads/workspace. The successor graph starts at the frozen source view.
    for name in session.series.clone() {
        if replacements.iter().any(|r| r.subject == name) {
            continue;
        }
        let tip = session
            .mappings
            .iter()
            .rev()
            .find(|m| m.subject == name)
            .map(|m| native::parse_id(&m.copy))
            .transpose()?;
        if let Some(tip) = tip {
            let commit = native::commit(session.tx.repo(), &tip)?;
            native::block_on(session.tx.repo_mut().add_head(&commit))?;
            native::set_bookmark(&mut session.tx, &name, &tip);
        }
    }
    for name in session.glues.clone() {
        if replacements.iter().any(|r| r.subject == name) {
            continue;
        }
        if let Some(tip) = native::bookmark(prepared.as_ref(), &name)? {
            native::set_bookmark(&mut session.tx, &name, &tip);
        }
    }
    let mut subjects = BTreeSet::new();
    let mut approved = original.proposal.approved_new_glues.clone();
    for replacement in replacements {
        anyhow::ensure!(
            subjects.insert(replacement.subject.clone()),
            "duplicate repair subject"
        );
        let issue = original
            .issues
            .iter()
            .find(|i| i.id == replacement.issue)
            .context("repair issue is absent from parent plan")?;
        anyhow::ensure!(
            issue.subject == replacement.subject,
            "repair issue/subject mismatch"
        );
        let valid_kind = match replacement.kind {
            RepairKind::Series => matches!(issue.code.as_str(), "series-conflict" | "series-check"),
            RepairKind::Glue => issue.code == "glue-conflict",
            RepairKind::NewGlue => issue.code == "glue-needed",
            RepairKind::Fork => matches!(issue.code.as_str(), "fork-conflict" | "fork-check"),
        };
        anyhow::ensure!(
            valid_kind && !replacement.commits.is_empty(),
            "invalid repair kind or empty result"
        );
        if replacement.kind != RepairKind::Series {
            anyhow::ensure!(
                replacement.commits.len() == 1,
                "non-series repair must contain one commit"
            );
        }
        for id in &replacement.commits {
            let commit = native::commit(session.tx.repo(), id)?;
            anyhow::ensure!(
                !commit.has_conflict(),
                "submitted repair still contains conflicts"
            );
            native::block_on(session.tx.repo_mut().add_head(&commit))?;
        }
        let tip = replacement.commits.last().unwrap();
        match replacement.kind {
            RepairKind::Series => {
                anyhow::ensure!(
                    session.series.contains(&replacement.subject),
                    "repair is not a frozen series"
                );
                let mappings: Vec<_> = session
                    .mappings
                    .iter_mut()
                    .filter(|m| m.subject == replacement.subject)
                    .collect();
                anyhow::ensure!(
                    mappings.len() == replacement.commits.len(),
                    "repaired series changed commit count"
                );
                let mut parent = session.target.clone();
                for (mapping, id) in mappings.into_iter().zip(&replacement.commits) {
                    let prior =
                        native::commit(prepared.as_ref(), &native::parse_id(&mapping.copy)?)?;
                    let commit = native::commit(session.tx.repo(), id)?;
                    anyhow::ensure!(
                        commit.parent_ids() == std::slice::from_ref(&parent)
                            && commit.change_id() == prior.change_id(),
                        "repaired series changed parents or change identities"
                    );
                    mapping.copy = id.hex();
                    parent = id.clone();
                }
                native::set_bookmark(&mut session.tx, &replacement.subject, tip);
            }
            RepairKind::Glue | RepairKind::NewGlue => {
                if replacement.kind == RepairKind::NewGlue {
                    anyhow::ensure!(
                        !session.glues.contains(&replacement.subject),
                        "new glue already exists"
                    );
                    session.glues.push(replacement.subject.clone());
                    approved.push(replacement.subject.clone());
                }
                native::set_bookmark(&mut session.tx, &replacement.subject, tip);
                session.repaired_glues.insert(replacement.subject.clone());
            }
            RepairKind::Fork => session.candidate = Some(tip.clone()),
        }
    }
    // Every planned/repaired series is checked again, including one now based on the target.
    for name in session.series.clone() {
        if !subjects.contains(&name)
            && !original.proposal.mappings.iter().any(|m| m.subject == name)
        {
            continue;
        }
        let tip = session.tip(&name)?;
        let unresolved = original.issues.iter().find(|i| {
            i.subject == name && i.code == "series-conflict" && !subjects.contains(&name)
        });
        if let Some(issue) = unresolved {
            session.issues.push(issue.clone());
            session.outcome = PlanOutcome::Repair;
            continue;
        }
        session.check_targets.push(CheckTarget {
            subject: name.clone(),
            candidate: tip.hex(),
            patch: true,
        });
        let worktree = repo.worktree(
            &session.checker.scratch,
            &name.replace('/', "_"),
            &tip.hex(),
        )?;
        if let Outcome::Fail { check, tier, log } =
            session
                .checker
                .run(&worktree.path, &config.checks.patch, true, &name)?
        {
            session.issues.push(Issue {
                id: format!("series-check:{name}"),
                code: "series-check".into(),
                subject: name,
                candidate: Some(tip.hex()),
                tier: Some(tier),
                message: format!("fails {check}; log: {}", log.display()),
            });
            session.outcome = PlanOutcome::Repair;
        }
    }
    if session.outcome == PlanOutcome::Ready {
        let assembled = session.assemble()?;
        session.outcome = match assembled {
            Assembled::Done => PlanOutcome::Ready,
            Assembled::Repair => PlanOutcome::Repair,
            Assembled::Refused => PlanOutcome::Refused,
        };
    }
    // A glue is a single resolution, so every old-to-copy edge now points to its final
    // resolution. Superseded conflicted copies remain in the parent plan, not Ready heads.
    for name in session.glues.clone() {
        let tip = session.tip(&name)?;
        for mapping in session.mappings.iter_mut().filter(|m| m.subject == name) {
            mapping.copy = tip.hex();
        }
        let commit = native::commit(session.tx.repo(), &tip)?;
        native::block_on(session.tx.repo_mut().add_head(&commit))?;
    }
    let mut fresh = Native::load(&repo.root)?;
    original
        .frozen
        .revalidate(repo, &mut fresh, config, &original.context)?;
    workflow::probe_remotes(&fresh, config, &original.frozen.view)?;
    session.capture_proposal()?;
    let mut proposal = session.proposal.take().unwrap();
    proposal.approved_new_glues = approved;
    let plan = Plan {
        context: original.context.clone(),
        frozen: original.frozen.clone(),
        proposal,
        outcome: session.outcome,
        issues: session.issues,
        checks: session.checker.records,
    };
    let loaded = session.jj.load_prepared_operation(
        &jj_lib::op_store::OperationId::try_from_hex(plan.proposal.operation.as_deref().unwrap())
            .context("invalid prepared operation id")?,
    )?;
    workflow::validate_proposal(&plan, session.jj.repo.as_ref(), loaded.as_ref(), config)?;
    Ok(plan)
}

fn push_checked(
    jj: &mut Native,
    config: &Config,
    snapshot: &[String],
    series: &[String],
    glues: &[String],
) -> Result<Option<native::PushReport>> {
    let remote = &config.fork.remote;
    let anchor = jj.repo.clone();
    let observed = jj.probe_remote(remote)?;
    let changed = changed_names(snapshot, &fork_refs(observed.as_ref(), config));
    if !changed.is_empty() {
        report(&format!(
            "{remote} changed while jj-fork ran; nothing pushed. Changed on {remote}:"
        ));
        for name in &changed {
            report(&format!("  {name}"));
        }
        report("rerun: jj fork assemble --push");
        return Ok(None);
    }
    let now = jj.load_head()?;
    let mut names = vec![config.fork.branch.clone()];
    names.extend(series.iter().chain(glues).cloned());
    names.extend(config.fork.mirror_branch.clone());
    names.sort();
    names.dedup();
    let fork =
        native::bookmark(anchor.as_ref(), &config.fork.branch)?.context("fork branch is absent")?;
    let dropped = dropped_from(now.as_ref(), config, &fork)?;
    if !dropped.is_empty() {
        report_dropped(config, &dropped);
        report("nothing pushed");
        return Ok(None);
    }
    let mut updates = Vec::new();
    for name in names {
        let intended = native::bookmark(anchor.as_ref(), &name)?;
        if native::bookmark(now.as_ref(), &name)? != intended {
            report(&format!(
                "{name} changed locally while jj-fork ran; nothing pushed"
            ));
            return Ok(None);
        }
        let Some(after) = intended else {
            continue;
        };
        let remote_ref = observed
            .view()
            .get_remote_bookmark(jj_lib::ref_name::RemoteRefSymbol {
                name: jj_lib::ref_name::RefName::new(&name),
                remote: jj_lib::ref_name::RemoteName::new(remote),
            });
        anyhow::ensure!(
            !remote_ref.target.has_conflict(),
            "remote target for {name} is conflicted"
        );
        let before = remote_ref.target.as_normal().cloned();
        if let Some(theirs) = &before {
            if *theirs == after {
                continue;
            }
            if native::is_ancestor(observed.as_ref(), &after, theirs)? {
                report(&format!("not pushing {name}: it is behind {name}@{remote}"));
                continue;
            }
            if config.fork.mirror_branch.as_deref() == Some(&name)
                && !native::is_ancestor(observed.as_ref(), theirs, &after)?
            {
                continue;
            }
        }
        updates.push(native::PushUpdate {
            name,
            before,
            after,
        });
    }
    progress(&format!(
        "pushing {}",
        updates
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let result = jj.push_explicit(remote, &updates)?;
    for line in result.lines() {
        report(&line);
    }
    Ok(Some(result))
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
    native::unmerged_remote_bookmarks(repo, &fork.remote, &prefixes, merge)
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
