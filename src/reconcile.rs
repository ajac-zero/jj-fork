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
//! Nothing on the remote is ever changed here.

use anyhow::Result;

use crate::config::Config;
use crate::repo::Repo;
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
pub fn decide(
    local: &Local,
    remote: Option<&str>,
    is_ancestor: impl Fn(&str, &str) -> bool,
    on_remote: impl Fn(&str) -> bool,
) -> Decision {
    match remote {
        Some(theirs) => {
            if local.conflicted {
                return Decision::MoveToRemote {
                    rule: "2",
                    commit: theirs.to_string(),
                };
            }
            let ours = local.commits[0].as_str();
            if ours == theirs {
                Decision::Unchanged
            } else if is_ancestor(ours, theirs) {
                Decision::MoveToRemote {
                    rule: "1",
                    commit: theirs.to_string(),
                }
            } else if !is_ancestor(theirs, ours) && on_remote(ours) {
                // Diverged, but our commit is already on the remote, so nothing unpushed is lost:
                // the remote rewrote (restacked or rebased) what we have.
                Decision::MoveToRemote {
                    rule: "3b",
                    commit: theirs.to_string(),
                }
            } else {
                Decision::KeepUnpushed {
                    diverged: !is_ancestor(theirs, ours),
                }
            }
        }
        None if !local.commits.is_empty() && local.commits.iter().all(|c| on_remote(c)) => {
            Decision::Forget
        }
        None => Decision::KeepNew,
    }
}

/// Reads local bookmarks and the fork remote's commits for the fork's namespaces.
fn read(repo: &Repo, config: &Config) -> Result<Vec<(Local, Option<String>)>> {
    let fork = &config.fork;
    let mut args: Vec<String> = vec!["bookmark".into(), "list".into(), "--color=never".into()];
    args.push("--all-remotes".into());
    args.push(format!("exact:{:?}", fork.branch));
    args.extend(fork.mirror_branch.iter().map(|m| format!("exact:{m:?}")));
    args.extend(fork.series_prefixes.iter().map(|p| format!("glob:{p}*")));
    args.push(format!("glob:{}*", fork.glue_prefix));
    args.extend([
        "-T".into(),
        r#"name ++ "\t" ++ if(remote, remote, "") ++ "\t" ++ present ++ "\t" ++ conflict ++ "\t" ++ added_targets.map(|c| c.commit_id()).join(",") ++ "\n""#.into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.jj(&args)?;
    let mut locals: Vec<Local> = Vec::new();
    let mut remotes = std::collections::BTreeMap::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 5 || f[2] != "true" {
            continue;
        }
        if f[1].is_empty() {
            locals.push(Local {
                name: f[0].to_string(),
                commits: f[4].split(',').map(str::to_string).collect(),
                conflicted: f[3] == "true",
            });
        } else if f[1] == fork.remote && !f[4].contains(',') {
            remotes.insert(f[0].to_string(), f[4].to_string());
        }
    }
    Ok(locals
        .into_iter()
        .map(|l| {
            let remote = remotes.get(&l.name).cloned();
            (l, remote)
        })
        .collect())
}

/// Brings every local bookmark in the fork's namespaces in line with the fork remote and
/// reports each change as a `reconciled:` line.
pub fn reconcile(repo: &Repo, config: &Config) -> Result<()> {
    let remote = &config.fork.remote;
    for (local, theirs) in read(repo, config)? {
        let decision = decide(
            &local,
            theirs.as_deref(),
            |a, b| repo.is_ancestor(a, b).unwrap_or(false),
            |c| {
                repo.revs(&format!(
                    "{c} & ::remote_bookmarks(remote=exact:{remote:?})"
                ))
                .map(|ids| !ids.is_empty())
                .unwrap_or(false)
            },
        );
        let name = &local.name;
        match decision {
            Decision::Unchanged | Decision::KeepNew => {}
            Decision::MoveToRemote { rule, commit } => {
                repo.jj(&[
                    "bookmark",
                    "set",
                    name,
                    "-r",
                    &commit,
                    "--allow-backwards",
                    "--quiet",
                ])?;
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
                repo.jj(&["bookmark", "forget", name, "--quiet"])?;
                report(&format!(
                    "reconciled: {name} forgotten (rule 4: deleted on {remote} after being published)"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            decide(&one("a"), Some("c"), ancestor, published),
            Decision::MoveToRemote {
                rule: "1",
                commit: "c".into()
            }
        );
    }

    #[test]
    fn equal_is_unchanged() {
        assert_eq!(
            decide(&one("c"), Some("c"), ancestor, published),
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
            decide(&local, Some("b"), ancestor, published),
            Decision::MoveToRemote {
                rule: "2",
                commit: "b".into()
            }
        );
    }

    #[test]
    fn rule_3_ahead_and_diverged_are_kept() {
        assert_eq!(
            decide(&one("c"), Some("b"), ancestor, published),
            Decision::KeepUnpushed { diverged: false }
        );
        assert_eq!(
            decide(&one("d"), Some("c"), ancestor, published),
            Decision::KeepUnpushed { diverged: true }
        );
    }

    #[test]
    fn rule_3b_diverged_but_already_published_takes_remote() {
        // Local d is on the remote's history (say via the fork branch); the remote's bookmark
        // was rewritten to e.
        assert_eq!(
            decide(&one("d"), Some("e"), ancestor_rewritten, |c| c == "d"),
            Decision::MoveToRemote {
                rule: "3b",
                commit: "e".into()
            }
        );
        assert_eq!(
            decide(&one("d"), Some("e"), ancestor_rewritten, |_| false),
            Decision::KeepUnpushed { diverged: true }
        );
    }

    #[test]
    fn rule_4_published_then_deleted_is_forgotten() {
        assert_eq!(
            decide(&one("b"), None, ancestor, published),
            Decision::Forget
        );
    }

    #[test]
    fn rule_5_unpublished_without_remote_is_kept() {
        assert_eq!(
            decide(&one("d"), None, ancestor, published),
            Decision::KeepNew
        );
        let mixed = Local {
            name: "x".into(),
            commits: vec!["a".into(), "d".into()],
            conflicted: true,
        };
        assert_eq!(decide(&mixed, None, ancestor, published), Decision::KeepNew);
    }
}
