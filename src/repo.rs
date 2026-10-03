//! jj CLI and git queries against the colocated repository, for preparation and checks.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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

    pub fn bookmark_revset(name: &str) -> String {
        format!("bookmarks(exact:{name:?})")
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
