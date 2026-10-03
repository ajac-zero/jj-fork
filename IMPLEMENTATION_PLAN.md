# Native jj maintenance engine

This plan is the first implementation milestone of a reproducible, repairable
fork-maintenance engine. Preserve the current CLI, series/glue model, configured
checks, exit codes, and no-silent-drop safeguards. Do not add a UI, service,
agent scheduler, or persistent/public JSON plan format in this milestone.

## 1. Pin the integration and freeze inputs

- Link `jj-lib = "=0.43.0"` with the Git backend, matching the CLI release used
  by the installer and CI. Set the supported Rust minimum to the dependency's
  minimum and commit the lockfile. Reject incompatible CLI versions early.
- Retain CLI initialization, effective configuration loading, initial workspace
  snapshot/import, fetch, reconciliation, tracking, and network push boundaries.
  Restrict native maintenance to the supported colocated Git/default-store setup.
- After preparation, freeze the operation/head set, workspace snapshot,
  resolved target/candidate IDs, local membership/bookmark targets, remote refs
  (including the mirror), and tracking/pending-deletion state.
- Never repeatedly resolve mutable bookmarks while checks are running. Use an
  internal typed candidate/outcome model; defer a serialized plan schema.

## 2. Construct the exact candidates

- Replace Git cherry-pick validation and CLI duplicate output parsing with
  native `duplicate_commits`, `merge_commit_trees`, and repository graph/ref
  queries. Preserve duplication semantics: originals and their descendants are
  not rewritten, and copied commits receive new change IDs.
- Check intermediate copied series commits for conflicts. Preserve the existing
  refusal of stale series containing merges and skip patch checks for series
  already based on the target.
- Restack glues in increasing member-set size, retaining resolutions and inner
  glue dependencies. Reduce fork parents to graph heads. Keep nested-series and
  exact candidate-parent validation.
- Use jj's structured conflicts and merge semantics for diagnostics. Exclude
  generated paths from tier metrics and exclude base-side text from line counts.
  Treat non-text conflicts explicitly rather than parsing Git conflict markers.

## 3. Validate without source-workspace mutation

- Materialize conflict-free candidates by their exact full commit IDs in
  detached Git worktrees. The Git backend writes objects before jj view
  publication, so the checked candidate can be the actual published candidate.
- Continue using the existing check runner, Go retry/upstream-baseline policy,
  package selection, generated-file checks, and resource controls.
- Reject successful checks that moved HEAD or modified tracked/index contents.
  Ordinary untracked/ignored build output remains allowed.
- Never hold source-workspace or operation-head locks through long checks.

## 4. Select success, repair, or discard

| Result | Intended local outcome |
| --- | --- |
| Series conflict or patch-check failure | Discard all planned maintenance |
| Invalid model/candidate, fork-check failure, no-silent-drop refusal | Discard all planned maintenance, for sync and assemble |
| Restacked glue conflicts | Publish validated series/glue repair state, not the fork |
| Generated merge conflict explained by named pairs | Keep approved series/glue work, discard anonymous conflicted merge |
| Unexplained multiway merge conflict | Retain an identified repair head usable with `--candidate` |
| Supplied candidate fails | Leave the supplied commit intact |
| All checks pass | Publish approved bookmark moves and the exact fork candidate |

Repair outcomes never push. Replace maintenance `op restore` rollback with
discarding unpublished state. Preparation can still publish snapshots/fetches/
reconciliation, and candidate objects may remain in storage after discard.

## 5. Guard publication and handle side effects honestly

- jj transaction publication is not automatically compare-and-swap. Write an
  unpublished operation, acquire the default operation-head-store lock, compare
  the frozen head set, and update heads under the same lock. Stale inputs return
  exit 20 without publishing the candidate.
- Re-snapshot under the working-copy mutation lock before publication/checkout;
  refuse if the source changed while checking. Respect jj's ignore/tracking and
  snapshot policies. Preserve old workspace history.
- Successful assembly leaves an empty working-copy child above the fork.
  Glue repair leaves the checkout alone; an identified multiway repair may be
  checked out deliberately.
- Working-copy checkout and colocated Git HEAD/index/ref export are distinct
  side effects. Report a published local operation with incomplete checkout/
  export as partial completion, return 1, and never restore away the operation
  or push to hide the failure.
- Do not invoke CLI subprocesses while holding native workspace/store locks.

## 6. Push separately

- Fetch again and compare the relevant remote namespace against the initial
  snapshot, including mirror movement, additions, and deletions.
- Verify local intended targets still equal the validated targets, repeat the
  no-silent-drop guard, retain behind-bookmark skips and mirror fast-forward
  checks, and push explicit bookmarks anchored to a frozen operation.
- Preserve per-ref leases. Do not claim namespace-wide or multi-ref atomicity:
  a new unrelated remote bookmark after the final fetch is outside the guard,
  and remote push can partially succeed.

## Implementation ownership

- High agent: dependency pinning and native repository/maintenance engine;
  Cargo files and source files other than `checks.rs`.
- Medium agent: check integrity and black-box transactional regressions;
  `src/checks.rs` and tests.
- Low agent: README behavior/compatibility documentation.
- Supervisor: this plan, integration, implementation review, CI wiring, combined
  verification, and any necessary fixes after integration.

## Acceptance evidence

Run formatting, strict clippy, unit tests, the existing end-to-end suite, and
new transactional regressions. Cover exact checked/published candidates,
preserved original histories, intermediate conflicts, all-or-nothing check
failures, reachable glue repair, glue dependency ordering, supplied candidates,
untracked remote membership versus deliberate deletion, concurrent local
operations/source edits, mirror/remote changes, and check mutation detection.
Exercise partial checkout/export failures where practical and state limitations
where a deterministic fault injection is unavailable.

Completion requires no maintenance use of Git cherry-pick, CLI duplicate prose
parsing, or operation restore. CLI preparation/transport remains intentional.

### First-milestone verification

Implemented with the high/medium/low assignments above and supervisor integration
fixes, then committed and pushed as
[`f4da65b`](https://github.com/ajac-zero/jj-fork/commit/f4da65bd88ccd46ee444fb39ea565ac95acbe83b).
No new release or deployment was triggered.

- `cargo fmt --check`: clean.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `cargo test --locked`: 26 tests passed.
- `cargo build --locked`: succeeded.
- `tests/e2e.sh`: all 18 existing scenarios passed.
- `bash tests/transaction.sh`: 23 scenarios passed, including exact checked
  commit publication, no-op candidate stale guards, glue repair/dependency
  ordering, supplied repair candidates, and post-publication HEAD/export/
  skipped-checkout failures that preserve local work and never push.
- `cargo +1.89.0 check --locked`: succeeded. The lockfile keeps `kstring` at
  2.0.2, as in jj 0.43.0, rather than a newer release requiring Rust 1.96.

Configured patch checks still run on the series tip; intermediate commits are
checked for conflicts. Source edits that respect jj's snapshot policies and
local operation changes invalidate publication, even when an existing candidate
requires no repository edits. Ordinary filesystem writers do not honor jj
locks, so filesystem/remote atomicity remains explicitly outside the guarantee.
The native backend is limited to colocated Git/default stores/local working
copies; Watchman support is not enabled. Transport is still CLI-backed and
multi-ref pushes are not atomic. JSON plans and the following milestones remain
unimplemented.

## Remaining implementation contracts

The user authorized implementing and delivering the remaining plan. The CLI is
the product surface: machine-readable artifacts and isolated tasks support CI
and agents without adding a dashboard, TUI, service, database, or scheduler.
The following contracts are the oracle's continuation plan.

### Native preparation and transport

- Load native jj configuration with pinned jj-cli configuration helpers rather
  than parsing CLI templates. Preserve defaults, conditional scopes, layered
  overrides, aliases, `JJ_CONFIG`, and identity/operation environment settings.
  Configuration errors must not silently disable overrides or checks.
- Snapshot/import, fetch orchestration, reconciliation, tracking, and revision
  resolution are crate-backed. Freeze after preparation and resolve targets
  against that exact view. Preserve all reconciliation rules and distinguish
  conflicted remote targets, absence, tracking, forgetting, and deletion.
- Unshallow without deleting `.jj`: rebuild the default index and preserve
  operations, jj-only work, configuration, and workspace metadata.
- Bootstrap discovery, missing-workspace initialization, explicit init/alias
  configuration writes, Git unshallow/worktree commands, and configured checks
  may remain subprocesses. Initialized maintenance must work with a jj wrapper
  that rejects every CLI invocation.
- Use jj-lib fetch/push APIs and explicit before/after per-ref targets. Preserve
  credential helper/SSH/askpass and configured Git-executable behavior. Never
  mutate process-global environment or copy source credentials into tasks.
- Keep push eligibility, private-commit, identity, and signing policies. Never
  rewrite an already-checked candidate as an on-push signing side effect.
  Unsupported policy must fail explicitly rather than be ignored.
- Report accepted, rejected, unknown, skipped, and local-bookkeeping-incomplete
  updates honestly. A transport error does not prove nothing reached the remote.

### Structured reports and executable plans

```text
check [existing options] --report FILE
sync [existing options] --save-plan FILE [--report FILE]
assemble [existing options] --save-plan FILE [--report FILE]
apply PLAN [--push] [--report FILE]
```

- Preserve human output and exit codes; reports are informational documents.
  Saving a plan checks/builds but never publishes maintenance or pushes.
  Reject saving with `--push` or `--no-checks`. Save failure/conflict plans too.
- Use strict versioned typed envelopes for plans/reports/tasks. An executable
  plan includes source repository/workspace identity, frozen operation/working
  copy/Git/configuration/remote state, exact candidate graph and replay mappings,
  permitted concrete changes, outcome, stable issue IDs, and check records.
- Authenticate executable payloads with a repository-local random HMAC key
  outside tracked files. Domain-separate artifact kinds/versions and verify
  before interpreting supplied IDs or paths. Reject tampering, unknown fields,
  malformed IDs, unsupported versions, reports, unsigned inputs, and foreign
  repository/workspace artifacts. Never offer arbitrary JSON signing/import.
- Use restrictive race-safe key creation and atomic artifact writes. Store exact
  candidates through durable unpublished jj operations; do not add source heads
  or bookmarks to pin them. Plans expire if inputs change or objects are pruned;
  missing objects never trigger silent candidate reconstruction.
- `apply` does not run ordinary preparation or accept target/no-checks/no-fetch
  overrides. It verifies the permitted view/graph delta, probes remotes, reloads
  trusted configuration, reruns required checks on exact candidates, revalidates
  all preconditions, and only then guarded-publishes and optionally pushes.
- Check records are historical evidence, not authorization to skip execution.
  Never execute commands supplied by an artifact. Failed/not-ready plans require
  repair rather than arbitrary partial publication.

### Isolated repair tasks and successor plans

```text
repair start PLAN --issue ISSUE --dir DIR [--allow-path PATH]...
repair submit TASK_DIR... --save-plan NEXT_PLAN [--report FILE]
```

- A task is an independent Git object database and jj repository, not a shared
  Git worktree or jj workspace. Seed exact objects and a `repair/result` bookmark;
  copy no source remotes, hooks, credentials, or authority key.
- Authenticate the manifest's parent plan, issue, seed graph, destination,
  dependencies, and permitted edit scope. Conflict tasks default to conflict
  paths; check failures require explicit scope when diagnostics cannot supply it.
- Series tasks preserve captured linear-chain order/count/change identities and
  target ancestry. Glue/fork tasks require exact captured parents and one
  authorized resolution commit. New-glue authority names one concrete glue.
- Validate all path/mode changes, including deletion, rename endpoints, symlinks,
  executability, and submodules. Reject traversal, metadata/config changes, extra
  parents, dropped/squashed commits, unauthorized membership, and unrelated work.
- Submission verifies tasks and current source preconditions, freezes results,
  imports only candidate objects into unpublished source state, rebuilds
  downstream glues/fork, and reruns required checks. Repaired series receive
  patch checks even when already based on the target.
- Submission saves an authenticated successor plan; it never publishes source
  bookmarks/checkouts or pushes. `apply` is the sole publication boundary.
  Independent tasks can batch; overlapping destinations and stale dependencies
  are refused rather than silently reparented.
- Independent repositories prevent accidental shared ref/operation mutations,
  not malicious access to the source filesystem. Untrusted code additionally
  needs process/filesystem isolation; HMAC is local integrity, not identity.

### Remaining ownership and acceptance

- High native agent owns backend/config/init/reconciliation and dependencies.
- Medium workflow agent owns engine/CLI, artifacts, reports, and check records.
- A subsequent high repair assignment owns isolated tasks/submissions once the
  prepared-plan contracts are integrated. Low agent owns README documentation.
- Supervisor owns cross-module integration, adversarial regressions, review,
  combined verification, commits, and pushes. No agent ships independently.

Acceptance includes initialized maintenance without the jj CLI, layered native
settings, preservation through unshallow, native leases/partial transport results,
cross-process save/apply, invisible saved candidates, tamper/foreign/expired-plan
refusal, config/source/operation/remote invalidation, mandatory check reruns,
isolated valid repairs, rejected unauthorized edits/topology, batch independence,
repair checks on based-on-target tips, and explicit final application.
