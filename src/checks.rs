//! Configured checks: shell commands run in a worktree, with the go-test failure comparison
//! against upstream and the generated-files check.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{Check, CheckKind, Config, Generated};
use crate::repo::{Repo, Worktree};
use crate::run;

/// Shared state for one jj-fork invocation's checks.
pub struct Checker<'a> {
    pub repo: &'a Repo,
    pub config: &'a Config,
    pub target: String,
    pub scratch: PathBuf,
    pub log_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub jobs: usize,
    baseline: Option<Worktree>,
    /// Tests that fail on upstream too, as `package test` lines.
    pub upstream_failures: BTreeSet<String>,
}

pub enum Outcome {
    Pass,
    Fail {
        check: String,
        tier: String,
        log: PathBuf,
    },
}

impl<'a> Checker<'a> {
    pub fn new(
        repo: &'a Repo,
        config: &'a Config,
        target: String,
        scratch: PathBuf,
        log_dir: PathBuf,
    ) -> Checker<'a> {
        let (jobs, env) = resource_env(config);
        Checker {
            repo,
            config,
            target,
            scratch,
            log_dir,
            env,
            jobs,
            baseline: None,
            upstream_failures: BTreeSet::new(),
        }
    }

    /// Runs `checks` in `dir` in order, stopping at the first failure. `generated_if_changed`
    /// makes the generated-files check conditional on the inputs changing.
    pub fn run(
        &mut self,
        dir: &Path,
        checks: &[Check],
        generated_if_changed: bool,
        log_name: &str,
    ) -> Result<Outcome> {
        let log = self
            .log_dir
            .join(format!("{}.log", log_name.replace('/', "_")));
        let _ = std::fs::remove_file(&log);
        let packages = go_packages(dir, &self.target)?;
        for check in checks {
            let packages_arg = packages.join(" ");
            if check.when.as_deref() == Some("go_packages") && packages.is_empty() {
                continue;
            }
            let command = check
                .run
                .replace("{go_packages}", &packages_arg)
                .replace("{jobs}", &self.jobs.to_string());
            append(
                &log,
                &format!("jj-fork-check: {}\n$ {command}\n", check.name),
            )?;
            let ok = match check.kind {
                CheckKind::Command => run::shell(dir, &command, &self.env, &log)?,
                CheckKind::GoTest => self.go_test(dir, &command, &log)?,
            };
            if !ok {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let tier = match check.low_if_errors_at_most {
                    Some(max) if compiler_errors(&text) <= max => "low".to_string(),
                    _ => check.tier.clone(),
                };
                return Ok(Outcome::Fail {
                    check: check.name.clone(),
                    tier,
                    log,
                });
            }
        }
        if let Some(generated) = &self.config.generated {
            let changed = run::output(dir, "git", &["diff", "--name-only", &self.target, "HEAD"])?;
            let inputs = globs(&generated.inputs)?;
            let relevant = !generated_if_changed || changed.lines().any(|f| inputs.is_match(f));
            if relevant {
                append(&log, "jj-fork-check: generated\n")?;
                if !generated_current(dir, generated, &self.env, &log)? {
                    return Ok(Outcome::Fail {
                        check: "generated".into(),
                        tier: "low".into(),
                        log,
                    });
                }
            }
        }
        Ok(Outcome::Pass)
    }

    /// Runs a go-test check. A failing test is retried once, then run on upstream at the
    /// target; tests that fail there too are recorded and ignored.
    fn go_test(&mut self, dir: &Path, command: &str, log: &Path) -> Result<bool> {
        let attempt = self.scratch.join("go-test.log");
        let _ = std::fs::remove_file(&attempt);
        if run::shell(dir, command, &self.env, &attempt)? {
            append(log, &std::fs::read_to_string(&attempt).unwrap_or_default())?;
            return Ok(true);
        }
        let text = std::fs::read_to_string(&attempt).unwrap_or_default();
        append(log, &text)?;
        let mut real = false;
        for failure in go_test_failures(&text) {
            let Some(test) = failure.test else {
                append(
                    log,
                    &format!(
                        "jj-fork: {} failed without a failing test (build failure)\n",
                        failure.package
                    ),
                )?;
                real = true;
                continue;
            };
            let retry = format!("go test -count=1 -run '^{test}$' {}", failure.package);
            let scratch_log = self.scratch.join("retry.log");
            if run::shell(dir, &retry, &self.env, &scratch_log)? {
                append(
                    log,
                    &format!(
                        "jj-fork: note: {} {test} failed once, then passed on retry\n",
                        failure.package
                    ),
                )?;
                continue;
            }
            let baseline = self.baseline()?;
            if run::shell(&baseline, &retry, &self.env, &scratch_log)? {
                append(
                    log,
                    &format!(
                        "jj-fork: {} {test} fails here but passes on upstream\n",
                        failure.package
                    ),
                )?;
                real = true;
            } else {
                append(
                    log,
                    &format!(
                        "jj-fork: note: {} {test} also fails on upstream; ignoring\n",
                        failure.package
                    ),
                )?;
                self.upstream_failures
                    .insert(format!("{} {test}", failure.package));
            }
        }
        Ok(!real)
    }

    fn baseline(&mut self) -> Result<PathBuf> {
        if self.baseline.is_none() {
            self.baseline = Some(
                self.repo
                    .worktree(&self.scratch, "baseline", &self.target)?,
            );
        }
        Ok(self.baseline.as_ref().unwrap().path.clone())
    }
}

/// Lowers parallelism and sets the configured environment on machines with little memory.
fn resource_env(config: &Config) -> (usize, Vec<(String, String)>) {
    let mib = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    if mib == 0 || mib >= config.low_memory.below_mib {
        return (cpus, Vec::new());
    }
    let jobs = if mib < 4096 { 1 } else { 2 };
    let limit = (mib * 60 / 100).to_string();
    let env = config
        .low_memory
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.replace("{jobs}", &jobs.to_string())
                    .replace("{memory_limit_mib}", &limit),
            )
        })
        .collect();
    (jobs, env)
}

/// Go package directories (as `./dir`) containing `.go` files changed between `target` and HEAD.
fn go_packages(dir: &Path, target: &str) -> Result<Vec<String>> {
    let changed = run::output(
        dir,
        "git",
        &["diff", "--name-only", target, "HEAD", "--", "*.go"],
    )?;
    let dirs: BTreeSet<String> = changed
        .lines()
        .filter_map(|f| {
            Path::new(f)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
        })
        .collect();
    Ok(dirs
        .into_iter()
        .filter(|d| {
            let path = if d.is_empty() {
                dir.to_path_buf()
            } else {
                dir.join(d)
            };
            std::fs::read_dir(path)
                .map(|entries| {
                    entries
                        .flatten()
                        .any(|e| e.path().extension().is_some_and(|x| x == "go"))
                })
                .unwrap_or(false)
        })
        .map(|d| {
            if d.is_empty() {
                ".".to_string()
            } else {
                format!("./{d}")
            }
        })
        .collect())
}

fn generated_current(
    dir: &Path,
    generated: &Generated,
    env: &[(String, String)],
    log: &Path,
) -> Result<bool> {
    if !run::shell(dir, &generated.regenerate, env, log)? {
        return Ok(false);
    }
    if let Some(header) = &generated.header {
        let status = run::output(
            dir,
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        for line in status.lines() {
            let file = dir.join(line.get(3..).unwrap_or_default());
            restore_header(&file, &header.text, &header.comments)?;
        }
    }
    let status = run::output(
        dir,
        "git",
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if !status.is_empty() {
        append(
            log,
            &format!("generated files are out of date:\n{status}\n"),
        )?;
        return Ok(false);
    }
    Ok(true)
}

/// Prepends the license header to a file whose generator dropped it.
fn restore_header(
    file: &Path,
    text: &str,
    comments: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let Some(prefix) = file
        .extension()
        .and_then(|e| comments.get(&e.to_string_lossy().to_string()))
    else {
        return Ok(());
    };
    let Ok(content) = std::fs::read_to_string(file) else {
        return Ok(());
    };
    let first = text.lines().next().unwrap_or_default();
    if content.lines().next().is_some_and(|l| l.contains(first)) {
        return Ok(());
    }
    let mut header: String = text.lines().map(|l| format!("{prefix} {l}\n")).collect();
    header.push('\n');
    std::fs::write(file, header + &content)?;
    Ok(())
}

pub fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p)?);
    }
    Ok(builder.build()?)
}

fn append(log: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::options()
        .create(true)
        .append(true)
        .open(log)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

#[derive(Debug, PartialEq)]
pub struct GoTestFailure {
    pub package: String,
    pub test: Option<String>,
}

/// Parses `go test` output into failing top-level tests per package. A package that failed
/// without a failing test (a build failure) has `test: None`.
pub fn go_test_failures(output: &str) -> Vec<GoTestFailure> {
    let mut failures = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("--- FAIL: ") {
            if let Some(name) = rest.split_whitespace().next() {
                pending.push(name.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("FAIL\t") {
            let package = rest.split('\t').next().unwrap_or_default().to_string();
            if pending.is_empty() {
                failures.push(GoTestFailure {
                    package,
                    test: None,
                });
                continue;
            }
            for test in pending.drain(..) {
                failures.push(GoTestFailure {
                    package: package.clone(),
                    test: Some(test),
                });
            }
        }
    }
    failures
}

/// Counts compiler-style `path:line:col: message` lines.
pub fn compiler_errors(log: &str) -> usize {
    log.lines()
        .filter(|line| {
            let mut parts = line.splitn(4, ':');
            let (Some(path), Some(row), Some(col), Some(_)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return false;
            };
            !path.is_empty()
                && !path.contains(' ')
                && !row.is_empty()
                && row.chars().all(|c| c.is_ascii_digit())
                && !col.is_empty()
                && col.chars().all(|c| c.is_ascii_digit())
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_test_failures_attribute_tests_to_packages() {
        let output = "\
--- FAIL: TestA (0.00s)
    a_test.go:10: boom
    --- FAIL: TestA/sub (0.00s)
--- FAIL: TestB (0.01s)
FAIL
FAIL\texample.com/m/a\t0.1s
ok  \texample.com/m/b\t0.2s
# example.com/m/c
c.go:3:5: undefined: x
FAIL\texample.com/m/c [build failed]
";
        assert_eq!(
            go_test_failures(output),
            vec![
                GoTestFailure {
                    package: "example.com/m/a".into(),
                    test: Some("TestA".into())
                },
                GoTestFailure {
                    package: "example.com/m/a".into(),
                    test: Some("TestB".into())
                },
                GoTestFailure {
                    package: "example.com/m/c [build failed]".into(),
                    test: None
                },
            ]
        );
    }

    #[test]
    fn compiler_errors_count_only_located_messages() {
        let log = "# pkg\na/b.go:10:19: undefined: X\nc.go:1:2: bad\nnote: something: else\nhttp://host:80: no\n";
        assert_eq!(compiler_errors(log), 2);
    }

    #[test]
    fn header_is_prepended_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.yaml");
        std::fs::write(&file, "---\nkind: X\n").unwrap();
        let comments = [("yaml".to_string(), "#".to_string())]
            .into_iter()
            .collect();
        restore_header(&file, "Copyright A\nLine two", &comments).unwrap();
        restore_header(&file, "Copyright A\nLine two", &comments).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# Copyright A\n# Line two\n\n---\nkind: X\n"
        );
    }
}
