//! Configuration. Repository facts live in a committed `.jj-fork.toml` so every clone, CI job,
//! and agent sandbox sees them. Personal overrides live in jj config under `jj-fork.*` with the
//! same structure, for example `jj config set --user jj-fork.fork.remote mine`. They are read
//! from jj's effective configuration in-process, never by listing it. The whole effective config
//! serializes, so a saved plan can fingerprint it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = ".jj-fork.toml";
/// Copy kept inside the jj repository, so checkouts of bookmarks without the committed file (a
/// series is rooted on upstream) still find the configuration.
const STORED: &str = ".jj/repo/jj-fork.toml";

/// Chooses the configuration file: an explicit path, else the stored copy of the committed file
/// (refreshed from it), else the committed file. Using the stored copy everywhere makes saved
/// plans independent of which checkout created or applies them; its content is fingerprinted.
pub fn resolve_path(root: &Path, explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let tracked = root.join(FILE_NAME);
    let stored = root.join(STORED);
    if tracked.exists() {
        store_copy(root);
    }
    if stored.exists() { stored } else { tracked }
}

/// Best effort: a read-only repository directory only loses the fallback.
pub fn store_copy(root: &Path) {
    let tracked = root.join(FILE_NAME);
    let stored = root.join(STORED);
    if !root.join(".jj/repo").is_dir() {
        return;
    }
    if let Ok(text) = std::fs::read(&tracked)
        && std::fs::read(&stored).ok().as_deref() != Some(text.as_slice())
    {
        let _ = std::fs::write(&stored, text);
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub upstream: Upstream,
    #[serde(default)]
    pub fork: Fork,
    #[serde(default)]
    pub checks: Checks,
    #[serde(default)]
    pub generated: Option<Generated>,
    #[serde(default)]
    pub tiers: Tiers,
    #[serde(default)]
    pub low_memory: LowMemory,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    #[serde(default = "default_upstream_remote")]
    pub remote: String,
    pub url: String,
    #[serde(default = "default_main")]
    pub branch: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    #[serde(default = "default_origin")]
    pub remote: String,
    /// The generated branch that merges upstream with every patch.
    #[serde(default = "default_fork_branch")]
    pub branch: String,
    /// Bookmark prefixes of the series rooted on upstream, for example `patch/` for features
    /// meant for upstream and `tooling/` for fork-only tooling.
    #[serde(default = "default_series_prefixes")]
    pub series_prefixes: Vec<String>,
    /// Bookmark prefix of glues: merges of two or more series that hold only their resolution.
    #[serde(default = "default_glue_prefix")]
    pub glue_prefix: String,
    /// A fork branch that mirrors upstream and is fast-forwarded on sync, if any.
    #[serde(default)]
    pub mirror_branch: Option<String>,
    #[serde(default = "default_merge_message")]
    pub merge_message: String,
}

impl Default for Fork {
    fn default() -> Self {
        Fork {
            remote: default_origin(),
            branch: default_fork_branch(),
            series_prefixes: default_series_prefixes(),
            glue_prefix: default_glue_prefix(),
            mirror_branch: None,
            merge_message: default_merge_message(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Checks {
    /// Checks for each stale patch, run on its replay onto the upstream target.
    #[serde(default)]
    pub patch: Vec<Check>,
    /// Checks for a fork-branch candidate, run before the branch moves.
    #[serde(default)]
    pub fork: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    /// Shell command. Placeholders: `{go_packages}` (Go package directories the patch changes),
    /// `{jobs}` (parallelism, lower on small machines).
    pub run: String,
    /// Skip the check when this placeholder expands to nothing.
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub kind: CheckKind,
    /// Difficulty tier of a fixer when this check fails on a patch.
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Lower the tier to `low` when the log has at most this many compiler errors.
    #[serde(default)]
    pub low_if_errors_at_most: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    /// Any failure fails the check.
    #[default]
    Command,
    /// `go test` output: failing tests are retried, then compared with upstream. Tests that
    /// also fail on upstream are reported and ignored.
    GoTest,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Generated {
    /// Globs of generated files. They are regenerated instead of merged and do not count
    /// toward conflict difficulty.
    pub paths: Vec<String>,
    /// Command that regenerates every generated file.
    pub regenerate: String,
    /// Patch checks regenerate only when the patch changes files matching these globs.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// License header to restore on regenerated files whose generator drops it.
    #[serde(default)]
    pub header: Option<Header>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub text: String,
    /// Comment prefix by file extension, for example `{ go = "//", yaml = "#" }`.
    pub comments: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tiers {
    #[serde(default = "default_low_max")]
    pub low_max: Limits,
    #[serde(default = "default_medium_max")]
    pub medium_max: Limits,
}

impl Default for Tiers {
    fn default() -> Self {
        Tiers {
            low_max: default_low_max(),
            medium_max: default_medium_max(),
        }
    }
}

/// Upper bounds for a conflict tier, measured at the first conflicting commit.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub files: usize,
    pub hunks: usize,
    pub lines: usize,
    pub commits: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LowMemory {
    /// Machines with less memory than this run with reduced parallelism.
    #[serde(default = "default_low_memory_mib")]
    pub below_mib: u64,
    /// Environment for check commands on small machines. Placeholders: `{jobs}`,
    /// `{memory_limit_mib}` (60% of physical memory).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Default for LowMemory {
    fn default() -> Self {
        LowMemory {
            below_mib: default_low_memory_mib(),
            env: BTreeMap::new(),
        }
    }
}

fn default_upstream_remote() -> String {
    "upstream".into()
}
fn default_origin() -> String {
    "origin".into()
}
fn default_main() -> String {
    "main".into()
}
fn default_fork_branch() -> String {
    "fork/main".into()
}
fn default_series_prefixes() -> Vec<String> {
    vec!["patch/".into()]
}
fn default_glue_prefix() -> String {
    "glue/".into()
}
fn default_merge_message() -> String {
    "internal: assemble fork patch set".into()
}
fn default_tier() -> String {
    "medium".into()
}
fn default_low_max() -> Limits {
    Limits {
        files: 2,
        hunks: 3,
        lines: 60,
        commits: 3,
    }
}
fn default_medium_max() -> Limits {
    Limits {
        files: 12,
        hunks: 20,
        lines: 400,
        commits: 10,
    }
}
fn default_low_memory_mib() -> u64 {
    8192
}

impl Config {
    /// Loads the committed config, then applies `jj-fork.*` overrides from the effective jj
    /// config (user, repo, workspace, and environment layers, as jj resolves them), and checks
    /// that the bookmark roles are unambiguous.
    pub fn load(root: &Path, path: Option<&Path>) -> Result<Config> {
        let path = resolve_path(root, path);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read {}; run `jj fork init` first",
                path.display()
            )
        })?;
        let mut table: toml::Table =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        let jj = crate::native::jj_config::load(root, crate::native::CommandContext::Fork)?;
        if let Some(overrides) = crate::native::jj_config::fork_overrides(&jj)? {
            merge(&mut table, overrides);
        }
        let config: Config = toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("invalid configuration in {} or jj config", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Every bookmark must have exactly one role: fork branch, mirror, series, or glue.
    pub fn validate(&self) -> Result<()> {
        let fork = &self.fork;
        if fork.series_prefixes.is_empty() {
            bail!("fork.series_prefixes must name at least one prefix");
        }
        if fork.series_prefixes.iter().any(String::is_empty) || fork.glue_prefix.is_empty() {
            bail!("series and glue prefixes must not be empty");
        }
        let mut seen = std::collections::BTreeSet::new();
        for prefix in &fork.series_prefixes {
            if !seen.insert(prefix) {
                bail!("fork.series_prefixes lists {prefix} twice");
            }
            if prefix.starts_with(&fork.glue_prefix)
                || fork.glue_prefix.starts_with(prefix.as_str())
            {
                bail!(
                    "series prefix {prefix} and glue prefix {} overlap, so a bookmark could be both",
                    fork.glue_prefix
                );
            }
        }
        let roles: Vec<&str> = fork
            .series_prefixes
            .iter()
            .chain(std::iter::once(&fork.glue_prefix))
            .map(String::as_str)
            .collect();
        let mut named = vec![("fork.branch", fork.branch.as_str())];
        named.extend(
            fork.mirror_branch
                .as_deref()
                .map(|m| ("fork.mirror_branch", m)),
        );
        for (key, name) in &named {
            if name.is_empty() {
                bail!("{key} must not be empty");
            }
            if let Some(prefix) = roles.iter().find(|p| name.starts_with(**p)) {
                bail!("{key} {name} is inside the series or glue prefix {prefix}");
            }
        }
        if fork.mirror_branch.as_deref() == Some(fork.branch.as_str()) {
            bail!("fork.mirror_branch must differ from fork.branch");
        }
        if fork.remote == self.upstream.remote {
            bail!(
                "fork.remote and upstream.remote are both {}; they must be different remotes",
                fork.remote
            );
        }
        if self.upstream.branch.is_empty() {
            bail!("upstream.branch must not be empty");
        }
        Ok(())
    }

    pub fn upstream_ref(&self) -> String {
        format!("{}@{}", self.upstream.branch, self.upstream.remote)
    }
}

fn merge(base: &mut toml::Table, overrides: toml::Table) {
    for (key, value) in overrides {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(value)) => merge(base, value),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn overrides_merge_into_nested_tables() {
        let mut base: toml::Table = toml::from_str(
            "[upstream]\nurl = \"a\"\nbranch = \"main\"\n[fork]\nremote = \"origin\"\n",
        )
        .unwrap();
        let overrides: toml::Table = toml::from_str(
            "fork.remote = \"mine\"\nupstream.branch = \"trunk\"\nchecks.fork = [{ name = \"t\", run = \"true\" }]\n",
        )
        .unwrap();
        merge(&mut base, overrides);
        let config: Config = toml::Value::Table(base).try_into().unwrap();
        assert_eq!(config.fork.remote, "mine");
        assert_eq!(config.upstream.branch, "trunk");
        assert_eq!(config.upstream.url, "a");
        assert_eq!(config.fork.branch, "fork/main");
        assert_eq!(config.checks.fork.len(), 1);
    }

    #[test]
    fn roles_must_not_overlap() {
        assert!(parse("[upstream]\nurl = 'u'\n").validate().is_ok());
        let cases = [
            ("[fork]\nseries_prefixes = []", "at least one"),
            ("[fork]\nseries_prefixes = ['']", "not be empty"),
            ("[fork]\nglue_prefix = ''", "not be empty"),
            ("[fork]\nseries_prefixes = ['patch/', 'patch/']", "twice"),
            (
                "[fork]\nseries_prefixes = ['g']\nglue_prefix = 'glue/'",
                "overlap",
            ),
            ("[fork]\nseries_prefixes = ['glue/x/']", "overlap"),
            ("[fork]\nbranch = 'patch/fork'", "inside"),
            ("[fork]\nmirror_branch = 'glue/main'", "inside"),
            ("[fork]\nmirror_branch = 'fork/main'", "differ"),
            ("[fork]\nremote = 'upstream'", "different remotes"),
        ];
        for (text, message) in cases {
            let err = parse(&format!("[upstream]\nurl = 'u'\n{text}\n"))
                .validate()
                .unwrap_err()
                .to_string();
            assert!(err.contains(message), "{text}: {err}");
        }
    }

    #[test]
    fn effective_config_serializes() {
        let config = parse(
            "[upstream]\nurl = 'u'\n[checks]\nfork = [{ name = 't', run = 'true', kind = 'go-test' }]\n",
        );
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["fork"]["branch"], "fork/main");
        assert_eq!(json["checks"]["fork"][0]["kind"], "go-test");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let result: Result<Config, _> = toml::from_str("[upstream]\nurl = \"a\"\nbrnach = \"x\"\n");
        assert!(result.is_err());
    }
}
