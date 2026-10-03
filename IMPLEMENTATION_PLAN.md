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

Implemented locally with the high/medium/low assignments above and supervisor
integration fixes. No commit, push, release, or deployment has been performed.

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

## Following milestones

1. Native preparation: port reconciliation/tracking without changing their
   policy; replace snapshot/import/fetch/push CLI boundaries where worthwhile.
2. Versioned structured plans/reports: expose exact candidates, preconditions,
   diagnostics, and check records. Revalidate every precondition when applying
   a saved plan; a report is not authorization to apply stale state.
3. Repair interface: isolated tasks, explicit scope, and validated submissions
   for people or agents, with coordination around the same maintenance engine.
4. Product surfaces: a CLI, TUI, CI integration, or dashboard built on the proven
   engine rather than separate implementations of fork semantics.
