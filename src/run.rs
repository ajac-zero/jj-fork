//! Process helpers. jj-fork prepares clones and pushes with the pinned `jj` CLI, materializes
//! check worktrees with `git`, and runs the configured check commands as child processes.
//! Maintenance itself runs in-process through jj-lib (see `native`).

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Runs a program and returns its trimmed stdout. Fails with stderr when it exits non-zero.
pub fn output(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to start {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs a program and returns its combined stdout and stderr, ignoring the exit status.
pub fn output_all(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to start {program}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// Runs a program for its exit status, discarding output.
pub fn succeeds(dir: &Path, program: &str, args: &[&str]) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to start {program}"))?;
    Ok(status.success())
}

/// Runs a program with inherited stdout and stderr, failing when it exits non-zero.
pub fn run(dir: &Path, program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .with_context(|| format!("failed to start {program}"))?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

/// Runs a shell command, appending its output to `log`. Returns whether it succeeded.
pub fn shell(dir: &Path, command: &str, env: &[(String, String)], log: &Path) -> Result<bool> {
    let file = File::options()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("failed to open {}", log.display()))?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .with_context(|| format!("failed to run: {command}"))?;
    Ok(status.success())
}
