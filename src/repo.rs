//! jj and git queries against the colocated repository.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::run;

pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// Finds the repository containing `dir`.
    pub fn discover(dir: &Path) -> Result<Repo> {
        let root = run::output(dir, "git", &["rev-parse", "--show-toplevel"])
            .context("not inside a Git repository")?;
        Ok(Repo {
            root: PathBuf::from(root),
        })
    }

    pub fn jj(&self, args: &[&str]) -> Result<String> {
        run::output(&self.root, "jj", args)
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        run::output(&self.root, "git", args)
    }

    /// Resolves a revset that must name exactly one commit to its full commit id.
    pub fn rev(&self, revset: &str) -> Result<String> {
        let id = self.jj(&[
            "log",
            "--no-graph",
            "--color=never",
            "-r",
            &format!("exactly({revset}, 1)"),
            "-T",
            "commit_id",
        ])?;
        if id.is_empty() {
            bail!("revision {revset} not found");
        }
        Ok(id)
    }

    /// Lists commit ids of a revset, sorted.
    pub fn revs(&self, revset: &str) -> Result<Vec<String>> {
        let out = self.jj(&[
            "log",
            "--no-graph",
            "--color=never",
            "-r",
            revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ])?;
        let mut ids: Vec<String> = out.lines().map(str::to_string).collect();
        ids.sort();
        Ok(ids)
    }

    pub fn bookmark_revset(name: &str) -> String {
        format!("bookmarks(exact:{name:?})")
    }

    /// Local bookmarks whose names start with any of `prefixes`, skipping ones deleted locally
    /// but not yet on the remote.
    pub fn local_bookmarks(&self, prefixes: &[String]) -> Result<Vec<String>> {
        let mut args = vec![
            "bookmark".to_string(),
            "list".into(),
            "--color=never".into(),
        ];
        args.extend(prefixes.iter().map(|p| format!("glob:{p}*")));
        args.extend([
            "-T".into(),
            "if(!remote && present, name ++ \"\\n\")".into(),
        ]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.jj(&args)?;
        let mut names: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Remote-tracking refs of `remote` as `name commit` lines, for detecting concurrent pushes.
    pub fn remote_refs(&self, remote: &str) -> Result<Vec<String>> {
        let out = self.git(&[
            "for-each-ref",
            "--format=%(refname:lstrip=3) %(objectname)",
            &format!("refs/remotes/{remote}/"),
        ])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// The first local bookmark on `commit` that starts with one of `prefixes`.
    pub fn bookmark_on(&self, commit: &str, prefixes: &[String]) -> Result<Option<String>> {
        let out = self.jj(&[
            "log",
            "--no-graph",
            "--color=never",
            "-r",
            commit,
            "-T",
            "local_bookmarks.map(|b| b.name() ++ \"\\n\").join(\"\")",
        ])?;
        Ok(out
            .lines()
            .find(|name| prefixes.iter().any(|p| name.starts_with(p.as_str())))
            .map(str::to_string))
    }

    pub fn has_conflicts(&self, revset: &str) -> Result<bool> {
        Ok(!self
            .jj(&[
                "log",
                "--no-graph",
                "-r",
                &format!("({revset}) & conflicts()"),
                "-T",
                "commit_id",
            ])?
            .is_empty())
    }

    pub fn op_id(&self) -> Result<String> {
        self.jj(&["op", "log", "-n1", "--no-graph", "-T", "self.id()"])
    }

    pub fn op_restore(&self, op: &str) -> Result<()> {
        self.jj(&["op", "restore", "--quiet", op]).map(|_| ())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        run::succeeds(
            &self.root,
            "git",
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        self.git(&["merge-base", a, b])
    }

    pub fn subject(&self, commit: &str) -> Result<String> {
        self.git(&["log", "-1", "--format=%s", commit])
    }

    pub fn tree(&self, commit: &str) -> Result<String> {
        self.git(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    pub fn is_shallow(&self) -> Result<bool> {
        Ok(self.git(&["rev-parse", "--is-shallow-repository"])? == "true")
    }

    /// Creates a detached worktree at `commit` that is removed when dropped.
    pub fn worktree(&self, parent: &Path, name: &str, commit: &str) -> Result<Worktree> {
        let path = parent.join(name);
        self.git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &path.to_string_lossy(),
            commit,
        ])?;
        Ok(Worktree {
            repo: self.root.clone(),
            path,
        })
    }
}

pub struct Worktree {
    repo: PathBuf,
    pub path: PathBuf,
}

impl Worktree {
    pub fn git(&self, args: &[&str]) -> Result<String> {
        run::output(&self.path, "git", args)
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = run::succeeds(
            &self.repo,
            "git",
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        );
    }
}
