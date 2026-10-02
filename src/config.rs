//! Configuration. Repository facts live in a committed `.jj-fork.toml` so every clone, CI job,
//! and agent sandbox sees them. Personal overrides live in jj config under `jj-fork.*` with the
//! same structure, for example `jj config set --user jj-fork.fork.remote mine`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::run;

pub const FILE_NAME: &str = ".jj-fork.toml";

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    #[serde(default = "default_upstream_remote")]
    pub remote: String,
    pub url: String,
    #[serde(default = "default_main")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checks {
    /// Checks for each stale patch, run on its replay onto the upstream target.
    #[serde(default)]
    pub patch: Vec<Check>,
    /// Checks for a fork-branch candidate, run before the branch moves.
    #[serde(default)]
    pub fork: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    /// Any failure fails the check.
    #[default]
    Command,
    /// `go test` output: failing tests are retried, then compared with upstream. Tests that
    /// also fail on upstream are reported and ignored.
    GoTest,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub text: String,
    /// Comment prefix by file extension, for example `{ go = "//", yaml = "#" }`.
    pub comments: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
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
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub files: usize,
    pub hunks: usize,
    pub lines: usize,
    pub commits: usize,
}

#[derive(Debug, Deserialize)]
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
    /// Loads the committed config, then applies `jj-fork.*` overrides from jj config.
    pub fn load(root: &Path, path: Option<&Path>) -> Result<Config> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join(FILE_NAME));
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read {}; run `jj fork init` first",
                path.display()
            )
        })?;
        let mut table: toml::Table =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        let overrides = run::output(root, "jj", &["config", "list", "jj-fork"]).unwrap_or_default();
        merge(&mut table, parse_overrides(&overrides)?);
        let config: Config = toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("invalid configuration in {} or jj config", path.display()))?;
        Ok(config)
    }

    pub fn upstream_ref(&self) -> String {
        format!("{}@{}", self.upstream.branch, self.upstream.remote)
    }
}

/// Turns `jj config list jj-fork` output (`jj-fork.a.b = value` lines) into a table.
fn parse_overrides(listing: &str) -> Result<toml::Table> {
    let dotted: String = listing
        .lines()
        .filter_map(|line| line.strip_prefix("jj-fork."))
        .map(|line| format!("{line}\n"))
        .collect();
    toml::from_str(&dotted).context("invalid jj-fork overrides in jj config")
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

    #[test]
    fn overrides_merge_into_nested_tables() {
        let mut base: toml::Table = toml::from_str(
            "[upstream]\nurl = \"a\"\nbranch = \"main\"\n[fork]\nremote = \"origin\"\n",
        )
        .unwrap();
        let overrides = parse_overrides(
            "jj-fork.fork.remote = \"mine\"\njj-fork.upstream.branch = \"trunk\"\nother.key = 1\n",
        )
        .unwrap();
        merge(&mut base, overrides);
        let config: Config = toml::Value::Table(base).try_into().unwrap();
        assert_eq!(config.fork.remote, "mine");
        assert_eq!(config.upstream.branch, "trunk");
        assert_eq!(config.upstream.url, "a");
        assert_eq!(config.fork.branch, "fork/main");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let result: Result<Config, _> = toml::from_str("[upstream]\nurl = \"a\"\nbrnach = \"x\"\n");
        assert!(result.is_err());
    }
}
