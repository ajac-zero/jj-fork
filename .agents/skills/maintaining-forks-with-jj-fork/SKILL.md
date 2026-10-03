---
name: maintaining-forks-with-jj-fork
description: Maintains a long-lived fork as jj series (patch/*, tooling/*) plus glue/* merges on top of upstream with the jj-fork CLI. Use when asked to sync a fork with upstream, check or rebase patches, resolve a conflicted or broken series or glue, assemble or push fork/main, or run the scheduled fork-owner routine.
---

# Maintaining a fork with jj-fork

`jj-fork` (also `jj fork` after `jj-fork alias`) keeps a fork current with upstream. Use it instead of hand-rolled rebases. Needs jj 0.43.0 and a colocated clone with a committed `.jj-fork.toml`.

## Model

- **Series**: `patch/<name>` (upstreamable) and `tooling/<name>` (fork-only), each rooted directly on upstream, independent of each other. Names are unique and must not contain `+`.
- **Glue**: `glue/<a>+<b>` descends from exactly those series' tips and holds only the resolution of their conflicts. Three-way: `glue/a+b+c` (parents: `glue/a+b` and `patch/c`).
- **`fork/main`**: a generated merge of upstream, every series, every glue. Never edit it; rebuilding is always safe.
- Conflicts between two series that each pass alone belong in a glue, never in a series or in `fork/main`.

## Routine (single owner)

```bash
jj fork init                 # once per clone; idempotent, unshallows, keeps jj history
jj fork check                # read-only report per series
jj fork sync --push          # clean series -> rebase, assemble, check, push
```

Exit codes: `0` nothing to do / success, `10` (check) every stale series is clean, `20` a person or agent is needed (series conflict/broken, glue conflict, conflicting pair, dropped remote bookmark, stale source), `1` error or partial failure after local publication.

`check` lines are `<series> up-to-date|clean|conflict|broken [tier=low|medium|high]`. The tier picks the agent mode for the fixer (retry once a tier up).

`sync`/`assemble` change nothing unless every check passes. `--push` refuses if the fork remote changed during the run; rerun it. Never push by naming bookmarks by hand.

## Routine with a plan (preferred for agents and CI)

Plans are authenticated, repository-local, and never publish:

```bash
jj fork sync --save-plan /tmp/plan.json --report /tmp/report.json   # exit 20 = not ready
jj fork apply /tmp/plan.json --push                                  # reruns checks, then publishes
```

- Write plans/reports **outside the working tree** (or in a Git-ignored path). An unignored file inside it is a new source edit and `apply` refuses it as stale.
- `apply` takes no `--target/--no-fetch/--no-checks`; edited, foreign, expired, or stale plans are refused. If it refuses as stale, save a new plan.
- Do not run `jj util gc` between save and apply; the candidate objects can expire.
- `--save-plan` cannot be combined with `--push` or `--no-checks`.

## Repairing a conflicted or broken series (one isolated task per issue)

Issue ids are in the plan: `.payload.issues[].id`, e.g. `series-conflict:patch/a`, `series-check:patch/b`, `glue-conflict:glue/a+b`, `glue-needed:glue/b+c`, `fork-conflict:…`, `fork-check:fork/main`.

```bash
jj fork repair start /tmp/plan.json --issue series-conflict:patch/a --dir /tmp/task-a
jj fork repair start /tmp/plan.json --issue series-check:patch/b --dir /tmp/task-b --allow-path path/in/b
```

The task is its own Git+jj repository (no remotes, hooks, credentials, or plan key). Work only inside it:

1. `jj -R DIR edit 'repair/result-'` (the first conflicted commit; check the start output), fix files, `jj -R DIR status` to snapshot. jj carries the resolution into descendants.
2. Keep the series' commit count, order, change ids, and parents; do not squash, reorder, add parents, or rewrite descriptions.
3. Change only the allowed paths (conflicted paths by default; check failures need `--allow-path`). Never touch `.jj-fork.toml`, `.git*`, or other metadata.
4. Regenerate generated files rather than merging them by hand, if the repo's config names a `regenerate` command.

Then, from the source repo:

```bash
jj fork repair submit /tmp/task-a /tmp/task-b --save-plan /tmp/next.json
jj fork apply /tmp/next.json --push
```

Independent series batch in one submit; two tasks for the same series, or a stale plan, are refused. Submit rebuilds downstream glues/fork, reruns every required check (repaired series included), and saves a successor plan. It does not publish or push.

Glue conflict after a restack: repair the `glue-conflict` issue (one authorized resolution commit with the exact required parents). A conflicting pair: repair the `glue-needed:glue/<a>+<b>` issue. An unexplained multiway conflict: resolve inside the reported repair head and use `jj fork assemble --candidate <rev> --push`.

## Scheduled fork owner (low-mode router)

1. `jj fork sync --save-plan P --report R`. Exit `0`: `jj fork apply P --push`, done.
2. Exit `20`: for each issue in `R`, spawn one fixer thread (mode by tier) running the repair procedure above, all against the **same plan**; fixers return their task dirs.
3. `jj fork repair submit <dirs> --save-plan N`, then `jj fork apply N --push`. Resolve any remaining glue issue the same way and repeat.
4. Fixers never publish, push, or touch `fork/main`, `main`, glues, or other series.

## Changing the fork's series by hand

jj-fork does not create, retire, or rename series. Do it with jj, then assemble:

- New patch: `jj new 'main@upstream'`, edit, `jj bookmark create patch/<name> -r @`.
- Fix a patch: `jj new patch/<name>`, edit, `jj bookmark set patch/<name> -r @`.
- Retire an upstreamed patch: confirm it is in upstream, `jj bookmark delete patch/<name>` **and every `glue/*` naming it** (a leftover glue brings the patch back and assemble refuses a glue naming a missing series). Remote bookmarks are never deleted by jj-fork; delete them deliberately.
- Then `jj fork assemble` (local), or `jj fork assemble --push`.

The no-silent-drop guard refuses to move `fork/main` while a series/glue bookmark on the fork remote is neither merged nor deliberately deleted locally; a bookmark you never tracked counts as not deleted.

## Gotchas

- Stale clones are reconciled automatically on `check/sync/assemble` (see `reconciled:` lines); local unpushed work is kept. `apply` does not reconcile; it refuses on drift.
- Do not `jj rebase -s` a series: it drags descendants. jj-fork copies series onto the new target and leaves originals.
- Small-RAM machines: configure `[low_memory]` in `.jj-fork.toml`; checks use `{jobs}`/`{memory_limit_mib}`.
- Ignored upstream failures print `note: also fails on upstream, ignored`; do not "fix" them in a patch.
- Native commands do not need the jj CLI after `init`; git and jj must be on PATH for init/aliases, worktrees, and checks.
- Conflict-only run: `--no-checks` (not `--no-tests`) skips configured checks; it cannot be combined with `--save-plan`/`apply`.
- Not provided by jj-fork (keep in the repo's own skill or scripts): PR-head bookmarks, creating or retiring series, a Ship-button prompt that turns a thread into a series, `jj patch-new`/`fork-assemble`-style aliases, and orb bootstrap beyond `jj fork init`. A stale-script check is unnecessary because jj-fork is an installed binary; pin its version in setup instead.
