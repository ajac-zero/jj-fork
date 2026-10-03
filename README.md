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
jj fork create patch/NAME -m MESSAGE  start an empty, editable series on upstream
jj fork retire patch/NAME [--remove-glue glue/a+b]... [--save-plan FILE] [--push]
jj fork check [--report FILE]   report each series: up-to-date, clean, conflict, or broken
jj fork sync [--save-plan FILE] plan clean series updates and fork assembly
jj fork assemble [--save-plan FILE] plan glue restacks and fork assembly
jj fork apply PLAN [--push]     revalidate and publish a saved plan
jj fork repair start PLAN --issue ISSUE --dir DIR [--allow-path PATH]...
jj fork repair submit TASK_DIR... --save-plan NEXT_PLAN
jj fork alias                   add `aliases.fork` to your jj config
```

`check` constructs each stale series as an unpublished jj candidate, then materializes that exact commit in a detached Git worktree and runs the configured patch checks there. Each problem gets a difficulty tier (`[tier=low|medium|high]`) from the size of the conflict, or from the check that failed, so an automated fixer can pick a matching model.

`sync` and `assemble` use the same candidate-first transaction model: plan the jj operations and checks against unpublished candidates in detached worktrees; publish local bookmarks only after the required checks pass and the source state is still current. Checks do not change the source checkout. A stale local operation or source edit discovered while checks run makes the command refuse publication and preserve that work. A failed check discards planned maintenance rather than partially rebasing a series or moving the fork branch. Preparation (including snapshot, fetch, and reconciliation) remains separate and may already have changed local state; discarded transactions may also leave unreachable objects. This is not byte-for-byte rollback or filesystem/remote atomicity.

## Series lifecycle

`create` accepts a full bookmark name under a configured series prefix, such as `patch/my-fix` or `tooling/agent-workflow`. It creates an empty commit directly on upstream, sets that bookmark, and selects it as the working copy. It refuses unfinished working-copy changes, invalid names, `+` in the series name, and names already used locally or on a known remote (including the same suffix under another configured prefix). Edit the series with jj, then run `assemble`; creation never assembles or pushes.

`retire` deliberately stops including a series in the fork. Confirm upstream has absorbed it, or that you intend to drop it; the command does not infer semantic equivalence with upstream. It lists dependent glues and refuses unless each is explicitly approved with `--remove-glue NAME`. Review those resolutions first: a glue over three series can contain fixes still needed by the two remaining series. The reduced fork must be conflict-free and pass configured fork checks, even when its parent set is unchanged. Any refusal discards all planned removals, including partial glue repair state.

Retirement supports `--report`, `--save-plan`, `--no-fetch`, and `--push`, but never skips checks. A successful saved retirement plan can be applied normally. A failed retirement plan cannot use `repair start`; fix the remaining series/glues in the source and prepare retirement again.

Commit history and separate PR-head bookmarks are preserved. Even with `--push`, retirement never deletes remote selectors or glues: it pushes the reduced fork and remaining members only. Removal remains a local pending deletion, which subsequent preparation preserves. Other clones can still select those remote refs; coordinate explicit remote deletion separately if retirement should be permanent across clones.

## Saved plans and reports

Use `check --report FILE` to write a report, or `sync`/`assemble`/`retire --save-plan FILE [--report FILE]` to save validated candidate work for later application. Saving a plan does not publish maintenance or push; normal preparation may still snapshot, fetch, and reconcile the repository. `--save-plan` cannot be combined with `--push` or `--no-checks`.

Apply a saved plan with `jj fork apply PLAN [--push] [--report FILE]`. Apply authenticates the repository-local plan and its frozen preconditions, reruns the configured checks on the exact candidates, then compare-and-swap publishes only a successful candidate. Apply has no target, `--no-fetch`, or `--no-checks` override: the checked inputs cannot be silently changed at application time. Reports are informational, not executable instructions; plan and report formats are version-strict, and the tool never runs commands embedded in artifacts.

Write plans and reports outside the working tree, or in a path your Git excludes: an unignored artifact inside it is a new source file, so `apply` refuses the plan as stale. Plans are repository-local authenticated artifacts. Their HMAC authority is kept outside tracked files. Editing an artifact invalidates its authentication; plans are nonportable and become unusable when relevant inputs change or jj garbage-collects required objects. Missing objects are never silently reconstructed. Keep the repository's local authority and plan files private as appropriate; do not commit or share them as reusable portable plans.

## Isolated repair tasks

When a plan identifies work for a person or agent, start a separate task repository with `jj fork repair start PLAN --issue ISSUE --dir DIR [--allow-path PATH]...`. It uses an independent object database and jj repository, not another workspace or worktree in the source repository. It receives no source remotes, credential configuration, or plan-authentication key, and exposes only the designated repair/result bookmark. The default path scope is the set of paths captured by the plan; a check failure requires an explicit allowed-path scope.

Submit one or more task directories with `jj fork repair submit TASK_DIR... --save-plan NEXT_PLAN [--report FILE]`. Submission verifies each task's manifest, authentication, parent, allowed paths, topology, identity, and source preconditions; imports only needed candidate objects without publishing them; rebuilds downstream candidates; reruns checks; and saves a successor plan. It does not publish to the source repository or push. Independent series tasks can be batched; overlapping tasks or stale dependencies are refused. The separate repository prevents accidental source ref or operation changes, but is not a security sandbox against malicious task code that can access the source filesystem.

The CLI is the product interface; these workflows do not add a dashboard, service, or scheduler. Saved plans and reports are not a public JSON API or SDK.

Series and glues are copied onto new parents with new jj change IDs, preserving the original commits and their unrelated descendants. `assemble` checks a conflict-free fork candidate before moving the fork branch. If conflicting glue or multiway repair state is deliberately needed, that state may be published without moving the fork branch or pushing. Export or checkout errors can occur after local publication and are reported as partial failure.

With `--push`, jj-fork fetches again and checks a final remote snapshot, including the mirror. It refuses to push when the guarded refs changed during the run; the successful local operation remains published. Push uses per-ref leases, but neither those leases nor the snapshot make multi-ref publication or remote membership atomic. No silent drops: before moving the fork branch, and again before pushing, `assemble` checks every series and glue bookmark on the fork remote. Each must be merged into the new fork branch or deliberately deleted locally (in jj, a pending deletion); otherwise it refuses, names the bookmark, and exits `20`. A remote bookmark this clone never tracked counts as not deleted. Nothing on the remote is deleted.

Ordinary `check`, `sync`, and `assemble` preparation reconciles local bookmarks in the fork's namespaces (fork branch, mirror, series, glue) against the fork remote, with or without `--no-fetch`, so a stale clone (an old snapshot, or a plain `git fetch` that jj never saw) cannot resurrect or overwrite remote state. `apply` instead verifies saved preconditions without ordinary preparation. Each reconciliation change is a `reconciled:` line naming its rule: (1) local behind the remote moves to it; (2) a conflicted bookmark is set to the remote's commit; (3) local ahead of or diverged from the remote is kept as unpushed work and reported, except (3b) a diverged commit that is already on a remote ref (the remote restacked or rebased it) takes the remote's commit; (4) a bookmark missing on the remote whose commit is reachable from a remote ref was deleted after publishing, so it is forgotten locally; (5) any other bookmark missing on the remote is new work and kept. Nothing on the remote is deleted, and `--push` skips any bookmark that is behind its remote.

Exit codes: `0` nothing to do or success, `10` (`check`) every stale series is clean, `20` a series or merge needs a person or an agent, `1` error (including a reported partial failure after local publication). Reports go to stdout, progress to stderr.

## jj version

The engine uses `jj-lib` pinned to `=0.43.0`, with pinned jj library/CLI helper crates. The experimental API is version-coupled to that release; other jj-lib versions are not supported. Require jj CLI 0.43.0 where CLI support is needed. Initialized maintenance uses crate APIs for snapshots/import, settings and revsets, reconciliation, tracking, fetch, and push. Bootstrap tasks such as initializing missing `.jj` state, alias/config writes, and Git unshallowing may still use subprocesses, as do detached Git worktrees and configured shell checks. The jj executable is for bootstrap and manual task workflows, not the maintenance engine. Preserve the user's signing setup and private-repository policy. Working-copy updates and remote pushes are not atomic; report unknown or partial network outcomes honestly.

Native maintenance supports colocated Git repositories with jj's default stores and local working copy; Watchman support is not enabled. See [the implementation plan](IMPLEMENTATION_PLAN.md) for integration boundaries and verification evidence.

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
cargo test                # unit tests
cargo build               # shell suites run target/debug/jj-fork; cargo test does not rebuild it
tests/e2e.sh              # existing end-to-end scenarios against throwaway local repositories
bash tests/transaction.sh # transactional candidate and publication scenarios
bash tests/native.sh      # native settings, unshallow preservation, and transport
bash tests/workflow.sh    # saved-plan, authentication, and apply workflows
bash tests/repair.sh      # isolated repair-task workflows
```

The suites need Rust 1.89 or newer; repository preparation, Git transport, worktrees, and manual task workflows need `jj` and `git` on `PATH`. `scripts/install-jj [DIR]` installs the jj release that CI uses.

## License

MIT
