//! In-process jj. Loads the colocated workspace with the user's effective jj configuration,
//! prepares it (snapshot, Git import, fetch, reconciliation, tracking) the way jj commands would,
//! freezes it, builds maintenance commits in transactions that stay unpublished while checks run,
//! and publishes one only if the operation log, the working copy, and Git's refs are exactly as
//! they were when the plan froze. See `prepare` for preparation and `transport` for fetch, probe,
//! and push.
//!
//! Only the default stores are supported: the Git backend colocated with the workspace, the
//! simple operation-heads store, and the local working copy. Anything else is refused up front
//! rather than mutated through another path.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt as _, TryStreamExt as _};
use globset::GlobSet;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::conflicts::{MaterializedTreeValue, materialize_tree_value};
use jj_lib::files::{MergeResult, merge_hunks};
use jj_lib::fileset::{self, FilesetDiagnostics, FilesetParseContext};
use jj_lib::git;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::lock::FileLock;
use jj_lib::matchers::{Matcher, NothingMatcher};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OperationId, RefTarget, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as JjRepo, StoreFactories};
use jj_lib::repo_path::{RepoPath, RepoPathUiConverter};
use jj_lib::revset::{self, RevsetDiagnostics, RevsetExpression, SymbolResolver};
use jj_lib::rewrite::{duplicate_commits, merge_commit_trees};
use jj_lib::settings::HumanByteSize;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::transaction::Transaction;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::working_copy::{LockedWorkingCopy, SnapshotOptions};
use jj_lib::workspace::{Workspace, default_working_copy_factories};

pub mod jj_config;
mod prepare;
#[cfg(test)]
mod repo_tests;
mod transport;

use self::jj_config::JjEnv;
pub use self::jj_config::{CommandContext, EffectiveSettings};
pub use self::prepare::reindex;
pub use self::transport::{PushReport, PushUpdate};

/// Runs jj-lib's async APIs to completion. They do local I/O only, so no runtime is needed.
pub fn block_on<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// What a snapshot of the working copy must honor, as `jj` itself would: `snapshot.auto-track`
/// (with fileset aliases), Git's global and repository excludes, and the new-file size limit.
pub(crate) struct SnapshotPolicy {
    auto_track: Box<dyn Matcher>,
    ignores: Arc<GitIgnoreFile>,
    max_new_file_size: u64,
    ignore_files: Vec<PathBuf>,
}

impl SnapshotPolicy {
    fn load(env: &JjEnv, root: &Path, git_repo: &GitDirs) -> Result<SnapshotPolicy> {
        let settings = &env.settings;
        let converter = RepoPathUiConverter::Fs {
            cwd: PathBuf::new(),
            base: PathBuf::new(),
        };
        let context = FilesetParseContext {
            aliases_map: &env.fileset_aliases,
            path_converter: &converter,
        };
        let pattern = settings.get_string("snapshot.auto-track")?;
        let auto_track = fileset::parse(&mut FilesetDiagnostics::new(), &pattern, &context)
            .map_err(|err| anyhow!("invalid snapshot.auto-track: {err}"))?
            .to_matcher();
        let HumanByteSize(mut max_new_file_size) = settings
            .get_value_with("snapshot.max-new-file-size", TryInto::try_into)
            .context("invalid snapshot.max-new-file-size")?;
        if max_new_file_size == 0 {
            max_new_file_size = u64::MAX;
        }
        let mut ignore_files = Vec::new();
        ignore_files.extend(git_repo.excludes_file.as_ref().map(|path| root.join(path)));
        ignore_files.push(git_repo.git_dir.join("info").join("exclude"));
        let mut ignores = GitIgnoreFile::empty();
        for file in &ignore_files {
            ignores = ignores.chain_with_file(RepoPath::root(), file.clone())?;
        }
        Ok(SnapshotPolicy {
            auto_track,
            ignores,
            max_new_file_size,
            ignore_files,
        })
    }

    pub(crate) fn options(&self) -> SnapshotOptions<'_> {
        SnapshotOptions {
            base_ignores: self.ignores.clone(),
            progress: None,
            start_tracking_matcher: self.auto_track.as_ref(),
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: self.max_new_file_size,
        }
    }
}

/// Facts about the colocated Git repository, read through jj's own Git handle.
struct GitDirs {
    git_dir: PathBuf,
    /// Git's global excludes file, as jj finds it: `core.excludesFile` (read through the same Git
    /// configuration jj reads), else `$XDG_CONFIG_HOME/git/ignore`.
    excludes_file: Option<PathBuf>,
}

impl GitDirs {
    fn read(workspace: &Workspace) -> Result<GitDirs> {
        let backend = git::get_git_backend(workspace.repo_loader().store())
            .map_err(|_| anyhow!("jj-fork needs a jj repository backed by Git"))?;
        let colocated = backend
            .git_workdir()
            .and_then(|dir| canonical(dir).ok())
            .is_some_and(|dir| Some(dir) == canonical(workspace.workspace_root()).ok());
        if !colocated {
            bail!("jj-fork needs jj colocated with Git (jj git init --colocate)");
        }
        let git_repo = backend.git_repo();
        let config = git_repo.config_snapshot();
        let configured = config
            .string("core.excludesFile")
            .and_then(|value| std::str::from_utf8(&value).ok().map(str::to_string));
        let excludes_file = match configured {
            Some(path) => Some(jj_lib::file_util::expand_home_path(&path)),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .filter(|x| !x.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                .map(|dir| dir.join("git").join("ignore")),
        };
        Ok(GitDirs {
            git_dir: backend.git_repo_path().to_path_buf(),
            excludes_file,
        })
    }
}

/// How a guarded publication ended.
pub enum Publication {
    /// Published (or, for a proposal that changes nothing, verified current); the operation that
    /// now holds the result.
    Done(OperationId),
    /// Nothing was published because the repository changed after the plan froze.
    Stale(String),
}

/// The workspace and the repository state frozen after preparation.
pub struct Native {
    pub(crate) workspace: Workspace,
    pub(crate) env: JjEnv,
    /// The repository at the frozen operation (or at the last operation jj-fork published).
    pub repo: Arc<ReadonlyRepo>,
    /// The exact operation heads publication requires: only the frozen operation.
    pub(crate) heads: Vec<OperationId>,
    /// The working-copy commit at the frozen operation; its tree is the frozen snapshot.
    pub wc_commit: CommitId,
    pub(crate) policy: SnapshotPolicy,
    effective: EffectiveSettings,
    repository_path: PathBuf,
}

impl Native {
    /// Loads the workspace at `root` read-only and freezes its current, single operation head.
    /// Runs no jj CLI and writes nothing; refuses divergent operation heads and a stale working
    /// copy instead of resolving them.
    pub fn load(root: &Path) -> Result<Native> {
        let native = Native::open(root, false)?;
        native.check_working_copy()?;
        Ok(native)
    }

    /// Loads the workspace at `root`. With `resolve`, divergent operation heads are merged (and
    /// the merge published) the way any jj command would; otherwise they are refused.
    pub(crate) fn open(root: &Path, resolve: bool) -> Result<Native> {
        let config = jj_config::load(root, CommandContext::Fork)?;
        let env = JjEnv::new(config, root)?;
        let workspace = Workspace::load(
            &env.settings,
            root,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .context("failed to load the jj workspace")?;
        let loader = workspace.repo_loader();
        let op_heads = loader.op_heads_store();
        if op_heads
            .as_ref()
            .downcast_ref::<SimpleOpHeadsStore>()
            .is_none()
        {
            bail!(
                "unsupported jj operation-heads store {}; jj-fork needs the default store",
                op_heads.name()
            );
        }
        if workspace.working_copy().name() != "local" {
            bail!(
                "unsupported jj working copy {}; jj-fork needs the default local working copy",
                workspace.working_copy().name()
            );
        }
        let git_dirs = GitDirs::read(&workspace)?;
        let repo = if resolve {
            block_on(loader.load_at_head())?
        } else {
            load_single_head(&workspace)?
        };
        let heads = vec![repo.op_id().clone()];
        let wc_commit = repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .cloned()
            .context("this workspace has no working-copy commit")?;
        let policy = SnapshotPolicy::load(&env, workspace.workspace_root(), &git_dirs)?;
        let effective = EffectiveSettings::load(
            workspace.workspace_root(),
            &env.settings,
            &policy.ignore_files,
        )?;
        let repository_path = canonical(workspace.repo_path())
            .with_context(|| format!("cannot resolve {}", workspace.repo_path().display()))?;
        Ok(Native {
            workspace,
            env,
            repo,
            heads,
            wc_commit,
            policy,
            effective,
            repository_path,
        })
    }

    /// Re-reads the single current operation head and freezes it, with the working copy that
    /// must match it.
    pub(crate) fn freeze(&mut self) -> Result<()> {
        self.repo = load_single_head(&self.workspace)?;
        self.heads = vec![self.repo.op_id().clone()];
        self.wc_commit = self
            .repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .cloned()
            .context("this workspace has no working-copy commit")?;
        self.check_working_copy()
    }

    fn check_working_copy(&self) -> Result<()> {
        let wc_tree = self.repo.store().get_commit(&self.wc_commit)?.tree();
        let disk_tree = self.workspace.working_copy().tree()?;
        if disk_tree.tree_ids_and_labels() != wc_tree.tree_ids_and_labels() {
            bail!("the working copy is stale; run jj workspace update-stale");
        }
        Ok(())
    }

    /// The canonical path of the shared jj repository (`.jj/repo`, or the repository another
    /// workspace points at).
    pub fn repository_path(&self) -> &Path {
        &self.repository_path
    }

    pub fn workspace_name(&self) -> &WorkspaceName {
        self.workspace.workspace_name()
    }

    /// The exact operation heads a publication requires.
    pub fn op_heads(&self) -> &[OperationId] {
        &self.heads
    }

    /// The configuration as jj resolves it now for `context`, read afresh from disk and the
    /// environment, for fetches and pushes to honor their command-scoped settings.
    pub(crate) fn context_env(&self, context: CommandContext) -> Result<JjEnv> {
        let root = self.workspace.workspace_root();
        JjEnv::new(jj_config::load(root, context)?, root)
    }

    /// The jj settings that affect planning, snapshots, written commits, fetches, and pushes,
    /// per command context.
    pub fn effective_settings(&self) -> &EffectiveSettings {
        &self.effective
    }

    pub fn start(&self) -> Transaction {
        let mut tx = self.repo.start_transaction();
        tx.set_workspace_name(self.workspace.workspace_name());
        tx.set_attribute(
            "args".into(),
            std::env::args().collect::<Vec<_>>().join(" "),
        );
        tx
    }

    /// The repository at its current head, read-only, for checks after publication.
    pub fn load_head(&self) -> Result<Arc<ReadonlyRepo>> {
        Ok(block_on(self.workspace.repo_loader().load_at_head())?)
    }

    pub fn set_wc_commit(&self, tx: &mut Transaction, commit: &CommitId) -> Result<()> {
        tx.repo_mut()
            .set_wc_commit(self.workspace.workspace_name().to_owned(), commit.clone())
            .map_err(|_| anyhow!("cannot check out the root commit"))
    }

    /// Resolves a revision the way jj resolves `-r`, with the user's revset aliases, against
    /// the frozen repository. It must name exactly one commit.
    pub fn resolve_single(&self, expression: &str) -> Result<CommitId> {
        let repo = self.repo.as_ref();
        let context = self
            .env
            .revset_context(repo, self.workspace.workspace_name());
        let parsed = revset::parse(&mut RevsetDiagnostics::new(), expression, &context)
            .map_err(|err| anyhow!("invalid revision {expression}: {err}"))?;
        let id_prefixes = IdPrefixContext::new(self.env.extensions.clone());
        let resolver = SymbolResolver::new(repo, self.env.extensions.symbol_resolvers())
            .with_id_prefix_context(&id_prefixes);
        let resolved = parsed
            .resolve_user_expression(repo, &resolver)
            .map_err(|err| anyhow!("revision {expression}: {err}"))?;
        let ids: Vec<CommitId> = block_on(resolved.evaluate(repo)?.stream().take(2).try_collect())?;
        match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => bail!("revision {expression} not found"),
            _ => bail!("revision {expression} names more than one commit"),
        }
    }

    /// Loads an operation written earlier with `Transaction::write(..).leave_unpublished()`. It
    /// must be built directly on the frozen operation. A missing operation, view, or commit means
    /// the prepared state expired (for example after `jj util gc`), never a reason to rebuild.
    pub fn load_prepared_operation(&self, op: &OperationId) -> Result<Arc<ReadonlyRepo>> {
        let expired = |what: &str| {
            anyhow!(
                "prepared operation {} has expired ({what} is no longer stored); prepare it again",
                op.hex()
            )
        };
        let loader = self.workspace.repo_loader();
        let operation =
            block_on(loader.load_operation(op)).map_err(|_| expired("the operation"))?;
        if operation.parent_ids() != std::slice::from_ref(self.repo.op_id()) {
            bail!(
                "prepared operation {} was built on another operation than the current {}",
                op.hex(),
                self.repo.op_id().hex()
            );
        }
        let repo = block_on(loader.load_at(&operation)).map_err(|_| expired("its view"))?;
        for head in repo.view().heads() {
            repo.store()
                .get_commit(head)
                .map_err(|_| expired("a commit it references"))?;
        }
        Ok(repo)
    }

    /// Publishes `tx` if nothing changed since the plan froze. See `publish_prepared`.
    pub fn publish(&mut self, tx: Transaction, description: &str) -> Result<Publication> {
        let proposal = block_on(tx.write(description))?.leave_unpublished();
        self.publish_prepared(proposal, description)
    }

    /// Publishes a proposal (an unpublished operation built on the frozen operation) if nothing
    /// changed since the plan froze, then updates the working copy and Git's refs and HEAD to
    /// match. Holds, in jj's order, the Git import/export lock, the working-copy lock, and
    /// (briefly) the operation-heads lock; runs no jj CLI meanwhile. A proposal whose view equals
    /// the frozen source changes nothing, but is still checked for staleness under the same
    /// locks. Which view changes are permitted is the caller's decision.
    ///
    /// A failure after publication returns an error naming the published operation: the local
    /// result stands and nothing is restored.
    pub fn publish_prepared(
        &mut self,
        proposal: Arc<ReadonlyRepo>,
        description: &str,
    ) -> Result<Publication> {
        let base = self.repo.clone();
        if proposal.operation().parent_ids() != std::slice::from_ref(base.op_id()) {
            return Ok(Publication::Stale(
                "the prepared operation was not built on the frozen operation".into(),
            ));
        }
        let store = base.store().clone();
        let name = self.workspace.workspace_name().to_owned();
        let new_wc = proposal.view().get_wc_commit_id(&name).cloned();
        let moved = moved_bookmarks(base.as_ref(), proposal.as_ref());
        let no_op = proposal.view().store_view() == base.view().store_view();
        let frozen_tree = store.get_commit(&self.wc_commit)?.tree();
        let _git_lock = self.git_lock()?;
        let options = self.policy.options();
        let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
        if let Some(reason) = source_changed(
            locked.locked_wc(),
            &options,
            &frozen_tree,
            &base,
            &self.heads,
        )? {
            return Ok(Publication::Stale(reason));
        }
        let op_heads = base.loader().op_heads_store();

        {
            let _lock = block_on(op_heads.lock())?;
            if block_on(op_heads.get_op_heads())? != self.heads {
                return Ok(Publication::Stale("another jj operation ran".into()));
            }
            // An unchanged view publishes nothing: no artificial maintenance operation.
            if no_op {
                return Ok(Publication::Done(base.op_id().clone()));
            }
            block_on(op_heads.update_op_heads(&self.heads, proposal.op_id()))?;
        }
        let op = proposal.op_id().clone();
        self.repo = proposal.clone();
        self.heads = vec![op.clone()];

        let synced: Result<Arc<ReadonlyRepo>> = (|| {
            let skipped = if let Some(wc) = &new_wc
                && *wc != self.wc_commit
            {
                block_on(locked.locked_wc().check_out(&store.get_commit(wc)?))
                    .context("failed to update the working copy")?
                    .skipped_files
            } else {
                0
            };
            block_on(locked.finish(op.clone())).context("failed to save the working copy")?;
            if skipped > 0 {
                bail!(
                    "checkout skipped {skipped} updates blocked by untracked files; those files were preserved. Move the obstructing files aside and restore the intended checkout before retrying"
                );
            }
            let mut tx = proposal.start_transaction();
            if let Some(wc) = &new_wc {
                block_on(git::reset_head(tx.repo_mut(), &store.get_commit(wc)?))
                    .context("failed to reset Git HEAD")?;
            }
            let stats = git::export_refs(tx.repo_mut()).context("failed to export Git refs")?;
            let failed: Vec<String> = stats
                .failed_bookmarks
                .iter()
                .map(|(symbol, _)| symbol.name.as_str().to_string())
                .filter(|name| moved.contains(name))
                .collect();
            if !failed.is_empty() {
                bail!("failed to export bookmarks to Git: {}", failed.join(", "));
            }
            Ok(block_on(
                tx.commit(format!("{description}: export to Git")),
            )?)
        })();
        match synced {
            Ok(repo) => {
                self.heads = vec![repo.op_id().clone()];
                self.repo = repo;
                self.wc_commit = new_wc.unwrap_or_else(|| self.wc_commit.clone());
                Ok(Publication::Done(self.repo.op_id().clone()))
            }
            Err(err) => Err(err.context(format!(
                "published operation {} ({}), but updating the working copy or Git did not finish; nothing was restored or pushed. Inspect jj status and resolve the synchronization problem before retrying; a stale workspace may need jj workspace update-stale",
                op.hex(),
                if moved.is_empty() {
                    "no bookmarks moved".to_string()
                } else {
                    format!(
                        "moved {}",
                        moved.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                }
            ))),
        }
    }

    /// Whether the source is still exactly as frozen: the files on disk (snapshotted with jj's
    /// policies, so new auto-tracked files count), the working-copy state, the operation heads,
    /// and Git's refs and HEAD. Returns the reason when it is not. The same check guards
    /// `publish_prepared`; this one records nothing (the snapshot is not saved) and publishes
    /// nothing. Takes the Git import/export and working-copy locks briefly.
    pub fn verify_frozen(&mut self) -> Result<Option<String>> {
        let base = self.repo.clone();
        let frozen_tree = base.store().get_commit(&self.wc_commit)?.tree();
        let _git_lock = self.git_lock()?;
        let options = self.policy.options();
        let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
        source_changed(
            locked.locked_wc(),
            &options,
            &frozen_tree,
            &base,
            &self.heads,
        )
    }

    /// The lock jj holds while importing from and exporting to the colocated Git repository.
    pub(crate) fn git_lock(&self) -> Result<FileLock> {
        FileLock::lock(self.workspace.repo_path().join("git_import_export.lock"))
            .map_err(|err| anyhow!("failed to lock Git import/export: {err}"))
    }
}

/// Why the source no longer matches the frozen state, if it does not. The caller holds the Git
/// import/export lock and the working-copy lock; the operation heads are compared without a lock
/// (a publication compares them again under the operation-heads lock).
fn source_changed(
    locked_wc: &mut dyn LockedWorkingCopy,
    options: &SnapshotOptions<'_>,
    frozen_tree: &MergedTree,
    base: &Arc<ReadonlyRepo>,
    heads: &[OperationId],
) -> Result<Option<String>> {
    if locked_wc.old_tree().tree_ids_and_labels() != frozen_tree.tree_ids_and_labels() {
        return Ok(Some("another command updated the working copy".into()));
    }
    let (snapshot, _) = block_on(locked_wc.snapshot(options))?;
    if snapshot.tree_ids_and_labels() != frozen_tree.tree_ids_and_labels() {
        return Ok(Some(
            "files in the working copy changed (left as they are)".into(),
        ));
    }
    // A jj command also changes Git's refs when it exports, so name that cause first.
    if block_on(base.loader().op_heads_store().get_op_heads())? != heads {
        return Ok(Some("another jj operation ran".into()));
    }
    let mut probe = base.start_transaction();
    block_on(git::import_head(probe.repo_mut()))?;
    let import = git::GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    block_on(git::import_refs(probe.repo_mut(), &import))?;
    if probe.repo().has_changes() {
        return Ok(Some("Git's refs or HEAD changed outside jj".into()));
    }
    Ok(None)
}

/// Loads the repository at its only operation head, writing nothing.
fn load_single_head(workspace: &Workspace) -> Result<Arc<ReadonlyRepo>> {
    let loader = workspace.repo_loader();
    let op_heads = loader.op_heads_store();
    let heads = block_on(op_heads.get_op_heads())?;
    let [head] = heads.as_slice() else {
        bail!(
            "another jj command is running in this repository (divergent operations); rerun when it finishes"
        );
    };
    let operation = block_on(loader.load_operation(head))?;
    let repo = block_on(loader.load_at(&operation))?;
    if block_on(op_heads.get_op_heads())? != heads {
        bail!("another jj command is running in this repository; rerun when it finishes");
    }
    Ok(repo)
}

fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Local bookmarks whose target differs between two repositories.
fn moved_bookmarks(before: &dyn JjRepo, after: &dyn JjRepo) -> BTreeSet<String> {
    let names: BTreeSet<&RefName> = before
        .view()
        .local_bookmarks()
        .chain(after.view().local_bookmarks())
        .map(|(name, _)| name)
        .collect();
    names
        .into_iter()
        .filter(|name| {
            before.view().get_local_bookmark(name) != after.view().get_local_bookmark(name)
        })
        .map(|name| name.as_str().to_string())
        .collect()
}

// Queries. Each takes the repository to read, so they work on the frozen repository and on the
// unpublished transaction alike.

pub fn commit(repo: &dyn JjRepo, id: &CommitId) -> Result<Commit> {
    Ok(repo.store().get_commit(id)?)
}

pub fn parse_id(hex: &str) -> Result<CommitId> {
    CommitId::try_from_hex(hex).with_context(|| format!("not a commit id: {hex}"))
}

/// A local bookmark's commit, or None when absent. A conflicted bookmark is an error, never an
/// arbitrary side of the conflict.
pub fn bookmark(repo: &dyn JjRepo, name: &str) -> Result<Option<CommitId>> {
    let target = repo.view().get_local_bookmark(RefName::new(name));
    if target.has_conflict() {
        bail!("bookmark {name} is conflicted; settle it with: jj bookmark set {name} -r <commit>");
    }
    Ok(target.as_normal().cloned())
}

/// Present local bookmarks whose names start with any of `prefixes`, sorted.
pub fn bookmarks_with(repo: &dyn JjRepo, prefixes: &[String]) -> Vec<String> {
    repo.view()
        .local_bookmarks()
        .filter(|(_, target)| target.is_present())
        .map(|(name, _)| name.as_str().to_string())
        .filter(|name| prefixes.iter().any(|p| name.starts_with(p.as_str())))
        .collect()
}

pub fn set_bookmark(tx: &mut Transaction, name: &str, commit: &CommitId) {
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new(name), RefTarget::normal(commit.clone()));
}

/// The fork remote's bookmarks, as `(name, target)` with the target's added commit ids.
pub fn remote_bookmarks(repo: &dyn JjRepo, remote: &str) -> Vec<RemoteBookmark> {
    repo.view()
        .remote_bookmarks(RemoteName::new(remote))
        .filter(|(_, remote_ref)| remote_ref.target.is_present())
        .map(|(name, remote_ref)| RemoteBookmark {
            name: name.as_str().to_string(),
            commits: remote_ref.target.added_ids().cloned().collect(),
            tracked: remote_ref.state == RemoteRefState::Tracked,
        })
        .collect()
}

pub struct RemoteBookmark {
    pub name: String,
    pub commits: Vec<CommitId>,
    pub tracked: bool,
}

/// The no-silent-drop guard: bookmarks on `remote` under `prefixes` that publishing `merge` would
/// silently drop. A remote bookmark is kept when it is merged into `merge` (its local bookmark,
/// which is what gets pushed, or the remote's commits when there is no local copy) or when it
/// is deliberately deleted here: tracked on the remote and absent locally, a pending deletion.
/// An untracked remote bookmark without a local copy is not a deletion; it is unmerged work.
/// Sorted by name.
pub fn unmerged_remote_bookmarks(
    repo: &dyn JjRepo,
    remote: &str,
    prefixes: &[String],
    merge: &CommitId,
) -> Result<Vec<String>> {
    let mut dropped = Vec::new();
    for r in remote_bookmarks(repo, remote) {
        if !prefixes.iter().any(|p| r.name.starts_with(p.as_str())) {
            continue;
        }
        let local = bookmark(repo, &r.name)?;
        if r.tracked && local.is_none() {
            continue;
        }
        let tips = local.map(|c| vec![c]).unwrap_or(r.commits);
        let mut merged = true;
        for tip in &tips {
            merged &= is_ancestor(repo, tip, merge)?;
        }
        if !merged {
            dropped.push(r.name);
        }
    }
    dropped.sort();
    Ok(dropped)
}

pub fn is_ancestor(repo: &dyn JjRepo, ancestor: &CommitId, descendant: &CommitId) -> Result<bool> {
    Ok(repo.index().is_ancestor(ancestor, descendant)?)
}

/// The commits of `ids` that are not ancestors of others, sorted.
pub fn heads(repo: &dyn JjRepo, ids: &[CommitId]) -> Result<Vec<CommitId>> {
    let mut heads = repo.index().heads(&mut ids.iter())?;
    heads.sort();
    Ok(heads)
}

/// `from..to`: ancestors of `to` that are not ancestors of `from`, children before parents.
pub fn range(repo: &dyn JjRepo, from: &CommitId, to: &CommitId) -> Result<Vec<CommitId>> {
    let expression =
        RevsetExpression::commit(from.clone()).range(&RevsetExpression::commit(to.clone()));
    let revset = expression.evaluate(repo)?;
    let ids: Vec<_> = block_on(revset.stream().collect::<Vec<_>>());
    Ok(ids.into_iter().collect::<Result<_, _>>()?)
}

/// Copies `commits` (children before parents) onto `onto` as new changes, leaving the originals
/// and their descendants untouched. Returns each original's copy.
pub fn duplicate(
    tx: &mut Transaction,
    commits: &[CommitId],
    onto: &[CommitId],
) -> Result<HashMap<CommitId, Commit>> {
    let stats = block_on(duplicate_commits(
        tx.repo_mut(),
        commits,
        &HashMap::new(),
        onto,
        &[],
    ))?;
    Ok(stats.duplicated_commits.into_iter().collect())
}

/// The merged tree of `parents`, as jj merges them (recursively, with its own merge rules).
pub fn merged_tree(repo: &dyn JjRepo, parents: &[CommitId]) -> Result<MergedTree> {
    let commits: Vec<Commit> = parents
        .iter()
        .map(|id| commit(repo, id))
        .collect::<Result<_>>()?;
    Ok(block_on(merge_commit_trees(repo, &commits))?)
}

pub fn subject(commit: &Commit) -> &str {
    commit.description().lines().next().unwrap_or_default()
}

pub fn short_change(commit: &Commit) -> String {
    commit.change_id().reverse_hex()[..12].to_string()
}

/// One conflicted path of a tree, for reports.
pub struct ConflictedPath {
    pub path: String,
    pub sides: usize,
}

pub fn conflicted_paths(tree: &MergedTree) -> Result<Vec<ConflictedPath>> {
    tree.conflicts()
        .map(|(path, value)| {
            Ok(ConflictedPath {
                path: path.as_internal_file_string().to_string(),
                sides: value?.num_sides(),
            })
        })
        .collect()
}

/// Size of a conflict, the measure behind its difficulty tier.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConflictSize {
    /// Every conflicted path, generated or not.
    pub files: Vec<String>,
    pub code_files: usize,
    pub hunks: usize,
    pub lines: usize,
}

/// Measures the conflicts in `tree`, skipping generated paths. Text conflicts count each
/// conflicted hunk and the lines on its added sides; base lines are what the sides changed, not
/// work to reconcile. Binary, symlink, submodule, file/directory, and executable-bit conflicts
/// count as one hunk with no lines.
pub fn conflict_size(
    repo: &dyn JjRepo,
    tree: &MergedTree,
    generated: &GlobSet,
) -> Result<ConflictSize> {
    let store = repo.store();
    let mut size = ConflictSize::default();
    for (path, value) in tree.conflicts() {
        let value = value?;
        let name = path.as_internal_file_string().to_string();
        let is_generated = generated.is_match(&name);
        size.files.push(name);
        if is_generated {
            continue;
        }
        size.code_files += 1;
        let (hunks, lines) =
            match block_on(materialize_tree_value(store, &path, value, tree.labels()))? {
                MaterializedTreeValue::FileConflict(file) => {
                    text_conflict_size(&file.contents, store.merge_options())
                }
                _ => (1, 0),
            };
        size.hunks += hunks;
        size.lines += lines;
    }
    Ok(size)
}

fn text_conflict_size<T: AsRef<[u8]>>(
    contents: &Merge<T>,
    options: &MergeOptions,
) -> (usize, usize) {
    if contents.iter().any(|c| c.as_ref().contains(&0)) {
        return (1, 0);
    }
    match merge_hunks(contents, options) {
        // The text merges, so the path conflicts in some other way (its executable bit).
        MergeResult::Resolved(_) => (1, 0),
        MergeResult::Conflict(hunks) => {
            let conflicted: Vec<_> = hunks.iter().filter(|h| !h.is_resolved()).collect();
            let lines = conflicted
                .iter()
                .flat_map(|hunk| hunk.adds())
                .map(|side| count_lines(side.as_ref()))
                .sum();
            (conflicted.len(), lines)
        }
    }
}

fn count_lines(text: &[u8]) -> usize {
    text.split(|&b| b == b'\n').count() - usize::from(text.is_empty() || text.ends_with(b"\n"))
}

#[cfg(test)]
mod tests {
    use jj_lib::files::FileMergeHunkLevel;
    use jj_lib::merge::SameChange;

    use super::*;

    fn options() -> MergeOptions {
        MergeOptions {
            hunk_level: FileMergeHunkLevel::Line,
            same_change: SameChange::Accept,
        }
    }

    #[test]
    fn text_conflicts_count_added_sides_but_not_the_base() {
        // Upstream rewrites line1; the patch rewrites line1 and drops the rest. jj merges this
        // into one hunk: 3 upstream lines and 1 patch line; the 3 base lines do not count.
        let merge = Merge::from_vec(vec![
            "upstream line1\nline2\nline3\n",
            "line1\nline2\nline3\n",
            "patched line1\n",
        ]);
        assert_eq!(text_conflict_size(&merge, &options()), (1, 4));
    }

    #[test]
    fn separate_hunks_are_counted_separately() {
        let merge = Merge::from_vec(vec![
            "A\nkeep\nkeep\nkeep\nX\n",
            "a\nkeep\nkeep\nkeep\nx\n",
            "B\nkeep\nkeep\nkeep\nY\nY2\n",
        ]);
        assert_eq!(text_conflict_size(&merge, &options()), (2, 5));
    }

    #[test]
    fn binary_and_cleanly_merging_contents_are_one_hunk_without_lines() {
        let binary = Merge::from_vec(vec!["a\0", "b\0", "c\0"]);
        assert_eq!(text_conflict_size(&binary, &options()), (1, 0));
        let clean = Merge::from_vec(vec!["same\n", "same\n", "same\n"]);
        assert_eq!(text_conflict_size(&clean, &options()), (1, 0));
    }

    #[test]
    fn lines_without_a_trailing_newline_still_count() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
    }
}
