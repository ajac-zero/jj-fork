//! `jj fork init`: prepares a clone of the fork. Idempotent, so agent sandboxes can run it on
//! every start.

use std::path::Path;

use anyhow::{Context, Result, bail};
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::{RefNameBuf, RemoteName};
use jj_lib::repo::Repo as _;
use jj_lib::transaction::Transaction;

use crate::config::{Config, FILE_NAME};
use crate::native;
use crate::repo::Repo;
use crate::{progress, run};

/// Writes a starter config when none exists.
pub fn write_starter_config(root: &Path, upstream_url: &str) -> Result<()> {
    let path = root.join(FILE_NAME);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let text = format!(
        r#"# jj-fork configuration. Commit this file; personal overrides go in jj config under jj-fork.*.

[upstream]
url = "{upstream_url}"
# remote = "upstream"
# branch = "main"

[fork]
# remote = "origin"
# branch = "fork/main"        # generated merge of upstream and every patch
# series_prefixes = ["patch/"]   # e.g. ["patch/", "tooling/"]
# glue_prefix = "glue/"          # glue/<a>+<b> holds only the resolution between two series
# mirror_branch = "main"      # fast-forward a fork branch that mirrors upstream

# Checks run on each stale patch replayed onto upstream, then on the fork-branch candidate.
# Placeholders: {{go_packages}}, {{jobs}}. Set kind = "go-test" for go test commands.
[checks]
patch = [
  # {{ name = "build", run = "make build", tier = "medium" }},
  # {{ name = "test", run = "make test", tier = "high" }},
]
fork = [
  # {{ name = "test", run = "make test" }},
]
"#
    );
    std::fs::write(&path, text)?;
    progress(&format!(
        "wrote {}; fill in the checks and commit it",
        path.display()
    ));
    Ok(())
}

/// Prepares the clone. Bootstrap steps with no jj repository yet (`jj git init --colocate`) and
/// explicit config writes (`jj config set`) use the jj CLI; everything else is native.
pub fn init(repo: &Repo, config: &Config) -> Result<()> {
    let root = &repo.root;
    let fork = &config.fork.remote;
    // Fetch every branch of the fork, even from a single-branch clone.
    repo.git(&[
        "config",
        "--replace-all",
        &format!("remote.{fork}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{fork}/*"),
    ])?;
    // A shallow boundary hides commit parents from jj. Fetch the full history, then rebuild jj's
    // commit index from it; the operation log, jj-only commits, and config stay.
    if repo.is_shallow()? {
        progress("unshallowing the clone");
        repo.git(&["fetch", "--quiet", "--unshallow", fork])?;
        if root.join(".jj").exists() {
            crate::native::reindex(root)?;
        }
    }
    if !root.join(".jj").exists() {
        progress("colocating jj with the Git checkout");
        run::output_all(root, "jj", &["git", "init", "--colocate"])?;
    }
    crate::config::store_copy(root);
    let upstream = &config.upstream.remote;
    match repo.git(&["remote", "get-url", upstream]) {
        Err(_) => {
            repo.git(&["remote", "add", upstream, &config.upstream.url])?;
        }
        // The committed URL is authoritative: an upstream that moved or was renamed must not
        // leave existing clones fetching from the old address.
        Ok(current) if current != config.upstream.url => {
            repo.git(&["remote", "set-url", upstream, &config.upstream.url])?;
            progress(&format!(
                "updated remote {upstream} from {current} to {}",
                config.upstream.url
            ));
        }
        Ok(_) => {}
    }

    let mut native = native::Native::open_for_init(root)?;
    native.fetch_remote(fork)?;
    native.fetch_remote(upstream)?;
    native.track_fork_bookmarks(config)?;

    // Revset aliases for people working in the repo by hand, and auto-tracking so a series or
    // glue that someone else pushed becomes a local bookmark on the next fetch.
    let f = &config.fork;
    let series = f
        .series_prefixes
        .iter()
        .map(|p| format!("bookmarks(glob:{:?})", format!("{p}*")))
        .collect::<Vec<_>>()
        .join(" | ");
    let aliases = [
        ("trunk()", format!("exactly({}, 1)", config.upstream_ref())),
        ("fork_patches()", series),
        (
            "fork_glue()",
            format!("bookmarks(glob:{:?})", format!("{}*", f.glue_prefix)),
        ),
        (
            "fork_parents()",
            "heads(trunk() | fork_patches() | fork_glue())".to_string(),
        ),
        (
            "fork_head()",
            format!("exactly(bookmarks(exact:{:?}), 1)", f.branch),
        ),
    ];
    for (name, value) in aliases {
        repo.jj(&[
            "config",
            "set",
            "--repo",
            &format!("revset-aliases.{name:?}"),
            &value,
        ])?;
    }
    repo.jj(&[
        "config",
        "set",
        "--repo",
        &format!("remotes.{fork}.auto-track-bookmarks"),
        &tracked_patterns(config).join(" | "),
    ])?;

    // Commits need an author for jj to push them; borrow Git's identity when jj has none.
    let jj_config = native::jj_config::load(root, native::CommandContext::Fork)?;
    for key in ["name", "email"] {
        let jj_value: String = jj_config.get(["user", key]).unwrap_or_default();
        let git_value = repo
            .git(&["config", &format!("user.{key}")])
            .unwrap_or_default();
        if jj_value.is_empty() && !git_value.is_empty() {
            repo.jj(&[
                "config",
                "set",
                "--user",
                &format!("user.{key}"),
                &git_value,
            ])?;
        }
    }

    let native = native::Native::load(root)?;
    let target = native.resolve_single(&config.upstream_ref())?;
    let series = native::bookmarks_with(native.repo.as_ref(), &config.fork.series_prefixes);
    progress(&format!(
        "ready: upstream {} at {}, {} series",
        config.upstream_ref(),
        &target.hex()[..12],
        series.len()
    ));
    Ok(())
}

/// Bookmark patterns of the fork on its remote: the fork branch, mirror, series, and glues.
fn tracked_patterns(config: &Config) -> Vec<String> {
    let f = &config.fork;
    let mut patterns = vec![f.branch.clone()];
    patterns.extend(f.mirror_branch.clone());
    patterns.extend(f.series_prefixes.iter().map(|p| format!("{p}*")));
    patterns.push(format!("{}*", f.glue_prefix));
    patterns
}

/// Whether `name` is one of the fork's bookmarks: the fork branch, mirror, a series, or a glue.
fn is_fork_bookmark(config: &Config, name: &str) -> bool {
    let f = &config.fork;
    name == f.branch
        || f.mirror_branch.as_deref() == Some(name)
        || name.starts_with(&f.glue_prefix)
        || f.series_prefixes
            .iter()
            .any(|p| name.starts_with(p.as_str()))
}

/// Tracks the fork's untracked bookmarks on its remote, in `tx`, so they exist locally (as
/// `jj bookmark track` does). A tracked remote bookmark whose local bookmark is absent is a
/// deliberate deletion waiting to be pushed, and stays deleted.
pub fn track(tx: &mut Transaction, config: &Config) -> Result<()> {
    let remote = RemoteName::new(&config.fork.remote);
    let untracked: Vec<RefNameBuf> = tx
        .repo()
        .view()
        .remote_bookmarks(remote)
        .filter(|(name, r)| {
            is_fork_bookmark(config, name.as_str()) && !r.is_tracked() && r.target.is_present()
        })
        .map(|(name, _)| name.to_owned())
        .collect();
    for name in untracked {
        tx.repo_mut()
            .track_remote_bookmark(name.to_remote_symbol(remote))
            .with_context(|| format!("failed to track {}@{}", name.as_str(), remote.as_str()))?;
    }
    Ok(())
}

/// Adds the `jj fork` alias to the user's jj config.
pub fn install_alias(dir: &Path) -> Result<()> {
    run::output(
        dir,
        "jj",
        &[
            "config",
            "set",
            "--user",
            "aliases.fork",
            r#"["util", "exec", "--", "jj-fork"]"#,
        ],
    )?;
    progress("added `aliases.fork` to your jj config; `jj fork` now runs jj-fork");
    Ok(())
}

#[cfg(test)]
mod tests {
    use jj_lib::ref_name::{RefName, RemoteName};

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        run::output(dir, "git", args).unwrap();
    }

    /// An untracked remote series becomes a local bookmark; a tracked remote series whose local
    /// bookmark was deleted is a pending deletion and stays deleted; non-fork names are ignored.
    #[test]
    fn track_adopts_untracked_series_but_keeps_deliberate_deletions() {
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
        for branch in ["patch/theirs", "patch/deleted", "other"] {
            git(&src, &["branch", branch]);
        }
        git(dir.path(), &["clone", "-q", "--bare", "src", "remote.git"]);
        git(dir.path(), &["clone", "-q", "remote.git", "work"]);
        let work = dir.path().join("work");
        let jj = |args: &[&str]| run::output(&work, "jj", args).unwrap();
        jj(&["git", "init", "--colocate"]);
        jj(&["bookmark", "track", "patch/deleted", "--remote", "origin"]);
        jj(&["bookmark", "delete", "patch/deleted"]);

        let config: Config = toml::from_str("[upstream]\nurl = 'u'\n").unwrap();
        let native = crate::native::Native::load(&work).unwrap();
        let mut tx = native.start();
        track(&mut tx, &config).unwrap();
        let view = tx.repo().view();
        let origin = RemoteName::new("origin");
        let local = |name: &str| view.get_local_bookmark(RefName::new(name)).is_present();
        let tracked = |name: &str| {
            view.get_remote_bookmark(RefName::new(name).to_remote_symbol(origin))
                .is_tracked()
        };
        assert!(
            local("patch/theirs") && tracked("patch/theirs"),
            "untracked series adopted"
        );
        assert!(!local("patch/deleted"), "pending deletion resurrected");
        assert!(
            tracked("patch/deleted"),
            "pending deletion must stay tracked"
        );
        assert!(
            !local("other") && !tracked("other"),
            "non-fork bookmark tracked"
        );
    }
}
