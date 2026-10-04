---
name: setting-up-forks-on-amp
description: Sets up a long-lived fork to be maintained by Amp agents with jj-fork, including orb setup, project base branch, Ship prompt, AGENTS.md, and a scheduled fork owner. Use when asked to onboard a repository to Amp as a fork, configure its orbs, Ship button, or scheduled upstream sync.
---

# Setting up a fork on Amp

Amp-specific onboarding for a fork maintained with jj-fork. Day-to-day operation (sync, repair, create, retire) is in the `maintaining-forks-with-jj-fork` skill; print it with `jj fork skill maintaining-forks-with-jj-fork`. Keep operation instructions there and Amp settings here.

## Layout

- Series `patch/*` (upstreamable) and `tooling/*` (fork-only) are rooted on upstream. `fork/main` is generated from upstream plus every series and glue.
- Fork-only agent files (`.jj-fork.toml`, `AGENTS.md`, `.agents/`, skills) live in one `tooling/<name>` series, usually `tooling/agent-workflow`, so they reach `fork/main` like any other series.
- jj-fork guarantees series are structurally independent. Whether a patch builds and passes alone is not verified by jj-fork; the repo's `AGENTS.md`, Ship prompt, or CI must require it.

## One-time steps

1. Install jj-fork and jj in `.agents/setup` (idempotent). Pin versions and verify the release checksum:
   ```bash
   JJ_FORK_VERSION=<version>   # a released tag; check https://github.com/ajac-zero/jj-fork/releases
   # download jj-fork-v$JJ_FORK_VERSION-x86_64-unknown-linux-gnu.tar.gz and its .sha256, run sha256sum -c, then install the binary to ~/.local/bin
   jj-fork init                # remotes, full history, jj, tracking, revset aliases
   jj-fork alias               # optional: `jj fork` instead of `jj-fork`
   ```
   `jj` must match the version jj-fork was built against (see its README), and `git` must be >= 2.41 (put a newer git first on PATH if needed). Checks run with the caller's PATH, so put the repo's toolchain (for example `go`) on it.
2. Create `.jj-fork.toml` (`jj-fork init --upstream URL` writes a starter). Set `series_prefixes`, and `[checks]` only if the repo should run local checks.
3. Commit `.jj-fork.toml`, `.agents/`, and `AGENTS.md` to the `tooling/*` series, then `jj fork assemble --push`.
4. Install the bundled skills into the same series: `jj fork skill --install` (writes `.agents/skills/<name>/SKILL.md`), commit, and rerun after upgrading jj-fork (`check` warns when they differ).
5. Write the repo's `AGENTS.md`: that `fork/main` is generated and never committed onto; to load the `maintaining-forks-with-jj-fork` skill before version-control work; that a new feature is its own `patch/<name>` rooted on upstream and must build and test on upstream alone; and that fork-only files belong in the `tooling/*` series.

## Amp project settings

Changing these affects every user of the project; get explicit approval first.

- Base branch (what orbs clone and Ship targets): verify it is `fork/main` (`amp projects get <project>`), else `amp projects update <project> --base-branch fork/main`. Use `fork/main` because it holds the fork's code and its config. `main` is only the upstream mirror and a series lacks the other patches. The branch must already contain `.jj-fork.toml` and the skills, so change it only after step 3 has landed on `fork/main`.
- Ship behavior: `amp projects update <project> --ship-behavior custom --custom-ship-prompt-file .agents/ship.md`. Keep `.agents/ship.md` in the repo and push the same text to the project. The prompt should: pick `patch/*` or `tooling/*`; start with `jj fork create` or continue an existing series with `jj new <series>`; verify that series alone with the repo's own build and tests; then `jj fork assemble --save-plan` and `jj fork apply --push`; and stop and ask if the series is unclear.
- Orb size: raise it if checks need memory (`--orb-size`).

## Scheduled fork owner

Create one thread schedule (building-schedules skill) that runs `jj fork sync --save-plan P --report R`. Exit 0: `jj fork apply P --push`. Exit 20: one fixer per issue in `R` runs `jj fork repair start` and `submit`, then `apply`. Fixer orbs are separate machines, so they must return their results to the owner (a branch or an artifact) or run inside the owner's orb. Do not give fixers push access to `fork/main`. Apply a saved plan right after saving it and in the same checkout: other jj operations make it stale. An end-to-end fixer-orb handoff is not provided by jj-fork; test yours before relying on it.

## Verify

In a fresh orb: `jj-fork --version`, `jj fork check --no-checks` (every series reported), `jj fork skill` (lists bundled skills), and a dry run `jj fork assemble --save-plan /tmp/p.json` (exit 0 or 20, nothing published).
