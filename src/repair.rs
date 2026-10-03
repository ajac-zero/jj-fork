//! Isolated repair tasks and successor plans.
//!
//! `repair start` turns one issue of an authenticated saved plan into a task: a new directory
//! with its own Git object database and its own colocated jj repository, seeded with the exact
//! commits the issue concerns and a `repair/result` bookmark. Nothing is shared with the source:
//! no Git worktree or jj workspace, no remotes, hooks, credentials, configuration, or artifact
//! authority. Only the manifest is signed, with the source repository's authority, so a task
//! cannot widen its own scope.
//!
//! `repair submit` reads each task's `repair/result` once, validates its topology, change
//! identities, metadata, and every path and mode it changes against the signed scope, imports
//! only the result's new objects into the source object database, and asks the workflow engine
//! to rebuild an unpublished successor proposal with every required check rerun. It saves the
//! successor plan and never publishes source bookmarks or checkouts or pushes; `apply` remains
//! the only publication boundary.
//!
//! The separate repository protects the source from accidental shared ref, operation, or
//! working-copy mutation. It is not a sandbox: a process in the task can still reach the
//! source's files, so untrusted workers need process and filesystem isolation of their own.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Subcommand;
use futures::StreamExt as _;
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::git;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::MergedTreeValue;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefName;
use jj_lib::repo::{Repo as JjRepo, StoreFactories};
use jj_lib::revset::RevsetExpression;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{Workspace, default_working_copy_factories};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::native::{self, Native, block_on};
use crate::repo::Repo;
use crate::sync::{EXIT_NEEDS_AGENT, EXIT_OK, RepairKind, RepairReplacement};
use crate::workflow::{self, Issue, Plan, PlanOutcome};
use crate::{artifact, glue, report};

/// The task bookmark that names the repaired result.
pub const RESULT_BOOKMARK: &str = "repair/result";

/// Where the signed manifest lives, relative to the task directory. jj ignores `.jj` when it
/// snapshots, so the manifest is never committed by the worker.
pub fn manifest_path(task_dir: &Path) -> PathBuf {
    task_dir.join(".jj").join("jj-fork-task.json")
}

#[derive(Subcommand)]
pub enum Command {
    /// Create an isolated repository for one issue of a saved plan. Publishes nothing.
    Start {
        /// The saved plan (sync or assemble --save-plan).
        plan: PathBuf,
        /// The issue id, as listed in the plan (code:subject).
        #[arg(long)]
        issue: String,
        /// A new or empty directory outside the source workspace.
        #[arg(long)]
        dir: PathBuf,
        /// A repository path (file or directory) the repair may change. Conflict repairs may
        /// already change their conflicted paths; check-failure repairs need at least one.
        #[arg(long = "allow-path", value_name = "PATH")]
        allow_path: Vec<String>,
    },
    /// Verify finished tasks from one plan and save their successor plan. Publishes nothing;
    /// run apply on the successor plan to publish.
    Submit {
        #[arg(required = true, value_name = "TASK_DIR")]
        tasks: Vec<PathBuf>,
        #[arg(long, value_name = "NEXT_PLAN")]
        save_plan: PathBuf,
        #[arg(long, value_name = "FILE")]
        report: Option<PathBuf>,
    },
}

/// What a task repairs, as signed in its manifest. Maps one-to-one onto the workflow engine's
/// `RepairKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    /// A linear series copied onto the target: same commit count, order, and change ids.
    Series,
    /// A restacked glue: one resolution commit on the glue's exact parents.
    Glue,
    /// A glue that does not exist yet, named by the plan: one commit on its exact parents.
    NewGlue,
    /// The fork-branch merge: one commit on the merge's exact parents.
    Fork,
}

impl From<TaskKind> for RepairKind {
    fn from(kind: TaskKind) -> RepairKind {
        match kind {
            TaskKind::Series => RepairKind::Series,
            TaskKind::Glue => RepairKind::Glue,
            TaskKind::NewGlue => RepairKind::NewGlue,
            TaskKind::Fork => RepairKind::Fork,
        }
    }
}

/// A series or glue whose current tip a task builds on. A batch may not replace it as well.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub subject: String,
    pub commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedCommit {
    pub commit: String,
    /// Forward lowercase hex, as in plans.
    pub change: String,
}

/// The signed task manifest (artifact kind `jj-fork-task`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// The parent plan, exactly as it was authenticated when the task started.
    pub plan: Plan,
    pub issue: String,
    /// The series or glue bookmark, or the fork branch, that the result replaces.
    pub subject: String,
    pub kind: TaskKind,
    /// Exact parents of the first (or only) result commit, in order.
    pub parents: Vec<String>,
    pub dependencies: Vec<Dependency>,
    /// The seeded commits the result must correspond to, parents first. `repair/result`
    /// started at the last one.
    pub seed: Vec<SeedCommit>,
    /// Every source commit copied into the task, with its full tree, sorted.
    pub objects: Vec<String>,
    /// Copied commits whose parents were not copied (Git shallow roots in the task), sorted.
    pub boundary: Vec<String>,
    /// Conflicted paths across the seed, sorted.
    pub conflict_paths: Vec<String>,
    /// Normalized repository paths the result may change: each a file or directory, sorted.
    pub scope: Vec<String>,
}

impl Task {
    /// Checks ids and paths once the manifest is authenticated, before anything uses them.
    pub fn validate_ids(&self) -> Result<()> {
        self.plan.validate_ids()?;
        let hex = |id: &str, length: usize| -> Result<()> {
            ensure!(
                id.len() == length
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "malformed task object id"
            );
            Ok(())
        };
        for id in self
            .parents
            .iter()
            .chain(&self.objects)
            .chain(&self.boundary)
        {
            hex(id, 40)?;
        }
        for dependency in &self.dependencies {
            hex(&dependency.commit, 40)?;
        }
        ensure!(!self.seed.is_empty(), "task has no seed");
        for seed in &self.seed {
            hex(&seed.commit, 40)?;
            hex(&seed.change, 32)?;
        }
        ensure!(
            self.kind == TaskKind::Series || self.seed.len() == 1,
            "a merge task has exactly one seed commit"
        );
        for path in self.scope.iter().chain(&self.conflict_paths) {
            ensure!(
                normalize_path(path).as_deref() == Ok(path.as_str()),
                "malformed task path"
            );
        }
        ensure!(
            self.plan
                .issues
                .iter()
                .any(|i| i.id == self.issue && i.subject == self.subject)
                || self.kind == TaskKind::NewGlue
                    && self.plan.issues.iter().any(|i| i.id == self.issue),
            "task issue is not in its plan"
        );
        Ok(())
    }

    fn parent_ids(&self) -> Result<Vec<CommitId>> {
        self.parents.iter().map(|id| native::parse_id(id)).collect()
    }
}

pub fn run(repo: &Repo, config: &Config, command: Command) -> Result<i32> {
    match command {
        Command::Start {
            plan,
            issue,
            dir,
            allow_path,
        } => start(repo, config, &plan, &issue, &dir, &allow_path),
        Command::Submit {
            tasks,
            save_plan,
            report,
        } => submit(repo, config, &tasks, &save_plan, report.as_deref()),
    }
}

// Starting a task.

/// What an issue asks to repair, resolved against the plan's proposal.
struct Spec {
    kind: TaskKind,
    subject: String,
    parents: Vec<CommitId>,
    /// Existing seed commits, parents first; empty for a new glue, which the task creates.
    seed: Vec<CommitId>,
    /// Commits whose ancestry down to the anchors is copied.
    tips: Vec<CommitId>,
}

fn start(
    repo: &Repo,
    config: &Config,
    plan_path: &Path,
    issue_id: &str,
    dir: &Path,
    allow: &[String],
) -> Result<i32> {
    let jj = Native::load(&repo.root)?;
    let repository = canonical(jj.repository_path())?;
    let plan = artifact::load_plan(plan_path, &repository)?;
    ensure_same_source(repo, &repository, &plan)?;
    let issue = plan
        .issues
        .iter()
        .find(|i| i.id == issue_id)
        .with_context(|| {
            format!(
                "plan has no issue {issue_id}; issues: {}",
                plan.issues
                    .iter()
                    .map(|i| i.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?
        .clone();
    let protected = protected_paths(&plan);
    let mut extra = BTreeSet::new();
    for path in allow {
        let path = normalize_path(path).map_err(|e| anyhow!("--allow-path {path:?}: {e}"))?;
        ensure!(
            !is_protected(&path, &protected),
            "--allow-path {path}: repository metadata and jj-fork configuration cannot be repaired in a task"
        );
        extra.insert(path);
    }
    let conflict_issue = match issue.code.as_str() {
        "series-conflict" | "glue-conflict" | "fork-conflict" | "glue-needed" => true,
        "series-check" | "fork-check" => false,
        other => bail!("issue {issue_id} ({other}) cannot be repaired in a task"),
    };
    if !conflict_issue {
        ensure!(
            !extra.is_empty(),
            "{issue_id} is a check failure; name the paths the repair may change with --allow-path"
        );
    }
    let dir = new_task_dir(dir, &repo.root)?;

    let prepared = workflow::load_proposal(repo, config, &plan)?;
    let spec = resolve(&plan, prepared.as_ref(), config, &issue)?;
    let target = native::parse_id(&plan.frozen.target)?;
    let (objects, boundary) = seeded_graph(prepared.as_ref(), &target, &spec)?;
    let dependencies = dependencies(&plan, prepared.as_ref(), &spec)?;
    let settings = task_settings(prepared.settings())?;

    // The task repository: jj creates the colocated Git database, then the seeded objects are
    // copied in by Git with the source's configuration. The task's own configuration is new.
    block_on(Workspace::init_colocated_git(&settings, &dir))
        .context("failed to initialize the task repository")?;
    let task_git = dir.join(".git");
    copy_objects(&repo.root, None, &task_git, &objects, &[])?;
    let mut shallow: Vec<String> = boundary.iter().map(|id| id.hex()).collect();
    shallow.sort();
    std::fs::write(
        task_git.join("shallow"),
        shallow
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>(),
    )?;

    let mut workspace = load_task_workspace(&settings, &dir)?;
    let task_repo = block_on(workspace.repo_loader().load_at_head())?;
    let mut tx = task_repo.start_transaction();
    tx.set_attribute("args".into(), "jj-fork repair start".into());
    let seed: Vec<Commit> = if spec.seed.is_empty() {
        for parent in &spec.parents {
            let parent = native::commit(tx.repo(), parent)?;
            block_on(tx.repo_mut().add_head(&parent))?;
        }
        let tree = native::merged_tree(tx.repo(), &spec.parents)?;
        vec![block_on(
            tx.repo_mut().new_commit(spec.parents.clone(), tree).write(),
        )?]
    } else {
        spec.seed
            .iter()
            .map(|id| native::commit(tx.repo(), id))
            .collect::<Result<_>>()?
    };
    for (commit, id) in seed.iter().zip(&spec.seed) {
        let source = native::commit(prepared.as_ref(), id)?;
        ensure!(
            commit.change_id() == source.change_id(),
            "commit {} carries its change id only in the source's jj metadata; jj-fork tasks need git.write-change-id-header = true",
            id.hex()
        );
    }
    let tip = seed.last().expect("seed is not empty").clone();
    // Indexes the seeded graph; the boundary commits read as children of the root.
    block_on(tx.repo_mut().add_head(&tip))?;
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(RESULT_BOOKMARK),
        RefTarget::normal(tip.id().clone()),
    );
    let wc = block_on(
        tx.repo_mut()
            .new_commit(vec![tip.id().clone()], tip.tree())
            .write(),
    )?;
    tx.repo_mut()
        .set_wc_commit(workspace.workspace_name().to_owned(), wc.id().clone())
        .map_err(|_| anyhow!("cannot check out the task result"))?;
    let task_repo = block_on(tx.commit("jj-fork repair start"))?;
    block_on(workspace.check_out(task_repo.op_id().clone(), None, &wc))
        .context("failed to check out the task")?;
    let mut tx = task_repo.start_transaction();
    block_on(git::reset_head(tx.repo_mut(), &wc)).context("failed to set the task's Git HEAD")?;
    git::export_refs(tx.repo_mut()).context("failed to export the task's Git refs")?;
    block_on(tx.commit("jj-fork repair start: export to Git"))?;

    let mut conflicts = BTreeSet::new();
    for commit in &seed {
        for file in native::conflicted_paths(&commit.tree())? {
            conflicts.insert(file.path);
        }
    }
    let mut scope = extra;
    if conflict_issue {
        for path in &conflicts {
            if is_protected(path, &protected) {
                report(&format!(
                    "note: {path} is conflicted but protected; this task cannot change it"
                ));
            } else {
                scope.insert(path.clone());
            }
        }
    }
    let task = Task {
        plan,
        issue: issue.id.clone(),
        subject: spec.subject.clone(),
        kind: spec.kind,
        parents: spec.parents.iter().map(|id| id.hex()).collect(),
        dependencies,
        seed: seed
            .iter()
            .map(|c| SeedCommit {
                commit: c.id().hex(),
                change: c.change_id().hex(),
            })
            .collect(),
        objects: sorted_hex(&objects),
        boundary: shallow,
        conflict_paths: conflicts.into_iter().collect(),
        scope: scope.into_iter().collect(),
    };
    artifact::save_task(&manifest_path(&dir), &repository, &task)?;

    report(&format!(
        "task {} for {}: {} {}",
        dir.display(),
        task.issue,
        kind_label(task.kind),
        task.subject
    ));
    report(&format!(
        "{RESULT_BOOKMARK} -> {} ({} commit{} on {})",
        short(&tip.id().hex()),
        task.seed.len(),
        if task.seed.len() == 1 { "" } else { "s" },
        task.parents
            .iter()
            .map(|id| short(id))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for path in &task.conflict_paths {
        report(&format!("  conflicted: {path}"));
    }
    report(&format!("may change: {}", task.scope.join(", ")));
    report(match task.kind {
        TaskKind::Series => {
            "repair inside the existing commits (jj edit or jj squash into them): keep their order, count, change ids, descriptions, and parent"
        }
        _ => {
            "repair inside the seeded commit (jj squash into it): keep its parents, change id, and description"
        }
    });
    report(&format!(
        "leave {RESULT_BOOKMARK} on the result, then from the source run: jj fork repair submit {} --save-plan <NEXT_PLAN>",
        dir.display()
    ));
    Ok(EXIT_OK)
}

/// Resolves an issue to the commits a task seeds and the exact parents its result keeps.
fn resolve(plan: &Plan, repo: &dyn JjRepo, config: &Config, issue: &Issue) -> Result<Spec> {
    let candidate = || -> Result<CommitId> {
        native::parse_id(
            issue
                .candidate
                .as_deref()
                .with_context(|| format!("issue {} names no candidate", issue.id))?,
        )
    };
    let fork = &config.fork;
    match issue.code.as_str() {
        "series-conflict" | "series-check" => {
            ensure!(
                plan.frozen.series.contains(&issue.subject),
                "{} is not a series of the plan",
                issue.subject
            );
            let target = native::parse_id(&plan.frozen.target)?;
            let tip = candidate()?;
            let mut chain = native::range(repo, &target, &tip)?;
            chain.reverse();
            ensure!(!chain.is_empty(), "series candidate is the target itself");
            let copies: BTreeSet<&str> = plan
                .proposal
                .mappings
                .iter()
                .filter(|m| m.subject == issue.subject)
                .map(|m| m.copy.as_str())
                .collect();
            let mut parent = target.clone();
            for id in &chain {
                let commit = native::commit(repo, id)?;
                ensure!(
                    commit.parent_ids() == std::slice::from_ref(&parent),
                    "the saved copy of {} is not a linear chain on the target",
                    issue.subject
                );
                ensure!(
                    copies.contains(id.hex().as_str()),
                    "commit {} is not a saved copy of {}",
                    id.hex(),
                    issue.subject
                );
                parent = id.clone();
            }
            Ok(Spec {
                kind: TaskKind::Series,
                subject: issue.subject.clone(),
                parents: vec![target],
                tips: vec![tip],
                seed: chain,
            })
        }
        "glue-conflict" => {
            ensure!(
                plan_glues(plan).contains(&issue.subject),
                "{} is not a glue of the plan",
                issue.subject
            );
            let glue = candidate()?;
            let parents = native::commit(repo, &glue)?.parent_ids().to_vec();
            Ok(Spec {
                kind: TaskKind::Glue,
                subject: issue.subject.clone(),
                parents,
                tips: vec![glue.clone()],
                seed: vec![glue],
            })
        }
        "fork-conflict" | "fork-check" => {
            ensure!(
                issue.subject == fork.branch,
                "{} is not the fork branch",
                issue.subject
            );
            let merge = candidate()?;
            let parents = native::commit(repo, &merge)?.parent_ids().to_vec();
            ensure!(parents.len() > 1, "fork candidate is not a merge");
            Ok(Spec {
                kind: TaskKind::Fork,
                subject: issue.subject.clone(),
                parents,
                tips: vec![merge.clone()],
                seed: vec![merge],
            })
        }
        "glue-needed" => {
            let name = &issue.subject;
            ensure!(
                !plan_glues(plan).contains(name) && native::bookmark(repo, name)?.is_none(),
                "glue {name} already exists"
            );
            let series = glue::series_of(
                name,
                &fork.glue_prefix,
                &fork.series_prefixes,
                &plan.frozen.series,
            )
            .map_err(|reason| anyhow!("glue {name} is invalid: {reason}"))?;
            let glues = plan_glues(plan);
            let mut tips = Vec::new();
            for member in series.iter().map(String::as_str).chain(
                glue::inner_glues(name, &fork.glue_prefix, &glues)
                    .into_iter()
                    .map(String::as_str),
            ) {
                tips.push(
                    native::bookmark(repo, member)?
                        .with_context(|| format!("{member} is absent from the proposal"))?,
                );
            }
            let parents = native::heads(repo, &tips)?;
            ensure!(parents.len() > 1, "glue {name} would not be a merge");
            Ok(Spec {
                kind: TaskKind::NewGlue,
                subject: name.clone(),
                tips: parents.clone(),
                parents,
                seed: Vec::new(),
            })
        }
        other => bail!("issue code {other} cannot be repaired in a task"),
    }
}

/// The source commits a task copies: everything from the anchors (the target and every
/// pairwise merge base of the parents) up to the tips. Merges computed inside the task then
/// find the same bases as the source. Returns the commits and the shallow boundary.
fn seeded_graph(
    repo: &dyn JjRepo,
    target: &CommitId,
    spec: &Spec,
) -> Result<(Vec<CommitId>, Vec<CommitId>)> {
    let mut anchors = BTreeSet::from([target.clone()]);
    for (i, a) in spec.parents.iter().enumerate() {
        for b in &spec.parents[i + 1..] {
            anchors.extend(
                repo.index()
                    .common_ancestors(std::slice::from_ref(a), std::slice::from_ref(b))?,
            );
        }
    }
    let anchors: Vec<CommitId> = anchors.into_iter().collect();
    let expression = RevsetExpression::commits(anchors.clone())
        .range(&RevsetExpression::commits(spec.tips.clone()));
    let revset = expression.evaluate(repo)?;
    let ids: Vec<_> = block_on(revset.stream().collect::<Vec<_>>());
    let mut members: BTreeSet<CommitId> = ids.into_iter().collect::<Result<_, _>>()?;
    members.extend(anchors);
    members.extend(spec.parents.iter().cloned());
    let mut boundary = Vec::new();
    for id in &members {
        let commit = native::commit(repo, id)?;
        if commit
            .parent_ids()
            .iter()
            .any(|p| !members.contains(p) && p != repo.store().root_commit_id())
        {
            boundary.push(id.clone());
        }
    }
    Ok((members.into_iter().collect(), boundary))
}

/// The plan's glues, including new glues an earlier repair approved.
fn plan_glues(plan: &Plan) -> Vec<String> {
    plan.frozen
        .glues
        .iter()
        .chain(&plan.proposal.approved_new_glues)
        .cloned()
        .collect()
}

/// Series and glues whose proposal tips the task's parents contain.
fn dependencies(plan: &Plan, repo: &dyn JjRepo, spec: &Spec) -> Result<Vec<Dependency>> {
    let mut found = Vec::new();
    for name in plan.frozen.series.iter().chain(&plan_glues(plan)) {
        if *name == spec.subject {
            continue;
        }
        let Some(tip) = native::bookmark(repo, name)? else {
            continue;
        };
        let mut contained = false;
        for parent in &spec.parents {
            contained |= native::is_ancestor(repo, &tip, parent)?;
        }
        if contained {
            found.push(Dependency {
                subject: name.clone(),
                commit: tip.hex(),
            });
        }
    }
    Ok(found)
}

// Submitting tasks.

fn submit(
    repo: &Repo,
    config: &Config,
    dirs: &[PathBuf],
    save_plan: &Path,
    report_path: Option<&Path>,
) -> Result<i32> {
    let jj = Native::load(&repo.root)?;
    let repository = canonical(jj.repository_path())?;
    let mut tasks = Vec::new();
    for dir in dirs {
        let dir = canonical(dir)?;
        let task = artifact::load_task(&manifest_path(&dir), &repository)
            .with_context(|| format!("task {}", dir.display()))?;
        tasks.push((dir, task));
    }
    let plan = tasks[0].1.plan.clone();
    for (dir, task) in &tasks {
        ensure!(
            task.plan == plan,
            "task {} was started from a different plan; submit tasks of one plan together",
            dir.display()
        );
    }
    ensure_same_source(repo, &repository, &plan)?;
    let claims: Vec<Claim> = tasks
        .iter()
        .map(|(dir, task)| Claim::of(dir, task))
        .collect();
    let refusals = batch_conflicts(&claims, &config.fork.glue_prefix);
    if !refusals.is_empty() {
        for line in &refusals {
            report(&format!("refused: {line}"));
        }
        report("no successor plan saved");
        return Ok(EXIT_NEEDS_AGENT);
    }

    // A stale source refuses before anything is read from the tasks or imported.
    drop(workflow::load_proposal(repo, config, &plan)?);

    // Read every result once; everything after this works on the frozen ids.
    let mut results = Vec::new();
    let mut rejected = Vec::new();
    for (dir, task) in &tasks {
        let settings = task_settings(jj.repo.settings())?;
        match read_result(&settings, dir, task)? {
            Ok(commits) => results.push(commits),
            Err(problems) => {
                rejected.extend(
                    problems
                        .into_iter()
                        .map(|p| format!("{}: {p}", dir.display())),
                );
                results.push(Vec::new());
            }
        }
    }
    if !rejected.is_empty() {
        return Ok(reject(&rejected));
    }
    for ((dir, task), commits) in tasks.iter().zip(&results) {
        let mut include = commits.clone();
        let mut exclude = task.parent_ids()?;
        exclude.extend(
            task.objects
                .iter()
                .map(|id| native::parse_id(id))
                .collect::<Result<Vec<_>>>()?,
        );
        if task.kind == TaskKind::NewGlue {
            // The task created this conflicted merge; the result is compared against it.
            include.push(native::parse_id(&task.seed[0].commit)?);
        }
        copy_objects(
            &dir.join(".git"),
            Some(&repo.root),
            &repo.root.join(".git"),
            &include,
            &exclude,
        )
        .with_context(|| format!("failed to import the result of {}", dir.display()))?;
    }

    // The source binding is checked again; the imported objects are not in its view.
    let prepared = workflow::load_proposal(repo, config, &plan)?;
    let protected = protected_paths(&plan);
    let mut replacements = Vec::new();
    for ((dir, task), commits) in tasks.iter().zip(&results) {
        let problems = validate(prepared.as_ref(), task, commits, &protected)?;
        if problems.is_empty() {
            replacements.push(RepairReplacement {
                issue: task.issue.clone(),
                subject: task.subject.clone(),
                kind: task.kind.into(),
                commits: commits.clone(),
            });
        } else {
            rejected.extend(
                problems
                    .into_iter()
                    .map(|p| format!("{}: {p}", dir.display())),
            );
        }
    }
    if !rejected.is_empty() {
        return Ok(reject(&rejected));
    }
    drop(prepared);
    for replacement in &replacements {
        report(&format!(
            "accepted {} {} -> {}",
            kind_label(task_kind(replacement.kind)),
            replacement.subject,
            short(&replacement.commits.last().expect("validated").hex())
        ));
    }
    let next = crate::sync::rebuild_repaired(repo, config, &plan, &replacements)?;
    artifact::save_plan(save_plan, &repository, &next)?;
    for issue in &next.issues {
        report(&format!("{}: {}", issue.id, issue.message));
    }
    report(&format!(
        "saved successor plan {} ({}); nothing published. Publish with: jj fork apply {}",
        save_plan.display(),
        match next.outcome {
            PlanOutcome::Ready => "ready",
            PlanOutcome::Repair => "needs repair",
            PlanOutcome::Refused => "refused",
        },
        save_plan.display()
    ));
    if let Some(path) = report_path {
        artifact::save_report(path, &workflow::report_of(&next))?;
    }
    Ok(if next.outcome == PlanOutcome::Ready {
        EXIT_OK
    } else {
        EXIT_NEEDS_AGENT
    })
}

fn reject(problems: &[String]) -> i32 {
    for problem in problems {
        report(&format!("rejected {problem}"));
    }
    report("no successor plan saved; nothing imported into the source view");
    EXIT_NEEDS_AGENT
}

/// What one task in a batch replaces and builds on.
struct Claim<'a> {
    task: String,
    subject: &'a str,
    kind: TaskKind,
    dependencies: Vec<&'a str>,
}

impl<'a> Claim<'a> {
    fn of(dir: &Path, task: &'a Task) -> Claim<'a> {
        Claim {
            task: dir.display().to_string(),
            subject: &task.subject,
            kind: task.kind,
            dependencies: task
                .dependencies
                .iter()
                .map(|d| d.subject.as_str())
                .collect(),
        }
    }
}

/// Pairs of tasks that cannot be submitted together: the same destination, or one replacing
/// what another builds on (including a new glue that changes the fork's or an outer glue's
/// members). Such work is refused rather than silently reparented.
fn batch_conflicts(claims: &[Claim], glue_prefix: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (i, a) in claims.iter().enumerate() {
        for (j, b) in claims.iter().enumerate() {
            if i == j {
                continue;
            }
            if i < j && a.subject == b.subject {
                problems.push(format!(
                    "{} and {} both repair {}",
                    a.task, b.task, a.subject
                ));
            }
            let builds_on = b.dependencies.contains(&a.subject)
                || a.kind == TaskKind::NewGlue
                    && match b.kind {
                        TaskKind::Fork => true,
                        TaskKind::Glue | TaskKind::NewGlue => {
                            let inner = glue::names(a.subject, glue_prefix);
                            let outer = glue::names(b.subject, glue_prefix);
                            inner.len() < outer.len() && inner.is_subset(&outer)
                        }
                        TaskKind::Series => false,
                    };
            if builds_on {
                problems.push(format!(
                    "{} builds on {}, which {} replaces; submit {} first and start {} again from its successor plan",
                    b.task, a.subject, a.task, a.task, b.subject
                ));
            }
        }
    }
    problems
}

/// Reads `repair/result` from the task's single head operation and walks exactly the commits
/// the task may produce, so nothing beyond them is imported. Returns them parents first.
fn read_result(
    settings: &UserSettings,
    dir: &Path,
    task: &Task,
) -> Result<std::result::Result<Vec<CommitId>, Vec<String>>> {
    let workspace = load_task_workspace(settings, dir)?;
    let loader = workspace.repo_loader();
    let heads = block_on(loader.op_heads_store().get_op_heads())?;
    if heads.len() != 1 {
        return Ok(Err(vec![
            "the task has divergent operations; run any jj command in it to reconcile them".into(),
        ]));
    }
    let task_repo = block_on(loader.load_at_head())?;
    let target = task_repo
        .view()
        .get_local_bookmark(RefName::new(RESULT_BOOKMARK));
    let Some(tip) = target.as_normal().cloned() else {
        return Ok(Err(vec![format!(
            "{RESULT_BOOKMARK} must name exactly one commit"
        )]));
    };
    let parents = task.parent_ids()?;
    let mut chain = Vec::new();
    let mut id = tip;
    let length = task.seed.len();
    loop {
        let commit = native::commit(task_repo.as_ref(), &id)?;
        chain.push(id.clone());
        if task.kind != TaskKind::Series {
            if commit.parent_ids() != parents.as_slice() {
                return Ok(Err(vec![format!(
                    "{RESULT_BOOKMARK} {} must have exactly the parents {} (has {})",
                    short(&id.hex()),
                    hex_list(&parents),
                    hex_list(commit.parent_ids())
                )]));
            }
            break;
        }
        let [parent] = commit.parent_ids() else {
            return Ok(Err(vec![format!(
                "series commit {} has {} parents; a series stays linear",
                short(&id.hex()),
                commit.parent_ids().len()
            )]));
        };
        if *parent == parents[0] {
            break;
        }
        if chain.len() == length {
            return Ok(Err(vec![format!(
                "{RESULT_BOOKMARK} is not {length} commits on the target {}: a commit was added or the series was moved",
                short(&parents[0].hex())
            )]));
        }
        id = parent.clone();
    }
    if chain.len() != length {
        return Ok(Err(vec![format!(
            "{RESULT_BOOKMARK} has {} commits on the target, but the series has {length}: commits were dropped or squashed together",
            chain.len()
        )]));
    }
    chain.reverse();
    Ok(Ok(chain))
}

/// Validates imported results through the source store: exact parents, change identities,
/// metadata, conflicts, and every changed path and mode against the signed scope.
fn validate(
    repo: &dyn JjRepo,
    task: &Task,
    commits: &[CommitId],
    protected: &[String],
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    if commits
        .iter()
        .map(|id| id.hex())
        .eq(task.seed.iter().map(|s| s.commit.clone()))
    {
        problems.push(format!("{RESULT_BOOKMARK} is unchanged"));
        return Ok(problems);
    }
    let mut expected_parents = task.parent_ids()?;
    for (id, seed) in commits.iter().zip(&task.seed) {
        let result = native::commit(repo, id)?;
        let original = native::commit(repo, &native::parse_id(&seed.commit)?)?;
        let name = short(&id.hex()).to_string();
        if result.parent_ids() != expected_parents.as_slice() {
            problems.push(format!(
                "{name} has parents {}, expected exactly {}",
                hex_list(result.parent_ids()),
                hex_list(&expected_parents)
            ));
        }
        if result.change_id().hex() != seed.change {
            problems.push(format!(
                "{name} is change {}, expected {} at this position: commits were replaced, reordered, or squashed",
                result.change_id().hex(),
                seed.change
            ));
        }
        // jj refreshes the author timestamp when it rewrites an undescribed commit, so the
        // author's identity is compared, not the time.
        let (author, seeded) = (result.author(), original.author());
        if result.description() != original.description()
            || (&author.name, &author.email) != (&seeded.name, &seeded.email)
        {
            problems.push(format!(
                "{name} changes its description or author; commit metadata is not repairable here"
            ));
        }
        if result.has_conflict() {
            problems.push(format!("{name} still has conflicts"));
        }
        let mut diff = original
            .tree()
            .diff_stream(&result.tree(), &EverythingMatcher);
        while let Some(entry) = block_on(diff.next()) {
            let path = entry.path.as_internal_file_string().to_string();
            let values = entry.values?;
            if let Err(reason) =
                check_change(&path, &values.before, &values.after, &task.scope, protected)
            {
                problems.push(format!("{name} {reason}"));
            }
        }
        expected_parents = vec![id.clone()];
    }
    Ok(problems)
}

/// Whether one changed path is within the task's authority. The new value must be resolved;
/// a regular file keeps an executable bit the seed already had (new files are not executable),
/// and symlinks and submodules may only take a value one of the seed's sides already had.
fn check_change(
    path: &str,
    before: &MergedTreeValue,
    after: &MergedTreeValue,
    scope: &[String],
    protected: &[String],
) -> std::result::Result<(), String> {
    if is_protected(path, protected) {
        return Err(format!("changes protected path {path}"));
    }
    if !in_scope(path, scope) {
        return Err(format!("changes {path}, outside the task's scope"));
    }
    let Some(value) = after.as_resolved() else {
        // Reported once as an unresolved conflict for the commit.
        return Ok(());
    };
    let sides: Vec<&TreeValue> = before.iter().flatten().collect();
    match value {
        None => Ok(()),
        Some(value) if sides.contains(&value) => Ok(()),
        Some(TreeValue::File { executable, .. }) => {
            let allowed: BTreeSet<bool> = sides
                .iter()
                .filter_map(|side| match side {
                    TreeValue::File { executable, .. } => Some(*executable),
                    _ => None,
                })
                .collect();
            if allowed.contains(executable) || allowed.is_empty() && !executable {
                Ok(())
            } else {
                Err(format!("changes the mode of {path}"))
            }
        }
        Some(TreeValue::Symlink(_)) => Err(format!("makes {path} a new symlink target")),
        Some(TreeValue::GitSubmodule(_)) => Err(format!("points submodule {path} at a new commit")),
        Some(TreeValue::Tree(_)) => Err(format!("writes an unexpected tree at {path}")),
    }
}

// Paths.

/// Normalizes a repository path: relative, `/`-separated, without empty, `.`, or `..`
/// components. One trailing `/` is accepted.
fn normalize_path(path: &str) -> std::result::Result<String, String> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Err("the whole repository cannot be a repair scope".into());
    }
    if trimmed.starts_with('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err("must be a relative path inside the repository".into());
    }
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err("must not contain empty, '.', or '..' components".into());
        }
    }
    Ok(trimmed.to_string())
}

/// Files a task may never change: the jj-fork configuration and Git metadata files. Any
/// `.git` or `.jj` component is also protected (see `is_protected`).
fn protected_paths(plan: &Plan) -> Vec<String> {
    let mut paths = vec![
        config::FILE_NAME.to_string(),
        ".gitmodules".into(),
        ".gitattributes".into(),
    ];
    if let Ok(relative) = plan
        .context
        .config_path
        .strip_prefix(&plan.frozen.workspace)
        && let Some(relative) = relative.to_str()
    {
        paths.push(relative.replace(std::path::MAIN_SEPARATOR, "/"));
    }
    paths
}

fn is_protected(path: &str, protected: &[String]) -> bool {
    protected.iter().any(|p| p == path)
        || path
            .split('/')
            .any(|c| c.eq_ignore_ascii_case(".git") || c.eq_ignore_ascii_case(".jj"))
}

fn in_scope(path: &str, scope: &[String]) -> bool {
    scope.iter().any(|s| {
        path == s
            || path
                .strip_prefix(s.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

// Repositories and objects.

fn ensure_same_source(repo: &Repo, repository: &Path, plan: &Plan) -> Result<()> {
    ensure!(
        canonical(&repo.root)? == plan.frozen.workspace && repository == plan.frozen.repository,
        "plan belongs to another repository or workspace"
    );
    Ok(())
}

/// A new or empty directory for a task, outside the source workspace and not containing it.
fn new_task_dir(dir: &Path, source_root: &Path) -> Result<PathBuf> {
    if dir.exists() {
        ensure!(
            dir.is_dir() && std::fs::read_dir(dir)?.next().is_none(),
            "{} exists and is not an empty directory",
            dir.display()
        );
    } else {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let dir = canonical(dir)?;
    let source = canonical(source_root)?;
    ensure!(
        !dir.starts_with(&source) && !source.starts_with(&dir),
        "the task directory must be outside the source workspace (and not contain it)"
    );
    Ok(dir)
}

/// The source's effective settings for writing task commits, with signing off and change ids
/// always written into Git headers, so they survive the trip back into the source. Nothing of
/// them is written into the task.
fn task_settings(source: &UserSettings) -> Result<UserSettings> {
    let mut config = source.config().clone();
    let layer = ConfigLayer::parse(
        ConfigSource::CommandArg,
        "signing.backend = \"none\"\nsigning.behavior = \"drop\"\ngit.write-change-id-header = true\n",
    )?;
    config.add_layer(layer);
    Ok(UserSettings::from_config(config)?)
}

fn load_task_workspace(settings: &UserSettings, dir: &Path) -> Result<Workspace> {
    Workspace::load(
        settings,
        dir,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .with_context(|| format!("{} is not a jj repair task", dir.display()))
}

/// Copies `include` commits with their trees (and, unless `exclude` is empty, only objects not
/// reachable from `exclude`) from the Git repository at `from` into the one at `to`. Git runs in
/// `read_in` (the source) with `from`'s objects as an alternate, so only the source's trusted
/// Git configuration is used; with `read_in` None, Git runs in `from` itself.
fn copy_objects(
    from: &Path,
    read_in: Option<&Path>,
    to: &Path,
    include: &[CommitId],
    exclude: &[CommitId],
) -> Result<()> {
    let git = |dir: &Path| {
        let mut command = Process::new("git");
        command.arg("-C").arg(dir);
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(var);
        }
        command
    };
    let (dir, alternate) = match read_in {
        Some(dir) => (dir.to_path_buf(), Some(from.join("objects"))),
        None => (from.to_path_buf(), None),
    };
    let mut revs = String::new();
    for id in include {
        revs.push_str(&format!("{}\n", id.hex()));
    }
    let mut list = git(&dir);
    list.args(["rev-list", "--objects", "--stdin"]);
    if exclude.is_empty() {
        list.arg("--no-walk");
    } else {
        revs.push_str("--not\n");
        for id in exclude {
            revs.push_str(&format!("{}\n", id.hex()));
        }
    }
    let objects = pipe_through(list, alternate.as_deref(), revs.as_bytes())?;
    let mut pack = git(&dir);
    pack.args(["pack-objects", "--stdout", "-q"]);
    let pack = pipe_through(pack, alternate.as_deref(), &objects)?;
    let mut index = git(to);
    index.args(["index-pack", "--stdin"]);
    pipe_through(index, None, &pack)?;
    Ok(())
}

fn pipe_through(mut command: Process, alternate: Option<&Path>, input: &[u8]) -> Result<Vec<u8>> {
    if let Some(alternate) = alternate {
        command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start git")?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| anyhow!("git input writer panicked"))??;
    ensure!(
        output.status.success(),
        "git object transfer failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))
}

fn sorted_hex(ids: &[CommitId]) -> Vec<String> {
    let mut hex: Vec<String> = ids.iter().map(|id| id.hex()).collect();
    hex.sort();
    hex
}

fn hex_list(ids: &[CommitId]) -> String {
    ids.iter()
        .map(|id| short(&id.hex()).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn task_kind(kind: RepairKind) -> TaskKind {
    match kind {
        RepairKind::Series => TaskKind::Series,
        RepairKind::Glue => TaskKind::Glue,
        RepairKind::NewGlue => TaskKind::NewGlue,
        RepairKind::Fork => TaskKind::Fork,
    }
}

fn kind_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Series => "series",
        TaskKind::Glue => "glue",
        TaskKind::NewGlue => "new glue",
        TaskKind::Fork => "fork merge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_paths_reject_traversal_and_the_whole_repository() {
        assert_eq!(normalize_path("src/lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(normalize_path("src/").unwrap(), "src");
        for bad in [
            "",
            "/",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a\\b",
            "a/.",
        ] {
            assert!(normalize_path(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn scope_matches_whole_components_only() {
        let scope = vec!["src/a".to_string(), "README".to_string()];
        assert!(in_scope("src/a", &scope));
        assert!(in_scope("src/a/b.rs", &scope));
        assert!(
            !in_scope("src/ab.rs", &scope),
            "a prefix of a name is not a directory"
        );
        assert!(!in_scope("src", &scope));
        assert!(!in_scope("README.md", &scope));
    }

    fn claim<'a>(subject: &'a str, kind: TaskKind, dependencies: &[&'a str]) -> Claim<'a> {
        Claim {
            task: format!("task-{subject}"),
            subject,
            kind,
            dependencies: dependencies.to_vec(),
        }
    }

    #[test]
    fn independent_series_batch_but_shared_or_built_on_subjects_do_not() {
        let a = claim("patch/a", TaskKind::Series, &[]);
        let b = claim("patch/b", TaskKind::Series, &[]);
        assert!(batch_conflicts(&[a, b], "glue/").is_empty());

        let twice = [
            claim("patch/a", TaskKind::Series, &[]),
            claim("patch/a", TaskKind::Series, &[]),
        ];
        assert_eq!(
            batch_conflicts(&twice, "glue/").len(),
            1,
            "overlap reported once"
        );

        // The glue's recorded parents contain patch/a's current tip, which the other task replaces.
        let stale = [
            claim("glue/a+b", TaskKind::Glue, &["patch/a", "patch/b"]),
            claim("patch/a", TaskKind::Series, &[]),
        ];
        let problems = batch_conflicts(&stale, "glue/");
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].starts_with("task-glue/a+b builds on patch/a"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_new_glue_changes_the_fork_and_outer_glues_but_not_unrelated_work() {
        let new = || claim("glue/a+b", TaskKind::NewGlue, &["patch/a", "patch/b"]);
        let fork = claim(
            "fork/main",
            TaskKind::Fork,
            &["patch/a", "patch/b", "patch/c"],
        );
        assert_eq!(batch_conflicts(&[new(), fork], "glue/").len(), 1);
        let outer = claim("glue/a+b+c", TaskKind::Glue, &["patch/c"]);
        assert_eq!(batch_conflicts(&[new(), outer], "glue/").len(), 1);
        let sibling = claim("glue/a+c", TaskKind::Glue, &["patch/c"]);
        assert!(batch_conflicts(&[new(), sibling], "glue/").is_empty());
        let series = claim("patch/c", TaskKind::Series, &[]);
        assert!(batch_conflicts(&[new(), series], "glue/").is_empty());
    }

    fn file(byte: u8, executable: bool) -> Option<TreeValue> {
        Some(TreeValue::File {
            id: jj_lib::backend::FileId::new(vec![byte; 20]),
            executable,
            copy_id: jj_lib::backend::CopyId::placeholder(),
        })
    }

    #[test]
    fn changed_paths_keep_scope_kinds_and_modes() {
        use jj_lib::merge::Merge;
        let scope = vec!["src".to_string()];
        let protected = vec![".jj-fork.toml".to_string()];
        let check = |path: &str, before: Merge<Option<TreeValue>>, after: Option<TreeValue>| {
            check_change(path, &before, &Merge::resolved(after), &scope, &protected)
        };
        // A text resolution of a conflict between two regular files.
        let conflict = Merge::from_vec(vec![file(1, false), file(0, false), file(2, false)]);
        assert!(check("src/a", conflict.clone(), file(3, false)).is_ok());
        assert!(
            check("src/a", conflict.clone(), None).is_ok(),
            "deletion inside scope"
        );
        assert!(
            check("src/a", conflict, file(3, true)).is_err(),
            "new executable bit"
        );
        // An executable side may stay executable; a new file may not become one.
        let mixed = Merge::from_vec(vec![file(1, true), file(0, false), file(2, false)]);
        assert!(check("src/a", mixed, file(3, true)).is_ok());
        assert!(check("src/new", Merge::absent(), file(3, false)).is_ok());
        assert!(check("src/new", Merge::absent(), file(3, true)).is_err());
        // Symlinks and submodules only take a value a side already had.
        let link = |b: u8| {
            Some(TreeValue::Symlink(jj_lib::backend::SymlinkId::new(vec![
                b;
                20
            ])))
        };
        assert!(check("src/l", Merge::resolved(link(1)), link(1)).is_ok());
        assert!(check("src/l", Merge::resolved(file(1, false)), link(2)).is_err());
        let module = |b: u8| Some(TreeValue::GitSubmodule(CommitId::new(vec![b; 20])));
        assert!(
            check(
                "src/m",
                Merge::from_vec(vec![module(1), module(0), module(2)]),
                module(2)
            )
            .is_ok()
        );
        assert!(check("src/m", Merge::resolved(module(1)), module(9)).is_err());
        // Scope and protection apply to every changed endpoint, including deletions.
        assert!(check("other/a", Merge::resolved(file(1, false)), None).is_err());
        assert!(
            check(
                "src/x/.git/hooks/pre-commit",
                Merge::absent(),
                file(1, false)
            )
            .is_err()
        );
        assert!(check("srcx", Merge::absent(), file(1, false)).is_err());
    }

    #[test]
    fn metadata_paths_are_protected_at_any_depth() {
        let protected = vec![".jj-fork.toml".to_string(), ".gitmodules".into()];
        assert!(is_protected(".jj-fork.toml", &protected));
        assert!(is_protected("vendor/.git/config", &protected));
        assert!(is_protected("x/.JJ/repo", &protected));
        assert!(!is_protected("docs/.jj-fork.toml", &protected));
        assert!(!is_protected(".github/workflows/ci.yml", &protected));
    }
}
