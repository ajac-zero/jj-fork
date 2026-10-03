//! In-process jj. Loads the colocated workspace with the user's effective jj settings, builds
//! maintenance commits in a transaction that stays unpublished while checks run, and publishes it
//! only if the operation log, the working copy, and Git's refs are exactly as they were when the
//! plan froze.
//!
//! Only the default stores are supported: the Git backend colocated with the workspace, the
//! simple operation-heads store, and the local working copy. Anything else is refused up front
//! rather than mutated through another path.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt as _;
use globset::GlobSet;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflicts::{MaterializedTreeValue, materialize_tree_value};
use jj_lib::files::{MergeResult, merge_hunks};
use jj_lib::fileset::{self, FilesetAliasesMap, FilesetDiagnostics, FilesetParseContext};
use jj_lib::git;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::lock::FileLock;
use jj_lib::matchers::{Matcher, NothingMatcher};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OperationId, RefTarget, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as JjRepo, StoreFactories};
use jj_lib::repo_path::{RepoPath, RepoPathUiConverter};
use jj_lib::revset::RevsetExpression;
use jj_lib::rewrite::{duplicate_commits, merge_commit_trees};
use jj_lib::settings::{HumanByteSize, UserSettings};
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::transaction::Transaction;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{Workspace, default_working_copy_factories};

use crate::run;

/// The jj release whose CLI and library jj-fork pins. Preparation and push use the CLI, the
/// maintenance engine uses the library, so both must agree on the repository format.
pub const JJ_VERSION: &str = "0.43.0";

/// Runs jj-lib's async APIs to completion. They do local I/O only, so no runtime is needed.
pub fn block_on<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Refuses to run against a jj CLI other than the pinned release.
pub fn check_cli(dir: &Path) -> Result<()> {
    let version = run::output(dir, "jj", &["--version"])?;
    let number = version.strip_prefix("jj ").unwrap_or(&version);
    if number.split('-').next() != Some(JJ_VERSION) {
        bail!("jj-fork needs jj {JJ_VERSION}, found {version}; run scripts/install-jj");
    }
    Ok(())
}

/// Loads the user's effective jj settings (defaults, user, repo, and workspace config) from the
/// pinned CLI. The listing can contain secrets, so it is never printed, not even in errors.
fn load_settings(root: &Path) -> Result<UserSettings> {
    let listing = run::output(
        root,
        "jj",
        &[
            "--no-pager",
            "--color=never",
            "config",
            "list",
            "--include-defaults",
            "-T",
            r#"name ++ " = " ++ value ++ "\n""#,
        ],
    )
    .map_err(|_| anyhow!("failed to read the effective jj config (jj config list)"))?;
    let layer = ConfigLayer::parse(ConfigSource::User, &listing)
        .map_err(|_| anyhow!("failed to parse the effective jj config listing"))?;
    let mut config = StackedConfig::empty();
    config.add_layer(layer);
    UserSettings::from_config(config).context("invalid jj settings")
}

/// What a snapshot of the working copy must honor, as `jj` itself would: `snapshot.auto-track`
/// (with fileset aliases), Git's global and repository excludes, and the new-file size limit.
struct SnapshotPolicy {
    auto_track: Box<dyn Matcher>,
    ignores: Arc<GitIgnoreFile>,
    max_new_file_size: u64,
}

impl SnapshotPolicy {
    fn load(settings: &UserSettings, root: &Path, git_dir: &Path) -> Result<SnapshotPolicy> {
        let mut aliases = FilesetAliasesMap::new();
        for name in settings.table_keys("fileset-aliases") {
            let value = settings.get_string(["fileset-aliases", name])?;
            aliases
                .insert(name, value, None)
                .map_err(|err| anyhow!("invalid fileset alias {name}: {err}"))?;
        }
        let converter = RepoPathUiConverter::Fs {
            cwd: PathBuf::new(),
            base: PathBuf::new(),
        };
        let context = FilesetParseContext {
            aliases_map: &aliases,
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
        let mut ignores = GitIgnoreFile::empty();
        if let Some(excludes) = excludes_file(root) {
            ignores = ignores.chain_with_file(RepoPath::root(), excludes)?;
        }
        ignores =
            ignores.chain_with_file(RepoPath::root(), git_dir.join("info").join("exclude"))?;
        Ok(SnapshotPolicy {
            auto_track,
            ignores,
            max_new_file_size,
        })
    }

    fn options(&self) -> SnapshotOptions<'_> {
        SnapshotOptions {
            base_ignores: self.ignores.clone(),
            progress: None,
            start_tracking_matcher: self.auto_track.as_ref(),
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: self.max_new_file_size,
        }
    }
}

/// Git's global excludes file, as jj finds it: `core.excludesFile`, else `$XDG_CONFIG_HOME/git/ignore`.
fn excludes_file(root: &Path) -> Option<PathBuf> {
    if let Ok(path) = run::output(root, "git", &["config", "--path", "core.excludesFile"])
        && !path.is_empty()
    {
        return Some(root.join(path));
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_home.join("git").join("ignore"))
}

/// How a guarded publication ended.
pub enum Publication {
    /// Published; the operation that now holds the result.
    Done(OperationId),
    /// Nothing was published because the repository changed after the plan froze.
    Stale(String),
}

/// The workspace and the repository state frozen after preparation.
pub struct Native {
    workspace: Workspace,
    /// The repository at the frozen operation (or at the last operation jj-fork published).
    pub repo: Arc<ReadonlyRepo>,
    /// The exact operation heads publication requires: only the frozen operation.
    heads: Vec<OperationId>,
    /// The working-copy commit at the frozen operation; its tree is the frozen snapshot.
    pub wc_commit: CommitId,
    policy: SnapshotPolicy,
}

impl Native {
    /// Loads the workspace at `root` and freezes the current operation.
    pub fn load(root: &Path) -> Result<Native> {
        let settings = load_settings(root)?;
        let workspace = Workspace::load(
            &settings,
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
        let backend = git::get_git_backend(loader.store())
            .map_err(|_| anyhow!("jj-fork needs a jj repository backed by Git"))?;
        let colocated = backend
            .git_workdir()
            .and_then(|dir| canonical(dir).ok())
            .is_some_and(|dir| Some(dir) == canonical(workspace.workspace_root()).ok());
        if !colocated {
            bail!("jj-fork needs jj colocated with Git (jj git init --colocate)");
        }
        let git_dir = backend.git_repo_path().to_path_buf();
        let before = block_on(op_heads.get_op_heads())?;
        let repo = block_on(loader.load_at_head())?;
        let after = block_on(op_heads.get_op_heads())?;
        if before != after || after != [repo.op_id().clone()] {
            bail!("another jj command is running in this repository; rerun when it finishes");
        }
        let wc_commit = repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .cloned()
            .context("this workspace has no working-copy commit")?;
        let wc_tree = repo.store().get_commit(&wc_commit)?.tree();
        let disk_tree = workspace.working_copy().tree()?;
        if disk_tree.tree_ids_and_labels() != wc_tree.tree_ids_and_labels() {
            bail!("the working copy is stale; run jj workspace update-stale");
        }
        let policy = SnapshotPolicy::load(repo.settings(), workspace.workspace_root(), &git_dir)?;
        Ok(Native {
            workspace,
            repo,
            heads: after,
            wc_commit,
            policy,
        })
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

    /// Publishes `tx` if nothing changed since the plan froze, then updates the working copy and
    /// Git's refs and HEAD to match. Holds, in jj's order, the Git import/export lock, the
    /// working-copy lock, and (briefly) the operation-heads lock; runs no jj CLI meanwhile.
    ///
    /// A failure after publication returns an error naming the published operation: the local
    /// result stands and nothing is restored.
    pub fn publish(&mut self, tx: Transaction, description: &str) -> Result<Publication> {
        let base = self.repo.clone();
        let store = base.store().clone();
        let name = self.workspace.workspace_name().to_owned();
        let new_wc = tx.repo().view().get_wc_commit_id(&name).cloned();
        let moved = moved_bookmarks(base.as_ref(), tx.repo());
        let frozen_tree = store.get_commit(&self.wc_commit)?.tree();
        let _git_lock = FileLock::lock(self.workspace.repo_path().join("git_import_export.lock"))
            .map_err(|err| anyhow!("failed to lock Git import/export: {err}"))?;
        let options = self.policy.options();
        let mut locked = block_on(self.workspace.start_working_copy_mutation())?;
        if locked.locked_wc().old_tree().tree_ids_and_labels() != frozen_tree.tree_ids_and_labels()
        {
            return Ok(Publication::Stale(
                "another command updated the working copy".into(),
            ));
        }
        let (snapshot, _) = block_on(locked.locked_wc().snapshot(&options))?;
        if snapshot.tree_ids_and_labels() != frozen_tree.tree_ids_and_labels() {
            return Ok(Publication::Stale(
                "files in the working copy changed (left as they are)".into(),
            ));
        }
        // Checked again under the operation-heads lock below; this early look only names the
        // cause, since a jj command also changes Git's refs when it exports.
        let op_heads = base.loader().op_heads_store();
        if block_on(op_heads.get_op_heads())? != self.heads {
            return Ok(Publication::Stale("another jj operation ran".into()));
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
            return Ok(Publication::Stale(
                "Git's refs or HEAD changed outside jj".into(),
            ));
        }
        drop(probe);

        // Rechecking an existing candidate can require no repository edits. It still
        // needs the same stale-input guard before it may be pushed, without creating
        // an artificial maintenance operation for a no-op.
        if !tx.repo().has_changes() {
            let _lock = block_on(op_heads.lock())?;
            return Ok(if block_on(op_heads.get_op_heads())? == self.heads {
                Publication::Done(base.op_id().clone())
            } else {
                Publication::Stale("another jj operation ran".into())
            });
        }

        let published = block_on(tx.write(description))?.leave_unpublished();
        {
            let _lock = block_on(op_heads.lock())?;
            if block_on(op_heads.get_op_heads())? != self.heads {
                return Ok(Publication::Stale("another jj operation ran".into()));
            }
            block_on(op_heads.update_op_heads(&self.heads, published.op_id()))?;
        }
        let op = published.op_id().clone();
        self.repo = published.clone();
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
            let mut tx = published.start_transaction();
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
            Ok(block_on(tx.commit("jj-fork: export to Git"))?)
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
                    format!("moved {}", moved.iter().cloned().collect::<Vec<_>>().join(", "))
                }
            ))),
        }
    }
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
