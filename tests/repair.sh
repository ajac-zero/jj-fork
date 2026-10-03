#!/usr/bin/env bash
# Isolated repair tasks and successor plans. Usage: tests/repair.sh [path/to/jj-fork] [scenario...]
# Contract: `repair start` and `repair submit` never publish source bookmarks, checkouts, or
# operations, and never push; only `apply` of the authenticated successor plan publishes.
# Set KEEP=1 to keep the fixtures for inspection.
set -euo pipefail
bin="$(realpath "${1:-target/debug/jj-fork}")"
shift || true
root="$(mktemp -d /tmp/jj-fork-repair.XXXXXX)"
[[ -n ${KEEP:-} ]] || trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
export JJ_CONFIG="$root/jjconfig.toml"
printf '[user]\nname = "t"\nemail = "t@example.com"\n' >"$JJ_CONFIG"

fail() { echo "FAIL: $*" >&2; exit 1; }
equal() { [[ "$1" == "$2" ]] || fail "$3: got '$1', want '$2'"; }
contains() { grep -q -- "$2" "$1" || { cat "$1" >&2; fail "$3: missing '$2'"; }; }
expect() {
  local want="$1"; shift
  set +e; "$@" >"$d/out" 2>"$d/err"; local got=$?; set -e
  if [[ $got != "$want" ]]; then cat "$d/out" "$d/err" >&2; fail "$* exited $got, want $want"; fi
  cat "$d/err" >>"$d/out"
}
rev() { jj --ignore-working-copy log --no-graph -r "bookmarks(exact:\"$1\")" -T commit_id; }
local_bookmarks() { jj --ignore-working-copy bookmark list -T 'name ++ " " ++ normal_target.commit_id() ++ "\n"'; }
# Source state that start and submit must leave alone: bookmarks (local and remote), Git
# branches, remote-tracking refs and tags, HEAD, the operation head, and the working copy.
state() {
  jj --ignore-working-copy bookmark list --all-remotes -T 'name ++ "@" ++ remote ++ " " ++ normal_target.commit_id() ++ "\n"'
  git for-each-ref --format='%(refname) %(objectname)' refs/heads refs/remotes refs/tags
  git rev-parse HEAD
  jj --ignore-working-copy op log --no-graph --limit 1 -T 'id ++ "\n"'
  git status --porcelain
}
remote_refs() { git -C "$d/fork.git" for-each-ref --format='%(refname) %(objectname)' refs/heads; }
invoke() { "$bin" --config "$d/config.toml" "$@"; }
# Runs jj inside a task directory, as a repair worker would.
tj() { jj -R "$1" --quiet "${@:2}"; }

# Upstream main has base.txt, conflict.txt, and an executable tool.sh. patch/a edits
# conflict.txt, then adds a.txt; patch/b adds b.txt (containing "broken", which fails the
# configured checks, when BROKEN_B is set); patch/c adds c.txt. Upstream then rewrites
# conflict.txt (or UPSTREAM_FILE), so patch/a's first copy conflicts on the new target. The
# fork check alone also fails on a merge containing c.txt while $d/fork-strict exists.
fixture() {
  d="$root/$1"; mkdir -p "$d"
  git init -q -b main "$d/upstream-src"
  cd "$d/upstream-src"
  printf 'base\n' >base.txt
  printf 'original\n' >conflict.txt
  printf '#!/bin/sh\n' >tool.sh; chmod +x tool.sh
  git add .; git commit -qm base
  git clone -q --bare . "$d/upstream.git"
  git clone -q "$d/upstream.git" "$d/fork-src"
  cd "$d/fork-src"
  git checkout -qb patch/a
  printf 'fork side\n' >conflict.txt; git add .; git commit -qm 'a edits conflict'
  printf 'a\n' >a.txt; git add .; git commit -qm 'a adds a'
  git checkout -qb patch/b origin/main
  printf '%sb\n' "${BROKEN_B:+broken }" >b.txt; git add .; git commit -qm 'b adds b'
  git checkout -qb patch/c origin/main
  printf 'c\n' >c.txt; git add .; git commit -qm 'c adds c'
  git checkout -qb fork/main origin/main
  git merge -q --no-edit patch/a patch/b patch/c
  publish_fixture
  printf 'upstream rewrite\n' >"$d/upstream-src/${UPSTREAM_FILE:-conflict.txt}"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'upstream rewrites conflict'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
}

# No series conflicts, but patch/b and patch/c add shared.txt differently, so the fork merge
# conflicts in exactly that pair and the plan asks for glue/b+c. The initial fork branch
# merges only patch/a and patch/b; patch/c is pushed separately.
pair_fixture() {
  d="$root/$1"; mkdir -p "$d"
  git init -q -b main "$d/upstream-src"
  cd "$d/upstream-src"
  printf 'base\n' >base.txt; git add .; git commit -qm base
  git clone -q --bare . "$d/upstream.git"
  git clone -q "$d/upstream.git" "$d/fork-src"
  cd "$d/fork-src"
  git checkout -qb patch/a; printf 'a\n' >a.txt; git add .; git commit -qm 'a adds a'
  git checkout -qb patch/b origin/main; printf 'from b\n' >shared.txt; git add .; git commit -qm 'b adds shared'
  git checkout -qb patch/c origin/main; printf 'from c\n' >shared.txt; git add .; git commit -qm 'c adds shared'
  git checkout -qb fork/main origin/main
  git merge -q --no-edit patch/a patch/b
  publish_fixture
  printf 'new\n' >"$d/upstream-src/new.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'upstream advance'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
}

publish_fixture() {
  git clone -q --bare . "$d/fork.git"
  git push -q "$d/fork.git" patch/a patch/b patch/c
  git clone -q "$d/fork.git" "$d/work"
  cd "$d/work"; git checkout -q fork/main
  cat >"$d/config.toml" <<EOF
[upstream]
url = "$d/upstream.git"
[checks]
patch = [{ name = "patch", run = '''printf 'patch %s %s\n' "\$(git rev-parse HEAD)" "\$(cat conflict.txt 2>/dev/null)" >> "$d/checks"; ! grep -q broken *.txt''' }]
fork = [{ name = "fork", run = '''printf 'fork %s\n' "\$(git rev-parse HEAD)" >> "$d/checks"; ! grep -q broken *.txt && { test ! -e "$d/fork-strict" || test ! -e c.txt; }''' }]
EOF
  expect 0 invoke init
}

# Saves a plan. Preparation may fetch, but maintenance is neither published nor pushed.
save_plan() {
  local before remote; before="$(local_bookmarks)"; remote="$(remote_refs)"
  expect 20 invoke sync --save-plan "$d/plan.json"
  equal "$(local_bookmarks)" "$before" 'saving a plan moved bookmarks'
  equal "$(remote_refs)" "$remote" 'saving a plan pushed'
}

# start CODE ARGS...: starting a task, successful or not, changes nothing in the source.
start() {
  local want="$1"; shift
  local before; before="$(state)"
  expect "$want" invoke repair start "$d/plan.json" "$@"
  equal "$(state)" "$before" 'repair start changed the source'
}

# submit CODE ARGS...: submitting changes nothing visible in the source and pushes nothing.
submit() {
  local want="$1"; shift
  local before remote; before="$(state)"; remote="$(remote_refs)"
  expect "$want" invoke repair submit "$@"
  equal "$(state)" "$before" 'repair submit changed the source'
  equal "$(remote_refs)" "$remote" 'repair submit pushed'
}

# Starts a task for patch/a's conflict in a new directory.
series_task() {
  start 0 --issue series-conflict:patch/a --dir "$1"
}

# Resolves patch/a's conflicted first commit in a task, the way an agent would. jj rebases
# the second commit onto the resolution and keeps both change ids.
resolve_series() {
  tj "$1" edit 'repair/result-'
  printf 'resolved\n' >"$1/conflict.txt"
  tj "$1" status >/dev/null
}

change_ids() { jj --ignore-working-copy log --no-graph -r "$1" -T 'change_id.normal_hex() ++ "\n"'; }

series_conflict() {
  fixture series-conflict
  save_plan
  jq -e '.payload.issues|any(.id=="series-conflict:patch/a")' "$d/plan.json" >/dev/null || fail 'no series-conflict issue'
  t="$d/task"
  series_task "$t"
  contains "$d/out" 'conflicted: conflict.txt' 'start did not report the conflict'
  contains "$d/out" 'may change: conflict.txt' 'conflict scope is not the conflicted path'
  resolve_series "$t"
  submit 0 "$t" --save-plan "$d/next.json" --report "$d/next-report.json"
  jq -e '.kind=="jj-fork-plan" and .payload.outcome=="ready"' "$d/next.json" >/dev/null || fail 'successor not ready'
  jq -e '.kind=="jj-fork-report" and .payload.exit_code==0' "$d/next-report.json" >/dev/null || fail 'no report'
  repaired="$(jq -r '.payload.proposal.checks[]|select(.subject=="patch/a")|.candidate' "$d/next.json")"
  [[ -n $repaired ]] || fail 'repaired series has no patch check target'
  # The repaired copy is based on the target, yet its patch check ran on that exact commit.
  grep -q "^patch $repaired resolved$" "$d/checks" || fail 'repaired series tip was not patch-checked'
  [[ -z "$(jj --ignore-working-copy log --no-graph -r "all() & $repaired" -T commit_id 2>/dev/null)" ]] \
    || fail 'repaired candidate visible in the source before apply'
  copies="$(jq -r '.payload.proposal.mappings[]|select(.subject=="patch/a")|.copy' "$d/plan.json")"
  expected_changes="$(for c in $copies; do jq -r --arg c "$c" '.payload.proposal.candidates[]|select(.commit==$c)|.change' "$d/plan.json"; done | sort)"
  expect 0 "$bin" apply "$d/next.json"
  equal "$(rev patch/a)" "$repaired" 'apply did not publish the exact repaired series'
  equal "$(change_ids 'remote_bookmarks(exact:"main", exact:"upstream")..bookmarks(exact:"patch/a")' | sort)" "$expected_changes" 'series change ids changed'
  equal "$(git show "$(rev fork/main)":conflict.txt)" 'resolved' 'fork does not contain the resolution'
  equal "$(git log --format=%s -1 "$repaired")" 'a adds a' 'series tip metadata changed'
}

check_failure_and_batch() {
  BROKEN_B=1 fixture check-batch
  save_plan
  jq -e '[.payload.issues[].id]|contains(["series-conflict:patch/a","series-check:patch/b"])' "$d/plan.json" >/dev/null \
    || fail 'plan lacks the conflict and check-failure issues'
  start 1 --issue series-check:patch/b --dir "$d/b-unscoped"
  contains "$d/out" 'allow-path' 'check failure started without an explicit scope'
  [[ ! -e "$d/b-unscoped/.jj" ]] || fail 'refused start created a task'
  start 0 --issue series-check:patch/b --dir "$d/b" --allow-path b.txt
  contains "$d/out" 'may change: b.txt' 'explicit scope not recorded'
  tj "$d/b" edit repair/result
  printf 'b fixed\n' >"$d/b/b.txt"; tj "$d/b" status >/dev/null
  series_task "$d/a"; resolve_series "$d/a"
  series_task "$d/a-again"; resolve_series "$d/a-again"
  submit 20 "$d/a" "$d/a-again" --save-plan "$d/next.json"
  contains "$d/out" 'both repair patch/a' 'overlapping destinations not refused'
  [[ ! -e "$d/next.json" ]] || fail 'refused batch saved a plan'
  submit 0 "$d/a" "$d/b" --save-plan "$d/next.json"
  fixed_b="$(jq -r '.payload.proposal.checks[]|select(.subject=="patch/b")|.candidate' "$d/next.json")"
  grep -q "^patch $fixed_b " "$d/checks" || fail 'repaired patch/b was not rechecked'
  expect 0 "$bin" apply "$d/next.json"
  equal "$(git show "$(rev fork/main)":b.txt)" 'b fixed' 'fork lacks the check repair'
  equal "$(git show "$(rev fork/main)":conflict.txt)" 'resolved' 'fork lacks the conflict repair'
}

# Each case changes a fresh task of the same plan and must be rejected without a plan.
reject_case() {
  local name="$1" message="$2"; shift 2
  local t="$d/$name"
  series_task "$t"
  resolve_series "$t"
  "$@" "$t"
  submit 20 "$t" --save-plan "$d/$name.json"
  contains "$d/out" "$message" "$name not rejected for the right reason"
  [[ ! -e "$d/$name.json" ]] || fail "$name saved a successor plan"
}
edit_outside() { printf 'unrelated\n' >"$1/base.txt"; tj "$1" status >/dev/null; }
add_outside() { printf 'new\n' >"$1/new.txt"; tj "$1" status >/dev/null; }
make_executable() { chmod +x "$1/conflict.txt"; tj "$1" status >/dev/null; }
edit_config() { printf '[upstream]\nurl = "x"\n' >"$1/.jj-fork.toml"; tj "$1" status >/dev/null; }
describe() { tj "$1" describe -r repair/result -m 'rewritten description'; }
squash() { tj "$1" squash --from repair/result --into 'repair/result-' --use-destination-message; }
extra_commit() {
  tj "$1" new repair/result -m extra; printf 'more\n' >"$1/conflict.txt"
  tj "$1" bookmark set repair/result -r @
}
extra_parent() {
  tj "$1" new 'repair/result--' -m side
  tj "$1" rebase -r repair/result -o 'repair/result-' -o @
}
new_change() {
  local old; old="$(tj "$1" log --no-graph -r repair/result -T commit_id)"
  tj "$1" duplicate repair/result
  tj "$1" bookmark set repair/result --allow-backwards -r "heads(description(substring:'a adds a') ~ $old)"
  tj "$1" abandon "$old"
}
reorder() {
  tj "$1" rebase -r repair/result -B 'repair/result-'
  tj "$1" bookmark set repair/result --allow-backwards -r 'heads(repair/result::)'
}

unauthorized() {
  fixture unauthorized
  save_plan
  for bad in .jj-fork.toml ../outside /etc/passwd .git/config sub/.jj/x '' .gitmodules; do
    start 1 --issue series-conflict:patch/a --dir "$d/bad-path" --allow-path "$bad"
    [[ ! -e "$d/bad-path/.jj" ]] || fail "start accepted --allow-path '$bad'"
  done
  start 1 --issue series-conflict:patch/a --dir "$d/work/inside"
  contains "$d/out" 'outside the source workspace' 'task inside the source accepted'
  start 1 --issue no-such:issue --dir "$d/none"
  reject_case outside 'changes base.txt, outside' edit_outside
  reject_case added 'changes new.txt, outside' add_outside
  reject_case mode 'changes the mode of conflict.txt' make_executable
  reject_case config 'changes protected path .jj-fork.toml' edit_config
  reject_case metadata 'description or author' describe
  reject_case squashed 'dropped or squashed' squash
  reject_case extra 'a commit was added' extra_commit
  reject_case changed-change 'expected' new_change
  reject_case reordered 'expected' reorder
  reject_case two-parents 'has 2 parents' extra_parent
  series_task "$d/unchanged"
  submit 20 "$d/unchanged" --save-plan "$d/unchanged.json"
  contains "$d/out" 'unchanged' 'unchanged result accepted'
}

# Edits a valid task's signed manifest and expects submission to fail before anything is read.
tampering() {
  fixture tampering
  save_plan
  t="$d/task"; series_task "$t"; resolve_series "$t"
  manifest="$t/.jj/jj-fork-task.json"; cp "$manifest" "$d/manifest.orig"
  for filter in \
    '.payload.scope += ["base.txt"]' \
    '.payload.kind = "fork"' \
    '.payload.parents = ["1111111111111111111111111111111111111111"]' \
    '.payload.seed |= .[:1]' \
    '.payload.issue = "series-check:patch/b"' \
    '.payload.plan.frozen.target = "1111111111111111111111111111111111111111"' \
    '.payload.scope = ["../x"]' \
    '.payload.unexpected = true' \
    '.kind = "jj-fork-plan"' \
    'del(.authentication)'; do
    jq "$filter" "$d/manifest.orig" >"$manifest"
    submit 1 "$t" --save-plan "$d/next.json"
    [[ ! -e "$d/next.json" ]] || fail "tampered task ($filter) saved a plan"
  done
  cp "$d/plan.json" "$manifest"
  submit 1 "$t" --save-plan "$d/next.json"
  # A task signed by another repository's authority is foreign here.
  cp "$d/manifest.orig" "$manifest"
  here="$d"; fixture tampering-foreign; save_plan
  submit 1 "$here/task" --save-plan "$d/next.json"
  contains "$d/out" 'authentication failed' 'foreign task not refused by authentication'
  d="$here"; cd "$d/work"
  submit 0 "$t" --save-plan "$d/next.json"
}

stale_source() {
  fixture stale
  save_plan
  t="$d/task"; series_task "$t"; resolve_series "$t"
  # An unsnapshotted source edit refuses without being snapshotted or lost.
  printf 'user edit\n' >"$d/work/unsaved.txt"
  start 1 --issue series-conflict:patch/a --dir "$d/while-editing"
  submit 1 "$t" --save-plan "$d/next.json"
  equal "$(cat "$d/work/unsaved.txt")" 'user edit' 'source edit lost'
  [[ ! -e "$d/next.json" ]] || fail 'submission over an unsnapshotted edit saved a plan'
  rm "$d/work/unsaved.txt"
  jj --quiet bookmark create user/concurrent -r 'bookmarks(exact:"patch/b")'
  submit 1 "$t" --save-plan "$d/next.json"
  contains "$d/out" 'stale' 'stale source not reported'
  [[ ! -e "$d/next.json" ]] || fail 'stale submission saved a plan'
}

isolation() {
  fixture isolation
  save_plan
  t="$d/task"; series_task "$t"
  [[ -d "$t/.git" && ! -e "$t/.git/commondir" && ! -e "$t/.git/objects/info/alternates" ]] || fail 'task shares a Git object database'
  equal "$(git -C "$t" remote)" '' 'task has Git remotes'
  equal "$(tj "$t" git remote list)" '' 'task has jj remotes'
  ! git -C "$t" config --local --get-regexp '^(remote|credential|url|include|core\.hooksPath|core\.fsmonitor)' >/dev/null || fail 'task Git config copied from the source'
  [[ ! -e "$t/.jj/repo/config.toml" ]] || fail 'task has repository jj config'
  [[ -z "$(find "$t/.git/hooks" -type f ! -name '*.sample' ! -name docs.url 2>/dev/null)" ]] || fail 'task has active hooks'
  key="$(find "$d/work/.jj/repo" -path '*jj-fork/authority' -type f)"
  [[ -n $key ]] || fail 'source authority not found'
  while IFS= read -r f; do ! cmp -s "$key" "$f" || fail "authority key copied into $f"; done < <(find "$t" -type f -size 32c)
  equal "$(jj --ignore-working-copy workspace list -T 'name ++ "\n"')" 'default' 'task registered as a source workspace'
  equal "$(git worktree list | wc -l)" 1 'task registered as a source worktree'
  equal "$(git -C "$t" rev-parse --is-shallow-repository)" 'true' 'task holds full source history'
  [[ -z "$(git -C "$t" rev-list --all --objects 2>/dev/null | grep -F "$(git rev-parse 'origin/patch/a~1')")" ]] || fail 'task contains unrelated source history'
  # Work in the task never reaches the source.
  before="$(state)"
  tj "$t" bookmark create patch/evil -r repair/result
  printf 'task only\n' >"$t/task-only.txt"; tj "$t" status >/dev/null
  equal "$(state)" "$before" 'task work changed the source'
  [[ ! -e "$d/work/task-only.txt" ]] || fail 'task file appeared in the source'
}

new_glue() {
  pair_fixture new-glue
  save_plan
  jq -e '.payload.issues|any(.id=="glue-needed:glue/b+c")' "$d/plan.json" >/dev/null || { jq '.payload.issues' "$d/plan.json" >&2; fail 'no glue-needed issue'; }
  t="$d/task"
  start 0 --issue glue-needed:glue/b+c --dir "$t"
  contains "$d/out" 'conflicted: shared.txt' 'new glue seed is not the conflicted pair merge'
  equal "$(tj "$t" log --no-graph -r 'repair/result' -T 'parents.len()')" 2 'new glue seed is not a merge'
  tj "$t" edit repair/result
  printf 'from b and c\n' >"$t/shared.txt"; tj "$t" status >/dev/null
  submit 0 "$t" --save-plan "$d/next.json"
  glue="$(jq -r '.payload.proposal.view.bookmarks["glue/b+c"].terms[0]' "$d/next.json")"
  [[ $glue != null ]] || fail 'successor does not create glue/b+c'
  expect 0 "$bin" apply "$d/next.json"
  equal "$(rev glue/b+c)" "$glue" 'apply did not publish the repaired glue'
  equal "$(git show "$(rev fork/main)":shared.txt)" 'from b and c' 'fork lacks the glue resolution'
  [[ -n "$(jj --ignore-working-copy log --no-graph -r "$glue & ::fork/main" -T commit_id)" ]] || fail 'fork does not merge the glue'
}

fork_check() {
  UPSTREAM_FILE=new.txt fixture fork-check
  touch "$d/fork-strict"
  save_plan
  jq -e '.payload.issues|any(.id=="fork-check:fork/main")' "$d/plan.json" >/dev/null || { jq '.payload.issues' "$d/plan.json" >&2; fail 'no fork-check issue'; }
  start 1 --issue fork-check:fork/main --dir "$d/unscoped"
  t="$d/task"
  start 0 --issue fork-check:fork/main --dir "$t" --allow-path c.txt
  equal "$(tj "$t" log --no-graph -r 'repair/result' -T 'parents.len()')" 3 'fork seed lacks its exact parents (one per series)'
  tj "$t" edit repair/result
  rm "$t/c.txt"; tj "$t" status >/dev/null
  submit 0 "$t" --save-plan "$d/next.json"
  fork="$(jq -r '.payload.proposal.checks[]|select(.subject=="fork/main")|.candidate' "$d/next.json")"
  grep -q "^fork $fork$" "$d/checks" || fail 'repaired fork merge was not fork-checked'
  expect 0 "$bin" apply "$d/next.json"
  equal "$(rev fork/main)" "$fork" 'apply did not publish the repaired merge'
  ! git cat-file -e "$fork:c.txt" 2>/dev/null || fail 'repair not in the published merge'
  equal "$(git show "$(rev patch/c)":c.txt)" 'c' 'fork repair changed a series'
}

scenarios=(series_conflict check_failure_and_batch unauthorized tampering stale_source isolation new_glue fork_check)
for scenario in "${@:-${scenarios[@]}}"; do
  echo "== $scenario"
  "$scenario"
done
echo "repair: all scenarios passed"
