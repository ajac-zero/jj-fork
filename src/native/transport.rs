//! Fetch, probe, and push through jj's Git transport: the configured `git.executable-path`,
//! inheriting this process's environment (credential helpers, SSH agent, askpass) without
//! changing it.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::TryStreamExt as _;
use jj_cli::revset_util::{parse_remote_fetch_bookmarks, parse_remote_fetch_tags};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::git::{
    self, FetchTagsOverride, GitFetch, GitFetchRefExpression, GitProgress, GitPushError,
    GitPushOptions, GitPushRefTargets, GitSidebandLineTerminator, GitSubprocessCallback,
    expand_fetch_refspecs, load_default_fetch_bookmarks,
};
use jj_lib::merge::Diff;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as JjRepo};
use jj_lib::revset::{RevsetDiagnostics, RevsetExpression, RevsetStreamExt as _, SymbolResolver};
use jj_lib::str_util::StringExpression;
use serde::Serialize;

use super::jj_config::{CommandContext, JjEnv, cli_error};
use super::prepare::rebase_mutable_descendants;
use super::{Native, block_on};
use crate::progress;

/// One bookmark to push: the remote must be at `before` (absent for a new bookmark), and will be
/// set to `after`. Deletions are not supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushUpdate {
    pub name: String,
    pub before: Option<CommitId>,
    pub after: CommitId,
}

/// What happened to each requested update. Once the network was attempted, a failure never means
/// "nothing pushed": refs whose result is unknown say so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushReport {
    pub remote: String,
    pub refs: Vec<PushedRef>,
    /// The push itself failed (network, authentication, protocol): the remote side of every
    /// `Unknown` ref is unknown.
    pub transport_error: Option<String>,
    /// The remote accepted updates, but recording them locally (the remote-tracking bookmark and
    /// Git's `refs/remotes`) failed. A later fetch repairs it; nothing was rolled back.
    pub bookkeeping_error: Option<String>,
}

impl PushReport {
    /// Every requested update was accepted or already matched, and bookkeeping succeeded. An
    /// empty report (nothing to push) is a success.
    pub fn all_accepted(&self) -> bool {
        self.transport_error.is_none()
            && self.bookkeeping_error.is_none()
            && self.refs.iter().all(|r| {
                matches!(
                    r.outcome,
                    PushOutcome::Accepted | PushOutcome::Skipped { .. }
                )
            })
    }

    /// Human-readable lines, one per ref, then any errors.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .refs
            .iter()
            .map(|r| {
                let what = match &r.outcome {
                    PushOutcome::Accepted => "pushed".to_string(),
                    PushOutcome::Skipped { reason } => format!("skipped: {reason}"),
                    PushOutcome::Rejected { reason } => format!("rejected: {reason}"),
                    PushOutcome::RemoteRejected { reason } => {
                        format!("rejected by the remote: {reason}")
                    }
                    PushOutcome::Unknown { reason } => format!("unknown: {reason}"),
                };
                format!("{}@{} {}: {what}", r.name, self.remote, &r.after[..12])
            })
            .collect();
        lines.extend(
            self.transport_error
                .iter()
                .map(|e| format!("push to {} failed: {e}", self.remote)),
        );
        lines.extend(
            self.bookkeeping_error
                .iter()
                .map(|e| format!("pushed, but recording it locally failed: {e}")),
        );
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushedRef {
    pub name: String,
    /// Full hex commit ids.
    pub before: Option<String>,
    pub after: String,
    pub outcome: PushOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PushOutcome {
    Accepted,
    /// Not sent: the remote already has `after`.
    Skipped {
        reason: String,
    },
    /// Refused before the remote updated it: the remote was not at `before` (the lease) or the
    /// update was otherwise rejected by Git.
    Rejected {
        reason: String,
    },
    /// The remote refused it (a hook or protection rule).
    RemoteRejected {
        reason: String,
    },
    /// The push failed in a way that leaves the remote's state unknown.
    Unknown {
        reason: String,
    },
}

/// Discards Git's progress output; relays remote messages to stderr.
struct Quiet;

impl GitSubprocessCallback for Quiet {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _: &GitProgress) -> std::io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        message: &[u8],
        _: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        relay(message);
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        message: &[u8],
        _: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        relay(message);
        Ok(())
    }
}

fn relay(message: &[u8]) {
    let text = String::from_utf8_lossy(message);
    let text = text.trim();
    if !text.is_empty() {
        progress(&format!("remote: {text}"));
    }
}

impl Native {
    /// Fetches `remote` as `jj git fetch --remote` does (its configured bookmarks and tags),
    /// imports the result, and publishes it as an operation on the current head. The frozen state
    /// advances to that operation.
    pub fn fetch_remote(&mut self, remote: &str) -> Result<Arc<ReadonlyRepo>> {
        let _lock = self.git_lock()?;
        self.repo = block_on(self.workspace.repo_loader().load_at_head())?;
        let remote_name = RemoteName::new(remote);
        let env = self.context_env(CommandContext::Fetch)?;
        let mut tx = self.start();
        let settings = env.settings.clone();
        let remote_settings = settings.remote_settings()?;
        let ui = Ui::null();
        let git_repo = git::get_git_backend(tx.repo().store())?.git_repo();
        let bookmark = match parse_remote_fetch_bookmarks(&ui, &remote_settings, remote_name)
            .map_err(cli_error)?
        {
            Some(expr) => expr,
            None => {
                load_default_fetch_bookmarks(remote_name, &git_repo)
                    .map_err(|err| anyhow!("cannot fetch {remote}: {err}"))?
                    .1
            }
        };
        let (tag, no_implicit_tags) =
            match parse_remote_fetch_tags(&ui, &remote_settings, remote_name).map_err(cli_error)? {
                Some(expr) => (expr, true),
                None => (StringExpression::none(), false),
            };
        let expanded = expand_fetch_refspecs(remote_name, GitFetchRefExpression { bookmark, tag })
            .map_err(|err| anyhow!("cannot fetch {remote}: {err}"))?;
        let import_options = env.import_options()?;
        let mut fetch = GitFetch::new(tx.repo_mut(), env.subprocess_options()?, &import_options)?;
        fetch
            .fetch(
                remote_name,
                expanded,
                &mut Quiet,
                None,
                no_implicit_tags.then_some(FetchTagsOverride::NoTags),
            )
            .with_context(|| format!("failed to fetch {remote}"))?;
        block_on(fetch.import_refs()).with_context(|| format!("failed to import {remote}"))?;
        if tx.repo().has_changes() {
            rebase_mutable_descendants(&env, self.workspace.workspace_name(), &mut tx)?;
            self.finish_tx(tx, &format!("fetch from git remote {remote}"))?;
        }
        self.heads = vec![self.repo.op_id().clone()];
        Ok(self.repo.clone())
    }

    /// Observes `remote`'s branches without changing anything jj or the source repository
    /// tracks: Git fetches them into a private namespace (`refs/jj-fork/probe/`) that jj never
    /// imports, the observation is recorded as the remote's bookmarks in an unpublished operation
    /// on the frozen one, and the private refs are removed. Fetched objects stay in Git's object
    /// store, as with any fetch.
    pub fn probe_remote(&self, remote: &str) -> Result<Arc<ReadonlyRepo>> {
        // Serialize use of the temporary namespace with other probes and jj imports.
        let _lock = self.git_lock()?;
        let options = self
            .context_env(CommandContext::Fetch)?
            .subprocess_options()?;
        let backend = git::get_git_backend(self.repo.store())?;
        let git_dir = backend.git_repo_path().to_path_buf();
        let namespace = format!("refs/jj-fork/probe/{remote}/");
        let git = |args: &[&str]| -> Result<String> {
            let output = Command::new(&options.executable_path)
                .arg("--git-dir")
                .arg(&git_dir)
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "submodule.recurse=false",
                ])
                .args(args)
                .envs(&options.environment)
                .stdin(Stdio::null())
                .output()
                .with_context(|| format!("failed to run {}", options.executable_path.display()))?;
            if !output.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.first().unwrap_or(&""),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        };
        let clear = || -> Result<()> {
            let refs = git(&["for-each-ref", "--format=delete %(refname)", &namespace])?;
            if refs.trim().is_empty() {
                return Ok(());
            }
            let mut child = Command::new(&options.executable_path)
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["update-ref", "--stdin"])
                .envs(&options.environment)
                .stdin(Stdio::piped())
                .spawn()?;
            use std::io::Write as _;
            child
                .stdin
                .take()
                .context("no stdin for git update-ref")?
                .write_all(refs.as_bytes())?;
            if !child.wait()?.success() {
                bail!("failed to remove {namespace}");
            }
            Ok(())
        };
        clear()?;
        let observed = git(&[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            // Without an empty refmap Git also updates the remote's configured tracking refs.
            "--refmap=",
            "--quiet",
            remote,
            &format!("+refs/heads/*:{namespace}*"),
        ])
        .and_then(|_| {
            git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &namespace,
            ])
        });
        let cleared = clear();
        let observed = observed.with_context(|| format!("failed to observe {remote}"))?;
        cleared?;

        let mut branches = BTreeMap::new();
        for line in observed.lines() {
            let Some((name, id)) = line.split_once(' ') else {
                continue;
            };
            let name = name.strip_prefix(&namespace).unwrap_or(name);
            branches.insert(
                name.to_string(),
                CommitId::try_from_hex(id).context("bad id")?,
            );
        }
        let remote_name = RemoteName::new(remote);
        let mut tx = self.repo.start_transaction();
        let known: Vec<(RefNameBuf, RemoteRef)> = tx
            .repo()
            .view()
            .remote_bookmarks(remote_name)
            .map(|(name, r)| (name.to_owned(), r.clone()))
            .collect();
        for (name, old) in &known {
            if !branches.contains_key(name.as_str()) {
                tx.repo_mut().set_remote_bookmark(
                    name.to_remote_symbol(remote_name),
                    RemoteRef {
                        target: RefTarget::absent(),
                        state: old.state,
                    },
                );
            }
        }
        let mut commits = Vec::new();
        for (name, id) in &branches {
            let name = RefName::new(name);
            let state = known
                .iter()
                .find(|(known, _)| known.as_str() == name.as_str())
                .map_or(RemoteRefState::New, |(_, r)| r.state);
            commits.push(self.repo.store().get_commit(id)?);
            tx.repo_mut().set_remote_bookmark(
                name.to_remote_symbol(remote_name),
                RemoteRef {
                    target: RefTarget::normal(id.clone()),
                    state,
                },
            );
        }
        block_on(tx.repo_mut().add_heads(&commits))?;
        Ok(block_on(tx.write(format!("jj-fork: observe {remote}")))?.leave_unpublished())
    }

    /// Pushes exactly `updates` to `remote`, each leased on its `before`, the way
    /// `jj git push --bookmark` would push it. Refuses (with an error, before any network) what
    /// `jj git push` refuses: a conflicted local or remote bookmark, an untracked remote bookmark,
    /// a lease that disagrees with the recorded remote bookmark, commits without a description
    /// or identity, conflicted or private commits, and commits `git.sign-on-push` would rewrite.
    /// Accepted updates are recorded locally in a new operation.
    pub fn push_explicit(&mut self, remote: &str, updates: &[PushUpdate]) -> Result<PushReport> {
        let mut report = PushReport {
            remote: remote.to_string(),
            refs: Vec::new(),
            transport_error: None,
            bookkeeping_error: None,
        };
        if updates.is_empty() {
            return Ok(report);
        }
        let _lock = self.git_lock()?;
        self.repo = block_on(self.workspace.repo_loader().load_at_head())?;
        let repo = self.repo.clone();
        let remote_name = RemoteName::new(remote);
        let mut names = BTreeSet::new();
        let mut targets = GitPushRefTargets::default();
        for update in updates {
            if !names.insert(update.name.as_str()) {
                bail!("{} is listed twice in one push", update.name);
            }
            let name = RefName::new(&update.name);
            let local = repo.view().get_local_bookmark(name);
            if local.has_conflict() {
                bail!("won't push {}: the bookmark is conflicted", update.name);
            }
            if local.as_normal() != Some(&update.after) {
                bail!(
                    "won't push {}: the local bookmark moved since its target was validated",
                    update.name
                );
            }
            let remote_ref = repo
                .view()
                .get_remote_bookmark(name.to_remote_symbol(remote_name));
            if remote_ref.target.has_conflict() {
                bail!(
                    "won't push {}: {}@{remote} is conflicted; fetch first",
                    update.name,
                    update.name
                );
            }
            if remote_ref.target.is_present() && !remote_ref.is_tracked() {
                bail!(
                    "won't push {}: the remote bookmark {}@{remote} exists but is not tracked",
                    update.name,
                    update.name
                );
            }
            if remote_ref.target.as_normal() != update.before.as_ref() {
                bail!(
                    "won't push {}: the lease expects {}@{remote} at {}, but jj records {}",
                    update.name,
                    update.name,
                    describe(update.before.as_ref()),
                    describe(remote_ref.target.as_normal())
                );
            }
            let tracked_elsewhere = repo
                .view()
                .remote_bookmarks_matching(
                    &jj_lib::str_util::StringMatcher::exact(update.name.as_str()),
                    &jj_lib::str_util::StringMatcher::all(),
                )
                .any(|(symbol, r)| {
                    symbol.remote != remote_name
                        && symbol.remote.as_str() != git::REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str()
                        && r.is_tracked()
                });
            if update.before.is_none() && tracked_elsewhere {
                bail!(
                    "won't push {}: refusing to create {}@{remote} for a bookmark tracked on another remote",
                    update.name,
                    update.name
                );
            }
            if update.before.as_ref() == Some(&update.after) {
                report.refs.push(pushed_ref(
                    update,
                    PushOutcome::Skipped {
                        reason: "the remote already has it".into(),
                    },
                ));
                continue;
            }
            targets.bookmarks.push((
                RefNameBuf::from(update.name.as_str()),
                Diff::new(update.before.clone(), Some(update.after.clone())),
            ));
        }
        if targets.bookmarks.is_empty() {
            return Ok(report);
        }
        // Push policy comes from the configuration as jj resolves it for `git push` now.
        let env = self.context_env(CommandContext::Push)?;
        self.validate_commits(&env, repo.as_ref(), remote_name, &targets)?;

        let mut tx = self.start();
        let result = git::push_refs(
            tx.repo_mut(),
            env.subprocess_options()?,
            remote_name,
            &targets,
            &mut Quiet,
            &GitPushOptions::default(),
        );
        let requested = updates.iter().filter(|u| {
            targets
                .bookmarks
                .iter()
                .any(|(name, _)| name.as_str() == u.name)
        });
        let stats = match result {
            Ok(stats) => stats,
            Err(err @ (GitPushError::NoSuchRemote(_) | GitPushError::RemoteName(_))) => {
                return Err(err).context("nothing was pushed");
            }
            Err(err) => {
                let reason = err.to_string();
                for update in requested {
                    report.refs.push(pushed_ref(
                        update,
                        PushOutcome::Unknown {
                            reason: "the push failed before the remote reported a result".into(),
                        },
                    ));
                }
                report.transport_error = Some(reason);
                return Ok(report);
            }
        };
        let full = |name: &str| format!("refs/heads/{name}");
        let any_accepted = !stats.pushed.is_empty();
        for update in requested {
            let qualified = full(&update.name);
            let find = |list: &[(jj_lib::ref_name::GitRefNameBuf, Option<String>)]| {
                list.iter()
                    .find(|(name, _)| name.as_str() == qualified)
                    .map(|(_, reason)| reason.clone().unwrap_or_else(|| "no reason given".into()))
            };
            let outcome = if stats.pushed.iter().any(|name| name.as_str() == qualified) {
                PushOutcome::Accepted
            } else if let Some(reason) = find(&stats.rejected) {
                PushOutcome::Rejected { reason }
            } else if let Some(reason) = find(&stats.remote_rejected) {
                PushOutcome::RemoteRejected { reason }
            } else {
                PushOutcome::Unknown {
                    reason: "Git reported no result for this ref".into(),
                }
            };
            report.refs.push(pushed_ref(update, outcome));
        }
        if !stats.unexported_bookmarks.is_empty() {
            report.bookkeeping_error = Some(format!(
                "could not update Git's remote-tracking refs for {}",
                stats
                    .unexported_bookmarks
                    .iter()
                    .map(|(symbol, _)| symbol.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if any_accepted
            && let Err(err) = self.finish_tx(tx, &format!("push bookmarks to git remote {remote}"))
        {
            let previous = report.bookkeeping_error.take();
            report.bookkeeping_error = Some(match previous {
                Some(previous) => format!("{previous}; {err:#}"),
                None => format!("{err:#}"),
            });
        }
        Ok(report)
    }

    /// The commit checks `jj git push` makes, over every commit the push would publish.
    fn validate_commits(
        &self,
        env: &JjEnv,
        repo: &dyn JjRepo,
        remote: &RemoteName,
        targets: &GitPushRefTargets,
    ) -> Result<()> {
        let known: Vec<CommitId> = repo
            .view()
            .remote_bookmarks(remote)
            .flat_map(|(_, r)| r.target.added_ids())
            .cloned()
            .collect();
        let heads: Vec<CommitId> = targets
            .bookmarks
            .iter()
            .filter_map(|(_, d)| d.after.clone())
            .collect();
        let name = self.workspace.workspace_name();
        let immutable = env.immutable_heads(repo, name)?;
        let resolver = SymbolResolver::new(repo, env.extensions.symbol_resolvers());
        let immutable = immutable
            .resolve_user_expression(repo, &resolver)
            .map_err(|err| anyhow!("invalid revset-aliases.immutable_heads(): {err}"))?;
        let range = RevsetExpression::commits(known)
            .union(&immutable)
            .range(&RevsetExpression::commits(heads));
        let private = env.settings.get_string("git.private-commits")?;
        let context = env.revset_context(repo, name);
        let private_expr = jj_lib::revset::parse(&mut RevsetDiagnostics::new(), &private, &context)
            .map_err(|err| anyhow!("invalid git.private-commits: {err}"))?
            .resolve_user_expression(repo, &resolver)
            .map_err(|err| anyhow!("invalid git.private-commits: {err}"))?;
        let private_revset = private_expr.evaluate(repo)?;
        let is_private = private_revset.containing_fn();
        let sign_on_push = env.settings.get_bool("git.sign-on-push")?;
        let commits: Vec<_> = block_on(
            range
                .evaluate(repo)?
                .stream()
                .commits(repo.store())
                .try_collect(),
        )?;
        for commit in commits {
            let mut reasons = Vec::new();
            if commit.description().is_empty() {
                reasons.push("has no description".to_string());
            }
            if commit.author().name.is_empty()
                || commit.author().email.is_empty()
                || commit.committer().name.is_empty()
                || commit.committer().email.is_empty()
            {
                reasons.push("has no author and/or committer set".into());
            }
            if commit.has_conflict() {
                reasons.push("has conflicts".into());
            }
            if is_private(commit.id())? {
                reasons.push(format!("is private (git.private-commits: '{private}')"));
            }
            if sign_on_push && !commit.is_signed() {
                reasons.push(
                    "is unsigned, and git.sign-on-push would rewrite the checked commit; sign commits when they are created (signing.behavior) or disable git.sign-on-push".into(),
                );
            }
            if !reasons.is_empty() {
                bail!(
                    "won't push: commit {} {}",
                    &commit.id().hex()[..12],
                    reasons.join(" and ")
                );
            }
        }
        Ok(())
    }
}

fn pushed_ref(update: &PushUpdate, outcome: PushOutcome) -> PushedRef {
    PushedRef {
        name: update.name.clone(),
        before: update.before.as_ref().map(|id| id.hex()),
        after: update.after.hex(),
        outcome,
    }
}

fn describe(id: Option<&CommitId>) -> String {
    id.map_or_else(|| "nothing".to_string(), |id| id.hex()[..12].to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use jj_lib::object_id::ObjectId as _;
    use jj_lib::op_store::OperationId;
    use jj_lib::ref_name::{RefName, RemoteName};
    use jj_lib::repo::Repo as _;

    use crate::native::{Native, Publication, block_on};
    use crate::run;

    fn git(dir: &Path, args: &[&str]) -> String {
        run::output(dir, "git", args).unwrap()
    }

    /// A bare remote with one commit on main, and a colocated jj clone of it.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("a"), "a\n").unwrap();
        git(&src, &["add", "."]);
        git(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "a",
            ],
        );
        let remote = dir.path().join("remote.git");
        git(dir.path(), &["clone", "-q", "--bare", "src", "remote.git"]);
        git(dir.path(), &["clone", "-q", "remote.git", "work"]);
        let work = dir.path().join("work");
        run::output(&work, "jj", &["git", "init", "--colocate"]).unwrap();
        run::output(
            &work,
            "jj",
            &["bookmark", "track", "main", "--remote", "origin"],
        )
        .unwrap();
        (dir, remote, work)
    }

    fn advance_remote(dir: &Path) -> String {
        let src = dir.join("src");
        std::fs::write(src.join("b"), "b\n").unwrap();
        git(&src, &["add", "."]);
        git(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "b",
            ],
        );
        git(&src, &["push", "-q", "../remote.git", "main"]);
        git(&src, &["rev-parse", "HEAD"])
    }

    #[test]
    fn probe_observes_the_remote_without_touching_the_source() {
        let (dir, _remote, work) = fixture();
        let native = Native::load(&work).unwrap();
        // jj's GC-protection refs (refs/jj/keep) are never imported; everything else must stay.
        let refs = || {
            git(
                &work,
                &[
                    "for-each-ref",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                    "refs/jj-fork",
                ],
            )
        };
        let refs_before = refs();
        let moved = advance_remote(dir.path());
        let observed = native.probe_remote("origin").unwrap();
        let main = observed
            .view()
            .get_remote_bookmark(RefName::new("main").to_remote_symbol(RemoteName::new("origin")));
        assert_eq!(main.target.as_normal().unwrap().hex(), moved);
        assert!(main.is_tracked(), "tracking state is kept");
        assert_eq!(refs(), refs_before, "no Git ref changed");
        let heads = block_on(native.repo.loader().op_heads_store().get_op_heads()).unwrap();
        assert_eq!(heads, native.op_heads(), "the observation is not published");
        // The observation is an operation built on the frozen one, so it loads as prepared.
        let loaded = native.load_prepared_operation(observed.op_id()).unwrap();
        assert_eq!(loaded.op_id(), observed.op_id());
    }

    #[test]
    fn push_refuses_a_target_that_no_longer_matches_the_local_bookmark() {
        let (_dir, remote, work) = fixture();
        let mut native = Native::load(&work).unwrap();
        let main = crate::native::bookmark(native.repo.as_ref(), "main")
            .unwrap()
            .unwrap();
        let tree = native.repo.store().get_commit(&main).unwrap().tree();
        let mut tx = native.start();
        let candidate = block_on(
            tx.repo_mut()
                .new_commit(vec![main.clone()], tree)
                .set_description("checked candidate")
                .write(),
        )
        .unwrap();
        let before = git(&remote, &["for-each-ref"]);
        let err = native
            .push_explicit(
                "origin",
                &[super::PushUpdate {
                    name: "main".into(),
                    before: Some(main),
                    after: candidate.id().clone(),
                }],
            )
            .unwrap_err();
        assert!(err.to_string().contains("local bookmark moved"), "{err}");
        assert_eq!(git(&remote, &["for-each-ref"]), before);
    }

    #[test]
    fn verify_frozen_sees_new_auto_tracked_files_without_recording_them() {
        let (_dir, _remote, work) = fixture();
        let mut native = Native::load(&work).unwrap();
        assert_eq!(native.verify_frozen().unwrap(), None);
        std::fs::write(work.join("new-file"), "unsnapshotted\n").unwrap();
        let reason = native
            .verify_frozen()
            .unwrap()
            .expect("a new file is a change");
        assert!(
            reason.contains("files in the working copy changed"),
            "{reason}"
        );
        let heads = block_on(native.repo.loader().op_heads_store().get_op_heads()).unwrap();
        assert_eq!(heads, native.op_heads(), "verification published nothing");
        // Nothing was recorded, so a fresh load still sees the file as unsnapshotted.
        let mut again = Native::load(&work).unwrap();
        assert!(again.verify_frozen().unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(work.join("new-file")).unwrap(),
            "unsnapshotted\n"
        );
    }

    #[test]
    fn prepared_operations_publish_only_onto_the_frozen_source() {
        let (_dir, _remote, work) = fixture();
        let mut native = Native::load(&work).unwrap();
        let base = native.repo.op_id().clone();
        let unknown = OperationId::new(vec![0xab; 64]);
        let err = native.load_prepared_operation(&unknown).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");

        // A proposal that changes nothing is verified, not published.
        let no_op = block_on(native.start().write("no-op"))
            .unwrap()
            .leave_unpublished();
        match native.publish_prepared(no_op, "no-op").unwrap() {
            Publication::Done(op) => assert_eq!(op, base),
            Publication::Stale(why) => panic!("stale: {why}"),
        }

        // A real proposal is refused once another operation has run.
        let mut tx = native.start();
        let main = native
            .repo
            .view()
            .get_local_bookmark(RefName::new("main"))
            .as_normal()
            .unwrap()
            .clone();
        crate::native::set_bookmark(&mut tx, "patch/x", &main);
        let proposal = block_on(tx.write("propose")).unwrap().leave_unpublished();
        run::output(&work, "jj", &["bookmark", "create", "other", "-r", "main"]).unwrap();
        match native
            .publish_prepared(proposal.clone(), "propose")
            .unwrap()
        {
            Publication::Stale(why) => assert!(why.contains("operation"), "{why}"),
            Publication::Done(_) => panic!("published onto a moved operation log"),
        }
        let refs = git(&work, &["for-each-ref", "refs/heads/patch/"]);
        assert!(refs.is_empty(), "nothing exported: {refs}");

        // On a fresh freeze it publishes and exports.
        let mut native = Native::load(&work).unwrap();
        let mut tx = native.start();
        crate::native::set_bookmark(&mut tx, "patch/x", &main);
        let proposal = block_on(tx.write("propose")).unwrap().leave_unpublished();
        assert!(matches!(
            native.publish_prepared(proposal, "propose").unwrap(),
            Publication::Done(_)
        ));
        assert_eq!(git(&work, &["rev-parse", "patch/x"]), main.hex());
    }
}
