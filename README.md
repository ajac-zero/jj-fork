# jj-fork

Maintain a fork as independent [jj](https://github.com/jj-vcs/jj) series on top of upstream, and keep them current as upstream moves.

## The model

```text
upstream main ──┬── patch/a ──┬──────────────┐
                ├── patch/b ──┼── glue/a+b ──┼── fork/main
                └── tooling/x ───────────────┘
```

- **Series** (`patch/*`, `tooling/*`, configurable) are bookmarks whose commits sit directly on upstream. Each one is independent and could be proposed upstream on its own.
- **Glues** (`glue/<a>+<b>`) are merges of two or more series that hold only the resolution of their conflicts. When a series moves, its glues are restacked onto the new tip, carrying the resolution. A glue over a superset (`glue/a+b+c`) merges the glue over the subset (`glue/a+b`).
- **The fork branch** (`fork/main`) is a generated merge of upstream, every series, and every glue. It never holds hand-made changes, so rebuilding it is always safe.

## Commands

```text
jj fork init [--upstream URL]   prepare a clone: remotes, full history, jj, tracking, revset aliases
jj fork check                   report each series: up-to-date, clean, conflict, or broken
jj fork sync [--push]           rebase clean series onto upstream and assemble the fork branch
jj fork assemble [--push]       restack glues, then build, check, and move the fork branch
jj fork alias                   add `aliases.fork` to your jj config
```

`check` constructs each stale series as an unpublished jj candidate, then materializes that exact commit in a detached Git worktree and runs the configured patch checks there. Each problem gets a difficulty tier (`[tier=low|medium|high]`) from the size of the conflict, or from the check that failed, so an automated fixer can pick a matching model.

`sync` and `assemble` use the same candidate-first transaction model: plan the jj operations and checks against unpublished candidates in detached worktrees; publish local bookmarks only after the required checks pass and the source state is still current. Checks do not change the source checkout. A stale local operation or source edit discovered while checks run makes the command refuse publication and preserve that work. A failed check discards planned maintenance rather than partially rebasing a series or moving the fork branch. Preparation (including snapshot, fetch, and reconciliation) remains separate and may already have changed local state; discarded transactions may also leave unreachable objects. This is not byte-for-byte rollback or filesystem/remote atomicity.

Series and glues are copied onto new parents with new jj change IDs, preserving the original commits and their unrelated descendants. `assemble` checks a conflict-free fork candidate before moving the fork branch. If conflicting glue or multiway repair state is deliberately needed, that state may be published without moving the fork branch or pushing. Export or checkout errors can occur after local publication and are reported as partial failure.

With `--push`, jj-fork fetches again and checks a final remote snapshot, including the mirror. It refuses to push when the guarded refs changed during the run; the successful local operation remains published. Push uses per-ref leases, but neither those leases nor the snapshot make multi-ref publication or remote membership atomic. No silent drops: before moving the fork branch, and again before pushing, `assemble` checks every series and glue bookmark on the fork remote. Each must be merged into the new fork branch or deliberately deleted locally (in jj, a pending deletion); otherwise it refuses, names the bookmark, and exits `20`. A remote bookmark this clone never tracked counts as not deleted. Nothing on the remote is deleted.

Before anything else, every command reconciles local bookmarks in the fork's namespaces (fork branch, mirror, series, glue) against the fork remote, with or without `--no-fetch`, so a stale clone (an old snapshot, or a plain `git fetch` that jj never saw) cannot resurrect or overwrite remote state. Each change is a `reconciled:` line naming its rule: (1) local behind the remote moves to it; (2) a conflicted bookmark is set to the remote's commit; (3) local ahead of or diverged from the remote is kept as unpushed work and reported, except (3b) a diverged commit that is already on a remote ref (the remote restacked or rebased it) takes the remote's commit; (4) a bookmark missing on the remote whose commit is reachable from a remote ref was deleted after publishing, so it is forgotten locally; (5) any other bookmark missing on the remote is new work and kept. Nothing on the remote is deleted, and `--push` skips any bookmark that is behind its remote.

Exit codes: `0` nothing to do or success, `10` (`check`) every stale series is clean, `20` a series or merge needs a person or an agent, `1` error (including a reported partial failure after local publication). Reports go to stdout, progress to stderr.

## jj version

The engine uses `jj-lib` pinned to `=0.43.0`. Use jj CLI 0.43.0 for preparation and Git transport. The experimental jj-lib API is version-coupled to that release; other jj-lib versions are not supported.

Native maintenance supports colocated Git repositories with jj's default stores and local working copy; Watchman support is not enabled. See [the implementation plan](IMPLEMENTATION_PLAN.md) for the integration boundaries, verification evidence, and following milestones.

## Install

```sh
cargo install --git https://github.com/ajac-zero/jj-fork
jj-fork alias                 # makes `jj fork` run jj-fork
```

jj does not discover `jj-<name>` binaries on its own, so the alias runs `jj util exec -- jj-fork`.

## Configuration

Repository facts live in a committed `.jj-fork.toml`, so every clone, CI job, and agent sandbox sees them. `jj fork init --upstream URL` writes a starter file. Personal overrides live in jj config under `jj-fork.*` with the same structure, for example `jj config set --user jj-fork.fork.remote mine`.

```toml
[upstream]
url = "https://github.com/owner/project.git"
# remote = "upstream"
# branch = "main"

[fork]
# remote = "origin"
# branch = "fork/main"
# series_prefixes = ["patch/"]
# glue_prefix = "glue/"
# mirror_branch = "main"          # fast-forward a fork branch that mirrors upstream

[checks]
# Run on each stale series replayed onto upstream.
patch = [
  { name = "build", run = "go build ./...", tier = "medium", low_if_errors_at_most = 2 },
  { name = "test", run = "go test -count=1 {go_packages}", when = "go_packages", kind = "go-test", tier = "high" },
]
# Run on the fork-branch candidate before it moves.
fork = [
  { name = "test", run = "go test ./...", kind = "go-test" },
]

[generated]                        # optional
paths = ["api/*/zz_generated.deepcopy.go"]
inputs = ["api/**"]                # patch checks regenerate only when these change
regenerate = "make generate"

[tiers]                            # optional; upper bounds at the first conflicting commit
low_max = { files = 2, hunks = 3, lines = 60, commits = 3 }
medium_max = { files = 12, hunks = 20, lines = 400, commits = 10 }

[low_memory]                       # optional; applied below 8 GiB of RAM
env = { GOFLAGS = "-p={jobs}", GOMEMLIMIT = "{memory_limit_mib}MiB" }
```

Check placeholders: `{go_packages}` expands to the Go package directories a series changes, and `{jobs}` to the parallelism for this machine. A check with `kind = "go-test"` retries each failing test up to 3 times; if any retry passes, the test is noted as flaky and does not block. Otherwise it runs `go test -count=5` on bare upstream; if any of those runs fails, the test is reported as an upstream failure and ignored. Only a test that fails every retry and passes all 5 upstream runs fails the check.

See [examples/ai-gateway.toml](examples/ai-gateway.toml) for a complete configuration.

## Tests

```sh
cargo test           # unit tests
cargo build          # tests/e2e.sh runs target/debug/jj-fork; cargo test does not rebuild it
tests/e2e.sh         # end-to-end scenarios against throwaway local repositories
tests/transaction.sh # transactional candidate and publication scenarios
```

The end-to-end tests need `jj` and `git` on `PATH`. `scripts/install-jj [DIR]` installs the jj release that CI uses.

## License

MIT
