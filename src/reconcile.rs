//! Reconciles local bookmarks in the fork's namespaces with the fork remote.
//!
//! A clone can carry bookmarks that predate changes on the remote: an Amp orb starts from an old
//! snapshot and then updates Git with plain `git fetch`, which jj never sees. Left alone, a
//! series deleted or renamed on the remote would be merged into the fork branch and pushed back,
//! and a glue restacked on the remote would become a conflicted bookmark. So before anything
//! else, each local bookmark is compared with the remote's:
//!
//! 1. The remote has it and local is behind it: move local to the remote's commit.
//! 2. The remote has it and jj marks local conflicted: set it to the remote's commit.
//! 3. The remote has it and local is ahead or diverged: keep it (unpushed work) and report.
//!    Exception, 3b: if a diverged local commit is already reachable from a remote ref, nothing
//!    unpushed would be lost and the remote rewrote it (a restacked glue, a rebased series), so
//!    take the remote's commit.
//! 4. The remote lacks it but the commit is reachable from a remote ref: it was published and
//!    deleted, so forget the local bookmark.
//! 5. The remote lacks it and the commit is not on the remote: new local work, keep it.
//!
//! A remote bookmark that is itself conflicted is not absence: such a bookmark is kept and
//! reported. Nothing on the remote is ever changed here; the result is recorded in jj's view in
//! the caller's transaction.

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::git::REMOTE_NAME_FOR_LOCAL_GIT_REPO;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::Repo as _;
use jj_lib::transaction::Transaction;

use crate::config::Config;
use crate::native;
use crate::report;
use crate::sync::short;

/// A local bookmark as jj sees it: one commit, or several when conflicted.
pub struct Local {
    pub name: String,
    pub commits: Vec<String>,
    pub conflicted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Already equal to the remote, or nothing to compare.
    Unchanged,
    /// Rules 1 and 2: set the bookmark to the remote's commit.
    MoveToRemote { rule: &'static str, commit: String },
    /// Rule 3: keep unpushed local work. `diverged` is false when plainly ahead.
    KeepUnpushed { diverged: bool },
    /// Rule 4: published and since deleted on the remote.
    Forget,
    /// Rule 5: new local work.
    KeepNew,
}

/// Decides what to do with one local bookmark. `is_ancestor(a, b)` says whether `a` is an
/// ancestor of `b` (or equal), and `on_remote(c)` whether `c` is reachable from any remote ref.
/// Errors from either (a graph that cannot be read) are errors, never "no".
pub fn decide(
    local: &Local,
    remote: Option<&str>,
    is_ancestor: impl Fn(&str, &str) -> Result<bool>,
    on_remote: impl Fn(&str) -> Result<bool>,
) -> Result<Decision> {
    Ok(match remote {
        Some(theirs) => {
            if local.conflicted {
                return Ok(Decision::MoveToRemote {
                    rule: "2",
                    commit: theirs.to_string(),
                });
            }
            let ours = local.commits[0].as_str();
            if ours == theirs {
                Decision::Unchanged
            } else if is_ancestor(ours, theirs)? {
                Decision::MoveToRemote {
                    rule: "1",
                    commit: theirs.to_string(),
                }
            } else if !is_ancestor(theirs, ours)? && on_remote(ours)? {
                // Diverged, but our commit is already on the remote, so nothing unpushed is lost:
                // the remote rewrote (restacked or rebased) what we have.
                Decision::MoveToRemote {
                    rule: "3b",
                    commit: theirs.to_string(),
                }
            } else {
                Decision::KeepUnpushed {
                    diverged: !is_ancestor(theirs, ours)?,
                }
            }
        }
        None => {
            let mut published = !local.commits.is_empty();
            for commit in &local.commits {
                published &= on_remote(commit)?;
            }
            if published {
                Decision::Forget
            } else {
                Decision::KeepNew
            }
        }
    })
}

/// The fork remote's side of one bookmark.
enum Remote {
    Absent,
    At(String),
    Conflicted,
}

/// Local bookmarks in the fork's namespaces (fork branch, mirror, series, glue), each with the
/// fork remote's side.
fn read(repo: &dyn jj_lib::repo::Repo, config: &Config) -> Vec<(Local, Remote)> {
    let fork = &config.fork;
    let in_namespace = |name: &str| {
        name == fork.branch
            || fork.mirror_branch.as_deref() == Some(name)
            || name.starts_with(&fork.glue_prefix)
            || fork
                .series_prefixes
                .iter()
                .any(|p| name.starts_with(p.as_str()))
    };
    let view = repo.view();
    let remote = RemoteName::new(&fork.remote);
    view.local_bookmarks()
        .filter(|(name, target)| in_namespace(name.as_str()) && target.is_present())
        .map(|(name, target)| {
            let local = Local {
                name: name.as_str().to_string(),
                commits: target.added_ids().map(|id| id.hex()).collect(),
                conflicted: target.has_conflict(),
            };
            let theirs = &view
                .get_remote_bookmark(name.to_remote_symbol(remote))
                .target;
            let theirs = if theirs.has_conflict() {
                Remote::Conflicted
            } else if let Some(id) = theirs.as_normal() {
                Remote::At(id.hex())
            } else {
                Remote::Absent
            };
            (local, theirs)
        })
        .collect()
}

/// Brings every local bookmark in the fork's namespaces in line with the fork remote, in `tx`,
/// and reports each change as a `reconciled:` line.
pub fn reconcile(tx: &mut Transaction, config: &Config) -> Result<()> {
    let remote = config.fork.remote.clone();
    let remote_name = RemoteName::new(&remote);
    let base = tx.base_repo().clone();
    let repo = base.as_ref();
    let remote_heads: Vec<CommitId> = repo
        .view()
        .remote_bookmarks(remote_name)
        .flat_map(|(_, r)| r.target.added_ids())
        .cloned()
        .collect();
    let id = |hex: &str| native::parse_id(hex);
    let is_ancestor = |a: &str, b: &str| native::is_ancestor(repo, &id(a)?, &id(b)?);
    let on_remote = |c: &str| -> Result<bool> {
        let c = id(c)?;
        for head in &remote_heads {
            if native::is_ancestor(repo, &c, head)? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    for (local, theirs) in read(repo, config) {
        let name = &local.name;
        let theirs = match theirs {
            Remote::Conflicted => {
                report(&format!(
                    "reconciled: {name} kept ({name}@{remote} is conflicted; settle it with: jj git fetch)"
                ));
                continue;
            }
            Remote::At(commit) => Some(commit),
            Remote::Absent => None,
        };
        let decision = decide(&local, theirs.as_deref(), is_ancestor, on_remote)
            .with_context(|| format!("cannot reconcile {name}"))?;
        let ref_name = RefName::new(name);
        match decision {
            Decision::Unchanged | Decision::KeepNew => {}
            Decision::MoveToRemote { rule, commit } => {
                tx.repo_mut()
                    .set_local_bookmark_target(ref_name, RefTarget::normal(id(&commit)?));
                let why = match rule {
                    "1" => "local was behind",
                    "2" => "local was conflicted vs",
                    _ => "local diverged but was already published; remote rewrote",
                };
                report(&format!(
                    "reconciled: {name} -> {} (rule {rule}: {why} {name}@{remote})",
                    short(&commit)
                ));
            }
            Decision::KeepUnpushed { diverged } => {
                let how = if diverged {
                    "diverged from"
                } else {
                    "ahead of"
                };
                report(&format!(
                    "reconciled: {name} kept (rule 3: unpushed local work, {how} {name}@{remote})"
                ));
            }
            Decision::Forget => {
                forget(tx, ref_name);
                report(&format!(
                    "reconciled: {name} forgotten (rule 4: deleted on {remote} after being published)"
                ));
            }
        }
    }
    Ok(())
}

/// `jj bookmark forget`: clears the local bookmark and untracks its remote bookmarks (except
/// Git's own, which cannot be untracked), so a later fetch does not bring it back.
fn forget(tx: &mut Transaction, name: &RefName) {
    tx.repo_mut()
        .set_local_bookmark_target(name, RefTarget::absent());
    let remotes: Vec<(jj_lib::ref_name::RemoteNameBuf, RemoteRef)> = tx
        .repo()
        .view()
        .remote_views()
        .filter(|(remote, _)| *remote != REMOTE_NAME_FOR_LOCAL_GIT_REPO)
        .map(|(remote, _)| {
            let r = tx
                .repo()
                .view()
                .get_remote_bookmark(name.to_remote_symbol(remote));
            (remote.to_owned(), r.clone())
        })
        .filter(|(_, r)| r.target.is_present() || r.is_tracked())
        .collect();
    for (remote, _) in remotes {
        tx.repo_mut()
            .untrack_remote_bookmark(name.to_remote_symbol(&remote));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_ok(
        local: &Local,
        remote: Option<&str>,
        is_ancestor: impl Fn(&str, &str) -> bool,
        on_remote: impl Fn(&str) -> bool,
    ) -> Decision {
        decide(
            local,
            remote,
            |a, b| Ok(is_ancestor(a, b)),
            |c| Ok(on_remote(c)),
        )
        .unwrap()
    }

    #[test]
    fn graph_errors_are_not_absence() {
        let err = decide(
            &one("b"),
            None,
            |_, _| Ok(false),
            |_| anyhow::bail!("index unreadable"),
        );
        assert!(
            err.is_err(),
            "an unreadable graph must not forget a bookmark"
        );
        let err = decide(
            &one("a"),
            Some("c"),
            |_, _| anyhow::bail!("no"),
            |_| Ok(true),
        );
        assert!(err.is_err());
    }

    // History: a <- b <- c, and b <- d (a branch off b).
    fn ancestor(a: &str, b: &str) -> bool {
        let up: &[&str] = match b {
            "b" => &["a", "b"],
            "c" => &["a", "b", "c"],
            "d" => &["a", "b", "d"],
            _ => &[],
        };
        a == b || up.contains(&a)
    }

    fn one(commit: &str) -> Local {
        Local {
            name: "patch/x".into(),
            commits: vec![commit.into()],
            conflicted: false,
        }
    }

    fn published(c: &str) -> bool {
        matches!(c, "a" | "b" | "c")
    }

    // The remote's history: a <- b <- c, plus e rewritten from d.
    fn ancestor_rewritten(a: &str, b: &str) -> bool {
        ancestor(a, b) || (b == "e" && matches!(a, "a" | "b"))
    }

    #[test]
    fn rule_1_behind_moves_to_remote() {
        assert_eq!(
            decide_ok(&one("a"), Some("c"), ancestor, published),
            Decision::MoveToRemote {
                rule: "1",
                commit: "c".into()
            }
        );
    }

    #[test]
    fn equal_is_unchanged() {
        assert_eq!(
            decide_ok(&one("c"), Some("c"), ancestor, published),
            Decision::Unchanged
        );
    }

    #[test]
    fn rule_2_conflicted_takes_remote_even_if_one_side_is_ahead() {
        let local = Local {
            name: "glue/a+b".into(),
            commits: vec!["c".into(), "d".into()],
            conflicted: true,
        };
        assert_eq!(
            decide_ok(&local, Some("b"), ancestor, published),
            Decision::MoveToRemote {
                rule: "2",
                commit: "b".into()
            }
        );
    }

    #[test]
    fn rule_3_ahead_and_diverged_are_kept() {
        assert_eq!(
            decide_ok(&one("c"), Some("b"), ancestor, published),
            Decision::KeepUnpushed { diverged: false }
        );
        assert_eq!(
            decide_ok(&one("d"), Some("c"), ancestor, published),
            Decision::KeepUnpushed { diverged: true }
        );
    }

    #[test]
    fn rule_3b_diverged_but_already_published_takes_remote() {
        // Local d is on the remote's history (say via the fork branch); the remote's bookmark
        // was rewritten to e.
        assert_eq!(
            decide_ok(&one("d"), Some("e"), ancestor_rewritten, |c| c == "d"),
            Decision::MoveToRemote {
                rule: "3b",
                commit: "e".into()
            }
        );
        assert_eq!(
            decide_ok(&one("d"), Some("e"), ancestor_rewritten, |_| false),
            Decision::KeepUnpushed { diverged: true }
        );
    }

    #[test]
    fn rule_4_published_then_deleted_is_forgotten() {
        assert_eq!(
            decide_ok(&one("b"), None, ancestor, published),
            Decision::Forget
        );
    }

    #[test]
    fn rule_5_unpublished_without_remote_is_kept() {
        assert_eq!(
            decide_ok(&one("d"), None, ancestor, published),
            Decision::KeepNew
        );
        let mixed = Local {
            name: "x".into(),
            commits: vec!["a".into(), "d".into()],
            conflicted: true,
        };
        assert_eq!(
            decide_ok(&mixed, None, ancestor, published),
            Decision::KeepNew
        );
    }
}
