//! Frozen source contracts and executable saved proposals. Artifacts never carry executable
//! shell commands: apply reloads the source's trusted policy and reruns checks on exact IDs.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OperationId, RefTarget};
use jj_lib::repo::{ReadonlyRepo, Repo as JjRepo};
use serde::{Deserialize, Serialize};

use crate::artifact;
use crate::checks::{CheckRecord, Checker, Outcome};
use crate::config::Config;
use crate::native::{self, Native, Publication};
use crate::repo::Repo;
use crate::sync::{EXIT_NEEDS_AGENT, EXIT_OK};
use crate::{progress, report, run};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOutcome {
    Ready,
    Repair,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub id: String,
    pub code: String,
    pub subject: String,
    pub candidate: Option<String>,
    pub tier: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub command: String,
    pub config_path: PathBuf,
    pub target_expression: Option<String>,
    pub candidate_expression: Option<String>,
    pub fetched: bool,
    pub checks_enabled: bool,
}

/// Alternating add/remove/add terms retain nulls and conflict structure, not only added IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub terms: Vec<Option<String>>,
}
impl Target {
    fn of(target: &RefTarget) -> Self {
        Self {
            terms: target
                .as_merge()
                .iter()
                .map(|id| id.as_ref().map(|id| id.hex()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRef {
    pub remote: String,
    pub name: String,
    pub tag: bool,
    pub tracked: bool,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewState {
    pub heads: Vec<String>,
    pub bookmarks: BTreeMap<String, Target>,
    pub tags: BTreeMap<String, Target>,
    pub remotes: Vec<RemoteRef>,
    pub git_refs: BTreeMap<String, Target>,
    pub git_head: Target,
    pub workspaces: BTreeMap<String, String>,
}
impl ViewState {
    pub fn capture(repo: &dyn JjRepo) -> Self {
        let v = repo.view().store_view();
        let mut heads: Vec<_> = v.head_ids.iter().map(|id| id.hex()).collect();
        heads.sort();
        let mut remotes = Vec::new();
        for (remote, view) in &v.remote_views {
            for (tag, refs) in [(false, &view.bookmarks), (true, &view.tags)] {
                for (name, r) in refs {
                    remotes.push(RemoteRef {
                        remote: remote.as_str().into(),
                        name: name.as_str().into(),
                        tag,
                        tracked: r.is_tracked(),
                        target: Target::of(&r.target),
                    });
                }
            }
        }
        Self {
            heads,
            bookmarks: v
                .local_bookmarks
                .iter()
                .map(|(n, t)| (n.as_str().into(), Target::of(t)))
                .collect(),
            tags: v
                .local_tags
                .iter()
                .map(|(n, t)| (n.as_str().into(), Target::of(t)))
                .collect(),
            remotes,
            git_refs: v
                .git_refs
                .iter()
                .map(|(n, t)| (n.as_str().into(), Target::of(t)))
                .collect(),
            git_head: Target::of(&v.git_head),
            workspaces: v
                .wc_commit_ids
                .iter()
                .map(|(n, id)| (n.as_str().into(), id.hex()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitState {
    pub refs: String,
    pub head: String,
    pub symbolic_head: String,
    pub index: String,
    pub tracked_diff: String,
    pub policy: String,
}
impl GitState {
    fn capture(repo: &Repo) -> Result<Self> {
        Ok(Self {
            refs: repo.git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ])?,
            head: repo.git(&["rev-parse", "HEAD"])?,
            symbolic_head: run::output_all(&repo.root, "git", &["symbolic-ref", "-q", "HEAD"])?,
            index: repo.git(&["ls-files", "--stage"])?,
            tracked_diff: artifact::fingerprint(&repo.git(&["diff", "--binary", "HEAD", "--"])?)?,
            policy: artifact::fingerprint(&repo.git(&["config", "--null", "--list"])?)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub commit: String,
    pub change: String,
    pub parents: Vec<String>,
    pub tree_terms: Vec<String>,
    pub tree_labels: Vec<String>,
    pub conflicted: bool,
}
impl Candidate {
    pub fn of(commit: &Commit) -> Self {
        let tree = commit.tree();
        Self {
            commit: commit.id().hex(),
            change: commit.change_id().hex(),
            parents: commit.parent_ids().iter().map(|id| id.hex()).collect(),
            tree_terms: tree.tree_ids().iter().map(|id| id.hex()).collect(),
            tree_labels: tree.labels().as_slice().to_vec(),
            conflicted: commit.has_conflict(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenInputs {
    pub repository: PathBuf,
    pub workspace: PathBuf,
    pub workspace_name: String,
    pub base_operation: String,
    pub operation_heads: Vec<String>,
    pub working_copy: Candidate,
    pub target: String,
    pub candidate: Option<String>,
    pub series: Vec<String>,
    pub glues: Vec<String>,
    pub view: ViewState,
    pub git: GitState,
    pub policy_fingerprint: String,
    pub config_file_fingerprint: String,
}
impl FrozenInputs {
    pub fn capture(
        repo: &Repo,
        jj: &Native,
        config: &Config,
        context: &Context,
        target: &CommitId,
        candidate: Option<&CommitId>,
    ) -> Result<Self> {
        Ok(Self {
            repository: std::fs::canonicalize(jj.repository_path())?,
            workspace: std::fs::canonicalize(&repo.root)?,
            workspace_name: jj.workspace_name().as_str().into(),
            base_operation: jj.repo.op_id().hex(),
            operation_heads: jj.op_heads().iter().map(|op| op.hex()).collect(),
            working_copy: Candidate::of(&native::commit(jj.repo.as_ref(), &jj.wc_commit)?),
            target: target.hex(),
            candidate: candidate.map(|id| id.hex()),
            series: native::bookmarks_with(jj.repo.as_ref(), &config.fork.series_prefixes),
            glues: native::bookmarks_with(
                jj.repo.as_ref(),
                std::slice::from_ref(&config.fork.glue_prefix),
            ),
            view: ViewState::capture(jj.repo.as_ref()),
            git: GitState::capture(repo)?,
            policy_fingerprint: policy_fingerprint(jj, config)?,
            config_file_fingerprint: artifact::fingerprint(&std::fs::read(&context.config_path)?)?,
        })
    }
    pub fn revalidate(
        &self,
        repo: &Repo,
        jj: &mut Native,
        config: &Config,
        context: &Context,
    ) -> Result<()> {
        let now = Self::capture(
            repo,
            jj,
            config,
            context,
            &native::parse_id(&self.target)?,
            self.candidate
                .as_deref()
                .map(native::parse_id)
                .transpose()?
                .as_ref(),
        )?;
        ensure!(
            self == &now,
            "saved plan is stale: source operation, workspace, configuration, membership, tracking or Git inputs changed"
        );
        if let Some(reason) = jj.verify_frozen()? {
            anyhow::bail!("saved plan is stale: {reason}");
        }
        Ok(())
    }
}

fn policy_fingerprint(jj: &Native, config: &Config) -> Result<String> {
    artifact::fingerprint(&(config, jj.effective_settings()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub original: String,
    pub copy: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckTarget {
    pub subject: String,
    pub candidate: String,
    pub patch: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookmarkChange {
    pub name: String,
    pub before: Option<Target>,
    pub after: Option<Target>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub operation: Option<String>,
    pub view: ViewState,
    pub candidates: Vec<Candidate>,
    pub mappings: Vec<Mapping>,
    pub checks: Vec<CheckTarget>,
    pub bookmark_changes: Vec<BookmarkChange>,
    pub approved_new_glues: Vec<String>,
    pub workspace_after: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub context: Context,
    pub frozen: FrozenInputs,
    pub proposal: Proposal,
    pub outcome: PlanOutcome,
    pub issues: Vec<Issue>,
    pub checks: Vec<CheckRecord>,
}

#[derive(Serialize)]
pub struct Report {
    pub plan: Option<Plan>,
    pub diagnostics: Vec<Issue>,
    pub exit_code: i32,
    pub published_operation: Option<String>,
    pub push: Option<native::PushReport>,
}

impl Plan {
    pub fn validate_ids(&self) -> Result<()> {
        let check = |id: &str, length: usize| -> Result<()> {
            ensure!(
                id.len() == length
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "malformed artifact object id"
            );
            Ok(())
        };
        check(&self.frozen.base_operation, 128)?;
        for op in &self.frozen.operation_heads {
            check(op, 128)?;
        }
        if let Some(op) = &self.proposal.operation {
            check(op, 128)?;
        }
        check(&self.frozen.target, 40)?;
        if let Some(id) = &self.frozen.candidate {
            check(id, 40)?;
        }
        for candidate in self
            .proposal
            .candidates
            .iter()
            .chain(std::iter::once(&self.frozen.working_copy))
        {
            check(&candidate.commit, 40)?;
            check(&candidate.change, 32)?;
            for id in candidate.parents.iter().chain(&candidate.tree_terms) {
                check(id, 40)?;
            }
        }
        for mapping in &self.proposal.mappings {
            check(&mapping.original, 40)?;
            check(&mapping.copy, 40)?;
        }
        for target in &self.proposal.checks {
            check(&target.candidate, 40)?;
        }
        check(&self.proposal.workspace_after, 40)?;
        for view in [&self.frozen.view, &self.proposal.view] {
            for id in view.heads.iter().chain(view.workspaces.values()) {
                check(id, 40)?;
            }
            for target in view
                .bookmarks
                .values()
                .chain(view.tags.values())
                .chain(view.git_refs.values())
                .chain(std::iter::once(&view.git_head))
                .chain(view.remotes.iter().map(|r| &r.target))
            {
                ensure!(
                    !target.terms.is_empty() && target.terms.len() % 2 == 1,
                    "malformed ref target"
                );
                for id in target.terms.iter().flatten() {
                    check(id, 40)?;
                }
            }
        }
        for change in &self.proposal.bookmark_changes {
            for target in change.before.iter().chain(&change.after) {
                ensure!(
                    !target.terms.is_empty() && target.terms.len() % 2 == 1,
                    "malformed bookmark target"
                );
                for id in target.terms.iter().flatten() {
                    check(id, 40)?;
                }
            }
        }
        for record in &self.checks {
            check(&record.candidate, 40)?;
            check(&record.target, 40)?;
        }
        for issue in &self.issues {
            if let Some(id) = &issue.candidate {
                check(id, 40)?;
            }
        }
        Ok(())
    }
}

pub fn proposal(
    base: &dyn JjRepo,
    prepared: &dyn JjRepo,
    workspace: &str,
    mappings: Vec<Mapping>,
    checks: Vec<CheckTarget>,
    operation: Option<String>,
) -> Result<Proposal> {
    let before = ViewState::capture(base);
    let after = ViewState::capture(prepared);
    let names: BTreeSet<_> = before
        .bookmarks
        .keys()
        .chain(after.bookmarks.keys())
        .cloned()
        .collect();
    let bookmark_changes = names
        .into_iter()
        .filter(|name| before.bookmarks.get(name) != after.bookmarks.get(name))
        .map(|name| BookmarkChange {
            before: before.bookmarks.get(&name).cloned(),
            after: after.bookmarks.get(&name).cloned(),
            name,
        })
        .collect();
    let mut candidates = BTreeMap::new();
    let mut todo: Vec<_> = after
        .heads
        .iter()
        .chain(checks.iter().map(|c| &c.candidate))
        .chain(mappings.iter().map(|m| &m.copy))
        .map(|id| native::parse_id(id))
        .collect::<Result<_>>()?;
    while let Some(id) = todo.pop() {
        if candidates.contains_key(&id.hex()) {
            continue;
        }
        let commit = native::commit(prepared, &id)?;
        if !base.index().has_id(&id)? {
            todo.extend(commit.parent_ids().iter().cloned());
            candidates.insert(id.hex(), Candidate::of(&commit));
        } else if checks.iter().any(|c| c.candidate == id.hex()) {
            candidates.insert(id.hex(), Candidate::of(&commit));
        }
    }
    Ok(Proposal {
        operation,
        workspace_after: after
            .workspaces
            .get(workspace)
            .context("prepared workspace is absent")?
            .clone(),
        view: after,
        candidates: candidates.into_values().collect(),
        mappings,
        checks,
        bookmark_changes,
        approved_new_glues: Vec::new(),
    })
}

/// Validate the exact saved graph and the narrow view delta. No wildcard scope is accepted.
pub fn validate_proposal(
    plan: &Plan,
    base: &dyn JjRepo,
    prepared: &dyn JjRepo,
    config: &Config,
) -> Result<()> {
    ensure!(
        ViewState::capture(prepared) == plan.proposal.view,
        "prepared operation/view does not match plan"
    );
    let allowed: BTreeSet<_> = plan
        .frozen
        .series
        .iter()
        .chain(&plan.frozen.glues)
        .chain(&plan.proposal.approved_new_glues)
        .chain(std::iter::once(&config.fork.branch))
        .chain(config.fork.mirror_branch.iter())
        .collect();
    let mut expected = plan.frozen.view.clone();
    for change in &plan.proposal.bookmark_changes {
        ensure!(
            allowed.contains(&change.name),
            "plan contains an unapproved bookmark change"
        );
        ensure!(
            expected.bookmarks.get(&change.name) == change.before.as_ref(),
            "plan bookmark base mismatch"
        );
        match &change.after {
            Some(target) => {
                expected
                    .bookmarks
                    .insert(change.name.clone(), target.clone());
            }
            None => {
                expected.bookmarks.remove(&change.name);
            }
        }
    }
    expected.workspaces.insert(
        plan.frozen.workspace_name.clone(),
        plan.proposal.workspace_after.clone(),
    );
    expected.heads = plan.proposal.view.heads.clone();
    ensure!(
        expected == plan.proposal.view,
        "proposal changes tags, remotes, tracking, Git state or another workspace"
    );
    let mut ids = BTreeSet::new();
    for c in &plan.proposal.candidates {
        let actual = Candidate::of(&native::commit(prepared, &native::parse_id(&c.commit)?)?);
        ensure!(
            &actual == c && ids.insert(c.commit.clone()),
            "saved candidate metadata mismatch or duplicate"
        );
        if plan.outcome == PlanOutcome::Ready {
            ensure!(!c.conflicted, "ready plan contains a conflicted candidate");
        }
    }
    for head in &plan.proposal.view.heads {
        ensure!(
            base.index().has_id(&native::parse_id(head)?)? || ids.contains(head),
            "unapproved graph head"
        );
    }
    for head in &plan.frozen.view.heads {
        if !plan.proposal.view.heads.contains(head) {
            let id = native::parse_id(head)?;
            ensure!(
                plan.proposal
                    .view
                    .heads
                    .iter()
                    .any(|new| native::parse_id(new)
                        .and_then(|new| native::is_ancestor(prepared, &id, &new))
                        .unwrap_or(false)),
                "proposal discards a source graph head"
            );
        }
    }
    for change in &plan.proposal.bookmark_changes {
        if let Some(target) = &change.after {
            ensure!(
                target.terms.len() == 1 && target.terms[0].is_some(),
                "publication target must be concrete"
            );
        }
    }
    if plan.outcome == PlanOutcome::Ready {
        ensure!(
            matches!(
                plan.context.command.as_str(),
                "sync" | "assemble" | "retire"
            ) && plan.context.checks_enabled,
            "plan was not built with mandatory checks"
        );
        ensure!(
            plan.proposal
                .checks
                .iter()
                .any(|t| !t.patch && t.subject == config.fork.branch),
            "ready plan lacks required fork checks"
        );
        let mut subjects = BTreeSet::new();
        for target in &plan.proposal.checks {
            ensure!(subjects.insert(&target.subject), "duplicate check target");
            ensure!(
                if target.patch {
                    plan.frozen.series.contains(&target.subject)
                } else {
                    target.subject == config.fork.branch
                },
                "check target outside approved scope"
            );
            ensure!(
                plan.proposal.view.bookmarks.get(&target.subject)
                    == Some(&Target {
                        terms: vec![Some(target.candidate.clone())]
                    }),
                "check target is not the intended publication target"
            );
        }
    }
    Ok(())
}

/// Apply loads no alternate candidate and performs no preparation/snapshot/fetch publication.
pub fn apply(
    repo: &Repo,
    file: &Path,
    config_override: Option<&Path>,
    push: bool,
    report_path: Option<&Path>,
) -> Result<i32> {
    match apply_authenticated(repo, file, config_override, push, report_path) {
        Ok(code) => Ok(code),
        Err(error) => {
            if let Some(path) = report_path {
                artifact::save_report(
                    path,
                    &Report {
                        plan: None,
                        diagnostics: vec![Issue {
                            id: "invalid_artifact:apply".into(),
                            code: "invalid_artifact".into(),
                            subject: "apply".into(),
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
            Err(error)
        }
    }
}

fn apply_authenticated(
    repo: &Repo,
    file: &Path,
    config_override: Option<&Path>,
    push: bool,
    report_path: Option<&Path>,
) -> Result<i32> {
    let mut jj = Native::load(&repo.root)?;
    let initial_operation = jj.repo.op_id().hex();
    let mut plan = artifact::load_plan(file, jj.repository_path())?;
    ensure!(
        std::fs::canonicalize(&repo.root)? == plan.frozen.workspace
            && std::fs::canonicalize(jj.repository_path())? == plan.frozen.repository,
        "plan belongs to another repository/workspace"
    );
    if let Some(path) = config_override {
        ensure!(
            std::fs::canonicalize(path)? == plan.context.config_path,
            "apply cannot use --config {}: the plan was saved with {}. Omit --config to use the saved configuration, or prepare a new plan with the intended file",
            path.display(),
            plan.context.config_path.display()
        );
    }
    let config = Config::load(&repo.root, Some(&plan.context.config_path))?;
    let execute = |jj: &mut Native,
                   plan: &mut Plan|
     -> Result<(i32, Option<String>, Option<native::PushReport>)> {
        plan.frozen.revalidate(repo, jj, &config, &plan.context)?;
        probe_remotes(jj, &config, &plan.frozen.view)?;
        let op = OperationId::try_from_hex(
            plan.proposal
                .operation
                .as_deref()
                .context("plan has no prepared operation")?,
        )
        .context("malformed prepared operation")?;
        let prepared = jj
            .load_prepared_operation(&op)
            .context("saved plan expired: prepared operation is unavailable")?;
        ensure!(
            prepared.operation().parent_ids() == [jj.repo.op_id().clone()],
            "prepared operation has the wrong base"
        );
        validate_proposal(plan, jj.repo.as_ref(), prepared.as_ref(), &config)
            .context("invalid or expired saved proposal")?;
        if plan.outcome != PlanOutcome::Ready {
            for issue in &plan.issues {
                report(&format!("{}: {}", issue.code, issue.message));
            }
            report(
                "saved plan is not ready; repair must create a successor plan; nothing published",
            );
            return Ok((EXIT_NEEDS_AGENT, None, None));
        }
        let scratch = tempfile::Builder::new()
            .prefix("jj-fork-apply.")
            .tempdir()?;
        let logs = std::env::temp_dir().join("jj-fork-logs").join(format!(
            "apply-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&logs)?;
        let mut checker = Checker::new(
            repo,
            &config,
            plan.frozen.target.clone(),
            scratch.path().into(),
            logs,
        );
        for (ordinal, target) in plan.proposal.checks.iter().enumerate() {
            let id = native::parse_id(&target.candidate)?;
            let c = native::commit(prepared.as_ref(), &id).context("saved candidate expired")?;
            ensure!(
                !c.has_conflict(),
                "ready plan has a conflicted check target"
            );
            progress(&format!(
                "rechecking {} candidate {}",
                target.subject,
                id.hex()
            ));
            let worktree =
                repo.worktree(scratch.path(), &format!("candidate-{ordinal}"), &id.hex())?;
            let checks = if target.patch {
                &config.checks.patch
            } else {
                &config.checks.fork
            };
            if let Outcome::Fail { check, tier, log } =
                checker.run(&worktree.path, checks, target.patch, &target.subject)?
            {
                plan.checks = checker.records;
                plan.outcome = PlanOutcome::Refused;
                plan.issues.push(Issue {
                    id: format!("check_failed:{}", target.subject),
                    code: "check_failed".into(),
                    subject: target.subject.clone(),
                    candidate: Some(id.hex()),
                    tier: Some(tier),
                    message: format!("fails {check} checks; log: {}", log.display()),
                });
                report("saved candidate failed required rechecks; nothing published");
                return Ok((EXIT_NEEDS_AGENT, None, None));
            }
        }
        plan.checks = checker.records;
        // Reload the source, not the proposal, after arbitrary trusted check commands ran.
        let mut fresh = Native::load(&repo.root)?;
        plan.frozen
            .revalidate(repo, &mut fresh, &config, &plan.context)?;
        let current_config = Config::load(&repo.root, Some(&plan.context.config_path))?;
        ensure!(
            policy_fingerprint(&fresh, &current_config)? == plan.frozen.policy_fingerprint,
            "check policy changed during apply"
        );
        probe_remotes(&fresh, &config, &plan.frozen.view)?;
        if let Some(path) = report_path {
            artifact::save_report(
                path,
                &Report {
                    plan: Some(plan.clone()),
                    diagnostics: Vec::new(),
                    exit_code: EXIT_OK,
                    published_operation: None,
                    push: None,
                },
            )?;
        }
        match jj.publish_prepared(prepared, "jj-fork apply saved plan")? {
            Publication::Stale(reason) => {
                report(&format!("{reason}; nothing published"));
                plan.outcome = PlanOutcome::Refused;
                plan.issues.push(Issue {
                    id: "stale_source:apply".into(),
                    code: "stale_source".into(),
                    subject: "apply".into(),
                    candidate: None,
                    tier: None,
                    message: reason,
                });
                Ok((EXIT_NEEDS_AGENT, None, None))
            }
            Publication::Done(op) => {
                let published = Some(op.hex());
                report(&format!(
                    "applied saved plan; published operation {}",
                    op.hex()
                ));
                let pushed = if push {
                    crate::sync::push_saved(repo, jj, &config, plan)?
                } else {
                    None
                };
                let code = match (&pushed, push) {
                    (Some(r), true) if !r.all_accepted() => 1,
                    (None, true) => EXIT_NEEDS_AGENT,
                    _ => EXIT_OK,
                };
                Ok((code, published, pushed))
            }
        }
    };
    let result = execute(&mut jj, &mut plan);
    let (code, published, pushed) = match result {
        Ok(result) => result,
        Err(err) => {
            // Publication errors explicitly preserve published state. Do not restore or push.
            report(&format!("saved plan stopped: {err:#}"));
            plan.outcome = PlanOutcome::Refused;
            plan.issues.push(Issue {
                id: "apply_refused:apply".into(),
                code: "apply_refused".into(),
                subject: "apply".into(),
                candidate: None,
                tier: None,
                message: format!("{err:#}"),
            });
            let published =
                (jj.repo.op_id().hex() != initial_operation).then(|| jj.repo.op_id().hex());
            (
                if published.is_some() {
                    1
                } else {
                    EXIT_NEEDS_AGENT
                },
                published,
                None,
            )
        }
    };
    if let Some(path) = report_path {
        artifact::save_report(
            path,
            &Report {
                diagnostics: plan.issues.clone(),
                plan: Some(plan),
                exit_code: code,
                published_operation: published,
                push: pushed,
            },
        )?;
    }
    Ok(code)
}

pub fn probe_remotes(jj: &Native, config: &Config, frozen: &ViewState) -> Result<()> {
    for remote in BTreeSet::from([&config.fork.remote, &config.upstream.remote]) {
        let observed = jj.probe_remote(remote)?;
        let current = ViewState::capture(observed.as_ref());
        let relevant = |r: &&RemoteRef| r.remote == *remote;
        ensure!(
            frozen
                .remotes
                .iter()
                .filter(relevant)
                .eq(current.remotes.iter().filter(relevant)),
            "{remote} changed since the plan froze; nothing published or pushed"
        );
    }
    Ok(())
}

/// The caller must first authenticate the Plan (or the task embedding it). This read-only
/// entrypoint is shared by task start and submit, so neither can accidentally prepare a new base.
pub fn load_proposal(repo: &Repo, config: &Config, plan: &Plan) -> Result<Arc<ReadonlyRepo>> {
    plan.validate_ids()?;
    let mut jj = Native::load(&repo.root)?;
    plan.frozen
        .revalidate(repo, &mut jj, config, &plan.context)?;
    probe_remotes(&jj, config, &plan.frozen.view)?;
    let op = OperationId::try_from_hex(
        plan.proposal
            .operation
            .as_deref()
            .context("plan lacks prepared operation")?,
    )
    .context("invalid prepared operation id")?;
    let prepared = jj.load_prepared_operation(&op)?;
    validate_proposal(plan, jj.repo.as_ref(), prepared.as_ref(), config)?;
    Ok(prepared)
}

pub fn report_of(plan: &Plan) -> Report {
    Report {
        plan: Some(plan.clone()),
        diagnostics: plan.issues.clone(),
        exit_code: if plan.outcome == PlanOutcome::Ready {
            EXIT_OK
        } else {
            EXIT_NEEDS_AGENT
        },
        published_operation: None,
        push: None,
    }
}
