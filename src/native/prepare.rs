//! Preparation: what any jj command does before it runs (import Git's HEAD, snapshot the working
//! copy, import Git's refs), then fetching, reconciliation, and tracking, each published as its
//! own operation like the jj commands it replaces. Preparation can publish state; the plan
//! freezes only afterwards.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use jj_lib::backend::CommitId;
use jj_lib::default_index::DefaultIndexStore;
use jj_lib::git;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{Repo as JjRepo, StoreFactories};
use jj_lib::revset::SymbolResolver;
use jj_lib::rewrite::RebaseOptions;
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
use jj_lib::working_copy::WorkingCopyFreshness;
use jj_lib::workspace::{Workspace, default_working_copy_factories};

use super::jj_config::{self, JjEnv};
use super::{Native, block_on};
use crate::config::Config;
use crate::progress;
use crate::repo::Repo;

impl Native {
    /// Prepares the clone and freezes it: completes a shallow clone, imports Git's HEAD,
    /// snapshots the working copy, imports Git's refs, optionally fetches the fork and upstream
    /// remotes, reconciles the fork's bookmarks with the fork remote, and tracks the fork's
    /// remote bookmarks. Runs no jj CLI unless the clone first needs `jj fork init`.
    pub fn prepare(repo: &Repo, config: &Config, fetch: bool) -> Result<Native> {
        if !repo.root.join(".jj").is_dir() {
            bail!(
                "no jj repository in {}; run: jj fork init",
                repo.root.display()
            );
        }
        if repo.is_shallow()? {
            progress("shallow clone detected; running init");
            crate::init::init(repo, config)?;
        }
        let mut native = Native::open(&repo.root, true)?;
        native.import_and_snapshot()?;
        if fetch {
            progress(&format!(
                "fetching {} and {}",
                config.fork.remote, config.upstream.remote
            ));
            native.fetch_remote(&config.fork.remote)?;
            native.fetch_remote(&config.upstream.remote)?;
        }
        native.reconcile_and_track(config)?;
        native.freeze()?;
        Ok(native)
    }

    /// Opens the workspace for `jj fork init` and brings it up to date with Git and the disk.
    pub(crate) fn open_for_init(root: &Path) -> Result<Native> {
        let mut native = Native::open(root, true)?;
        native.import_and_snapshot()?;
        Ok(native)
    }

    /// Tracks the fork's untracked remote bookmarks, as its own operation.
    pub(crate) fn track_fork_bookmarks(&mut self, config: &Config) -> Result<()> {
        let _lock = self.git_lock()?;
        let mut tx = self.start();
        crate::init::track(&mut tx, config)?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        self.finish_tx(tx, "jj-fork: track fork bookmarks")
    }

    /// Imports Git's HEAD, snapshots the working copy, and imports Git's refs, under one Git
    /// import/export lock, as every jj command does first.
    pub(crate) fn import_and_snapshot(&mut self) -> Result<()> {
        let _lock = self.git_lock()?;
        self.repo = block_on(self.workspace.repo_loader().load_at_head())?;
        self.import_git_head()?;
        self.snapshot_working_copy()?;
        self.import_git_refs()?;
        self.heads = vec![self.repo.op_id().clone()];
        Ok(())
    }

    /// Follows a HEAD moved by Git (a `git checkout` or `git commit`): the working copy becomes a
    /// new change on the new HEAD, as in jj. The files on disk are already Git's.
    fn import_git_head(&mut self) -> Result<()> {
        let mut tx = self.start();
        block_on(git::import_head(tx.repo_mut()))?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        let new_head = tx.repo().view().git_head().as_normal().cloned();
        let Some(head) = new_head else {
            block_on(tx.repo_mut().rebase_descendants())?;
            return self.finish_tx(tx, "import git head");
        };
        let name = self.workspace.workspace_name().to_owned();
        let head = self.repo.store().get_commit(&head)?;
        let wc = block_on(tx.repo_mut().check_out(name, &head))?;
        let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
        block_on(locked.locked_wc().reset(&wc))?;
        block_on(tx.repo_mut().rebase_descendants())?;
        let repo = block_on(tx.commit("import git head"))?;
        block_on(locked.finish(repo.op_id().clone()))?;
        self.repo = repo;
        self.wc_commit = wc.id().clone();
        Ok(())
    }

    /// Records the files on disk in the working-copy commit with jj's snapshot policies. An
    /// immutable working-copy commit gets a new change on top instead of being rewritten.
    fn snapshot_working_copy(&mut self) -> Result<()> {
        let name = self.workspace.workspace_name().to_owned();
        let options = self.policy.options();
        let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
        let Some(wc_id) = self.repo.view().get_wc_commit_id(&name).cloned() else {
            return Ok(());
        };
        let mut wc = self.repo.store().get_commit(&wc_id)?;
        let old_op = locked.locked_wc().old_operation_id().clone();
        match block_on(WorkingCopyFreshness::check_stale(
            locked.locked_wc(),
            &wc,
            &self.repo,
        ))? {
            WorkingCopyFreshness::Fresh => {}
            WorkingCopyFreshness::Updated(operation) => {
                self.repo = block_on(self.repo.reload_at(&operation))?;
                let id = self
                    .repo
                    .view()
                    .get_wc_commit_id(&name)
                    .cloned()
                    .context("this workspace has no working-copy commit")?;
                wc = self.repo.store().get_commit(&id)?;
            }
            WorkingCopyFreshness::WorkingCopyStale => bail!(
                "the working copy is stale (not updated since operation {}); run jj workspace update-stale",
                &old_op.hex()[..12]
            ),
            WorkingCopyFreshness::SiblingOperation => bail!(
                "the working copy's operation {} is a sibling of the repository's; run jj op integrate {}",
                &old_op.hex()[..12],
                &old_op.hex()[..12]
            ),
        }
        let (tree, stats) = block_on(locked.locked_wc().snapshot(&options))?;
        for (path, reason) in &stats.untracked_paths {
            progress(&format!(
                "warning: {} left untracked: {reason:?}",
                path.as_internal_file_string()
            ));
        }
        if tree.tree_ids_and_labels() != wc.tree().tree_ids_and_labels() {
            let mut tx = self.repo.start_transaction();
            tx.set_is_snapshot(true);
            tx.set_workspace_name(&name);
            let immutable = is_immutable(&self.env, &name, tx.repo(), wc.id())?;
            let new_wc = if immutable {
                progress(
                    "the working-copy commit is immutable; recording the edits in a new commit on top of it",
                );
                block_on(
                    tx.repo_mut()
                        .new_commit(vec![wc.id().clone()], tree.clone())
                        .write(),
                )?
            } else {
                block_on(
                    tx.repo_mut()
                        .rewrite_commit(&wc)
                        .set_tree(tree.clone())
                        .write(),
                )?
            };
            tx.repo_mut()
                .set_wc_commit(name.clone(), new_wc.id().clone())
                .map_err(|_| anyhow!("cannot snapshot into the root commit"))?;
            block_on(tx.repo_mut().rebase_descendants())?;
            block_on(git::update_intent_to_add(
                tx.base_repo().as_ref(),
                &wc.tree(),
                &new_wc.tree(),
            ))?;
            git::export_refs(tx.repo_mut())?;
            self.repo = block_on(tx.commit("snapshot working copy"))?;
        }
        block_on(locked.finish(self.repo.op_id().clone()))?;
        self.wc_commit = self
            .repo
            .view()
            .get_wc_commit_id(&name)
            .cloned()
            .unwrap_or(wc_id);
        Ok(())
    }

    /// Imports refs moved by Git (a plain `git fetch`, a `git branch`), as jj does before every
    /// command, then rebases mutable descendants of rewritten commits.
    fn import_git_refs(&mut self) -> Result<()> {
        let options = self.env.import_options()?;
        let mut tx = self.start();
        block_on(git::import_refs(tx.repo_mut(), &options))?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        rebase_mutable_descendants(&self.env, self.workspace.workspace_name(), &mut tx)?;
        self.finish_tx(tx, "import git refs")
    }

    /// Publishes a preparation transaction the way a jj command finishes one: export to Git,
    /// commit, then update the working copy if its commit changed. The caller holds the Git
    /// import/export lock.
    pub(crate) fn finish_tx(&mut self, mut tx: Transaction, description: &str) -> Result<()> {
        let name = self.workspace.workspace_name().to_owned();
        let old_wc = tx.base_repo().view().get_wc_commit_id(&name).cloned();
        let new_wc = tx.repo().view().get_wc_commit_id(&name).cloned();
        let store = self.repo.store().clone();
        if let Some(wc) = &new_wc {
            match block_on(git::reset_head(tx.repo_mut(), &store.get_commit(wc)?)) {
                Ok(()) => {}
                Err(err @ git::GitResetHeadError::UpdateHeadRef(_)) => {
                    progress(&format!("warning: {err}"));
                }
                Err(err) => return Err(err).context("failed to reset Git HEAD"),
            }
        }
        let stats = git::export_refs(tx.repo_mut()).context("failed to export Git refs")?;
        for (symbol, reason) in &stats.failed_bookmarks {
            progress(&format!(
                "warning: failed to export {}@{} to Git: {reason}",
                symbol.name.as_str(),
                symbol.remote.as_str()
            ));
        }
        let repo = block_on(tx.commit(description))?;
        self.repo = repo;
        self.heads = vec![self.repo.op_id().clone()];
        if let Some(wc) = new_wc
            && Some(&wc) != old_wc.as_ref()
        {
            let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
            if let Some(old) = &old_wc
                && locked.locked_wc().old_tree().tree_ids_and_labels()
                    != store.get_commit(old)?.tree().tree_ids_and_labels()
            {
                bail!("the working copy changed concurrently while jj-fork updated it");
            }
            block_on(locked.locked_wc().check_out(&store.get_commit(&wc)?))
                .context("failed to update the working copy")?;
            block_on(locked.finish(self.repo.op_id().clone()))?;
            self.wc_commit = wc;
        }
        Ok(())
    }

    /// Reconciles the fork's bookmarks with the fork remote, then tracks its remote bookmarks,
    /// in one operation.
    fn reconcile_and_track(&mut self, config: &Config) -> Result<()> {
        let _lock = self.git_lock()?;
        let mut tx = self.start();
        crate::reconcile::reconcile(&mut tx, config)?;
        crate::init::track(&mut tx, config)?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        self.finish_tx(tx, "jj-fork: reconcile and track fork bookmarks")
    }
}

/// Whether `commit` is in the user's immutable set (`::immutable_heads()`) in `repo`.
pub(crate) fn is_immutable(
    env: &JjEnv,
    workspace_name: &WorkspaceName,
    repo: &dyn JjRepo,
    commit: &CommitId,
) -> Result<bool> {
    let immutable = env.immutable_heads(repo, workspace_name)?.ancestors();
    let resolver = SymbolResolver::new(repo, env.extensions.symbol_resolvers());
    let resolved = immutable
        .resolve_user_expression(repo, &resolver)
        .map_err(|err| anyhow!("invalid revset-aliases.immutable_heads(): {err}"))?;
    let revset = resolved.evaluate(repo)?;
    Ok(revset.containing_fn()(commit)?)
}

/// Rebases descendants of commits a Git import or fetch rewrote, keeping the immutable commits
/// of `env`'s configuration in place, as `jj git fetch` does.
pub(crate) fn rebase_mutable_descendants(
    env: &JjEnv,
    workspace_name: &WorkspaceName,
    tx: &mut Transaction,
) -> Result<()> {
    let base = tx.base_repo().clone();
    let immutable = env
        .immutable_heads(base.as_ref(), workspace_name)?
        .ancestors();
    let resolver = SymbolResolver::new(base.as_ref(), env.extensions.symbol_resolvers());
    let immutable = immutable
        .resolve_user_expression(base.as_ref(), &resolver)
        .map_err(|err| anyhow!("invalid revset-aliases.immutable_heads(): {err}"))?;
    block_on(tx.repo_mut().rebase_descendants_with_options(
        &immutable,
        &RebaseOptions::default(),
        |_, _| {},
    ))?;
    Ok(())
}

/// Rebuilds jj's commit index from the Git history after a shallow clone was completed, so
/// commits at the old shallow boundary get their real parents. The operation log, jj-only
/// commits, and configuration are kept.
pub fn reindex(root: &Path) -> Result<()> {
    let settings =
        UserSettings::from_config(jj_config::load(root, jj_config::CommandContext::Fork)?)?;
    let workspace = Workspace::load(
        &settings,
        root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load the jj workspace")?;
    let loader = workspace.repo_loader();
    let Some(index_store) = loader.index_store().downcast_ref::<DefaultIndexStore>() else {
        bail!("unsupported jj index store {}", loader.index_store().name());
    };
    index_store
        .reinit()
        .map_err(|err| anyhow!("failed to reset the jj index: {err}"))?;
    for head in block_on(loader.op_heads_store().get_op_heads())? {
        let operation = block_on(loader.load_operation(&head))?;
        block_on(index_store.build_index_at_operation(&operation, loader.store()))
            .map_err(|err| anyhow!("failed to rebuild the jj index: {err}"))?;
    }
    Ok(())
}
