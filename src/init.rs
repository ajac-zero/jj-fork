//! `jj fork init`: prepares a clone of the fork. Idempotent, so agent sandboxes can run it on
//! every start.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{Config, FILE_NAME};
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
    // A shallow boundary hides commit parents from jj, so fetch full history and rebuild jj's
    // view of it.
    if repo.is_shallow()? {
        progress("unshallowing the clone");
        repo.git(&["fetch", "--quiet", "--unshallow", fork])?;
        if root.join(".jj").exists() {
            std::fs::remove_dir_all(root.join(".jj"))
                .context("failed to reset .jj after unshallowing")?;
        }
    }
    if !root.join(".jj").exists() {
        progress("colocating jj with the Git checkout");
        run::output_all(root, "jj", &["git", "init", "--colocate"])?;
    }
    let upstream = &config.upstream.remote;
    if repo.git(&["remote", "get-url", upstream]).is_err() {
        repo.git(&["remote", "add", upstream, &config.upstream.url])?;
    }
    repo.jj(&["git", "fetch", "--remote", fork, "--quiet"])?;
    repo.jj(&["git", "fetch", "--remote", upstream, "--quiet"])?;

    track(repo, config);

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
    for key in ["name", "email"] {
        let jj_value = repo
            .jj(&["config", "get", &format!("user.{key}")])
            .unwrap_or_default();
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

    let series = repo.local_bookmarks(&config.fork.series_prefixes)?;
    progress(&format!(
        "ready: upstream {} at {}, {} series",
        config.upstream_ref(),
        &repo.rev(&config.upstream_ref())?[..12],
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

/// Tracks the fork's bookmarks on its remote, so they exist locally after a fetch.
pub fn track(repo: &Repo, config: &Config) {
    let mut args = vec!["bookmark".to_string(), "track".into()];
    args.extend(
        tracked_patterns(config)
            .into_iter()
            .map(|p| format!("glob:{p}")),
    );
    args.extend(["--remote".into(), config.fork.remote.clone()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let _ = repo.jj(&args);
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
