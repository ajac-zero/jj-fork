#!/usr/bin/env bash
# Black-box transactional engine regressions. Usage: tests/transaction.sh [path/to/jj-fork]
# Each scenario gets fresh local remotes. Failures do not prevent later scenarios running.
# Contract: stale clean `check` exits 10; refusals exit 20. Check worktrees contain the
# exact unpublished commits that success publishes. Preparation operations are allowed;
# assertions compare bookmarks, source files, and candidate visibility, not operation counts.
set -euo pipefail
bin="$(realpath "${1:-target/debug/jj-fork}")"
root="$(mktemp -d /tmp/jj-fork-transaction.XXXXXX)"
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
export JJ_CONFIG="$root/jjconfig.toml"
printf '[user]\nname = "t"\nemail = "t@example.com"\n' >"$JJ_CONFIG"

fail() { echo "FAIL: $*" >&2; exit 1; }
equal() { [[ "$1" == "$2" ]] || fail "$3: got '$1', want '$2'"; }
expect_code() {
  local want="$1"; shift
  set +e; "$@" >"$d/stdout" 2>"$d/stderr"; local got=$?; set -e
  if [[ $got != "$want" ]]; then
    cat "$d/stdout" "$d/stderr" >&2
    fail "$* exited $got, want $want"
  fi
}
rev() { jj --ignore-working-copy log --no-graph -r "bookmarks(exact:\"$1\")" -T commit_id; }
refs() { for name in main patch/a patch/b fork/main; do printf '%s %s\n' "$name" "$(rev "$name")"; done; }
remote_refs() { git -C "$d/fork.git" for-each-ref --format='%(refname) %(objectname)' refs/heads; }
invoke() { "$bin" --config "$d/config.toml" "$@"; }
config() {
  # Commands are literal TOML strings so shell quoting remains unchanged.
  cat >"$d/config.toml" <<EOF
[upstream]
url = "$d/upstream.git"
[fork]
mirror_branch = "main"
[checks]
patch = [{ name = "patch", run = '''${1:-true}''' }]
fork = [{ name = "fork", run = '''${2:-true}''' }]
EOF
}
fixture() {
  d="$root/$1"; mkdir -p "$d"
  git init -q -b main "$d/upstream-src"
  cd "$d/upstream-src"
  printf 'base\n' >base.txt
  printf 'upstream original\n' >conflict.txt
  git add .; git commit -qm base
  git clone -q --bare . "$d/upstream.git"
  git clone -q "$d/upstream.git" "$d/fork-src"
  cd "$d/fork-src"
  git checkout -qb patch/a
  printf 'a1\n' >a.txt; git add .; git commit -qm 'a first'
  printf 'a2\n' >a2.txt; git add .; git commit -qm 'a second'
  git checkout -qb patch/b origin/main
  printf 'b\n' >b.txt; git add .; git commit -qm 'b first'
  git checkout -qb fork/main origin/main
  git merge -q --no-edit patch/a patch/b
  git clone -q --bare . "$d/fork.git"
  git clone -q "$d/fork.git" "$d/work"
  cd "$d/work"; git checkout -q fork/main
  config
  expect_code 0 invoke init
  before="$(refs)"; remote_before="$(remote_refs)"
}
advance_upstream() {
  printf 'new upstream\n' >"$d/upstream-src/new.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'upstream advance'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
}
add_glue() {
  jj new --quiet 'bookmarks(exact:"patch/a")' 'bookmarks(exact:"patch/b")' -m 'glue resolution'
  printf 'resolution\n' >glue.txt
  jj bookmark create --quiet glue/a+b -r @
  expect_code 0 invoke assemble --push
  # Leave the working copy away from series/glue to distinguish source edits from candidates.
  jj new --quiet 'bookmarks(exact:"fork/main")' -m 'working copy'
}

exact_candidate() {
  fixture exact
  advance_upstream
  config 'if test -f a.txt; then label=a; else label=b; fi; printf "%s %s %s\n" "$label" "$(git rev-parse HEAD)" "$(git rev-parse HEAD^{tree})" >> "'$d'/checked"' \
    'printf "fork %s %s\n" "$(git rev-parse HEAD)" "$(git rev-parse HEAD^{tree})" >> "'$d'/checked"'
  expect_code 0 invoke sync --push
  for label in a b fork; do
    branch="patch/$label"; [[ $label != fork ]] || branch=fork/main
    read -r _ checked_commit checked_tree < <(grep "^$label " "$d/checked" | tail -1)
    [[ -n "$checked_commit" && -n "$checked_tree" ]] || fail "check did not record $label"
    equal "$(git -C "$d/fork.git" rev-parse "$branch^{tree}")" "$checked_tree" "$branch checked tree"
  done
  echo 'all series and fork checked trees match publication'
  for label in a b fork; do
    branch="patch/$label"; [[ $label != fork ]] || branch=fork/main
    read -r _ checked_commit checked_tree < <(grep "^$label " "$d/checked" | tail -1)
    # The native engine checks the exact commit, including its parents, not just an equal tree.
    equal "$(git -C "$d/fork.git" rev-parse "$branch")" "$checked_commit" "$branch checked commit"
  done
}

duplicate_history() {
  fixture history
  old_a="$(rev patch/a)"; old_base="$(rev main)"
  old_changes="$(jj log --no-graph -r "$old_base..$old_a" -T 'commit_id ++ " " ++ change_id ++ "\n"')"
  old_history="$(git log --format='%H %P %B' "$old_a" --not "$old_base")"
  jj new --quiet "$old_a" -m 'unrelated descendant'
  printf 'descendant\n' >descendant.txt
  jj bookmark create --quiet saved -r @
  saved="$(rev saved)"
  jj new --quiet 'bookmarks(exact:"fork/main")'
  advance_upstream
  expect_code 0 invoke sync --push
  while read -r old_commit old_change; do
    equal "$(jj log --no-graph -r "$old_change" -T commit_id)" "$old_commit" 'old change id moved'
  done <<<"$old_changes"
  equal "$(git log --format='%H %P %B' "$old_a" --not "$old_base")" "$old_history" 'old history/descriptions changed'
  equal "$(rev saved)" "$saved" 'unrelated descendant rewritten'
  equal "$(git show "$saved:descendant.txt")" descendant 'descendant content'
}

later_failure() {
  fixture later-failure; advance_upstream
  config '! test -f b.txt'
  expect_code 20 invoke sync --push
  equal "$(refs)" "$before" 'later failure published earlier series/mirror/fork'
  equal "$(remote_refs)" "$remote_before" 'later failure pushed refs'
}

fork_failure() {
  local mode="$1"
  fixture "fork-failure-$mode"; add_glue
  if [[ $mode == sync ]]; then
    advance_upstream
  else
    jj new --quiet 'bookmarks(exact:"patch/a")' -m 'a advance'
    printf 'extra\n' >extra.txt
    jj bookmark set --quiet patch/a -r @
    jj new --quiet 'bookmarks(exact:"fork/main")'
  fi
  before="$(refs)"; glue_before="$(rev glue/a+b)"; remote_before="$(remote_refs)"
  config true false
  expect_code 20 invoke "$mode" --push
  equal "$(refs)" "$before" "$mode fork failure published planned series"
  equal "$(rev glue/a+b)" "$glue_before" "$mode fork failure published glue restack"
  equal "$(remote_refs)" "$remote_before" "$mode fork failure pushed refs"
}

local_change() {
  fixture local-change; advance_upstream
  old_a="$(rev patch/a)"
  # A real jj operation, triggered only once even if the engine runs several checks.
  config true 'if test ! -e "'$d'/changed"; then jj -R "'$d'/work" bookmark create user/concurrent -r "'$old_a'" && touch "'$d'/changed"; fi'
  expect_code 20 invoke sync --push
  [[ -e "$d/changed" ]] || fail 'concurrent bookmark check never ran'
  equal "$(rev user/concurrent)" "$old_a" 'concurrent jj bookmark was overwritten'
  equal "$(refs)" "$before" 'stale plan published after local jj operation'
  equal "$(remote_refs)" "$remote_before" 'stale plan pushed after local jj operation'
}

working_copy_edit() {
  fixture source-edit; advance_upstream
  config true 'printf "user unsnapshotted edit\n" > "'$d'/work/base.txt"'
  expect_code 20 invoke sync --push
  equal "$(cat base.txt)" 'user unsnapshotted edit' 'source working-copy edit lost'
  # --ignore-working-copy prevents these assertions from snapshotting the edit themselves.
  equal "$(refs)" "$before" 'plan published despite source working-copy edit'
  equal "$(remote_refs)" "$remote_before" 'source edit refusal still pushed'
}

mirror_move() {
  fixture mirror-move; advance_upstream
  git -C "$d/fork-src" checkout -q main
  printf 'remote writer\n' >"$d/fork-src/remote.txt"
  git -C "$d/fork-src" add .; git -C "$d/fork-src" commit -qm 'independent mirror move'
  moved="$(git -C "$d/fork-src" rev-parse HEAD)"
  config true 'git -C "'$d'/fork-src" push -q "'$d'/fork.git" main'
  expect_code 20 invoke sync --push
  equal "$(git -C "$d/fork.git" rev-parse main)" "$moved" 'concurrent mirror move overwritten'
  equal "$(remote_refs | grep -v 'refs/heads/main ')" "$(printf '%s\n' "$remote_before" | grep -v 'refs/heads/main ')" 'refusal still pushed other refs'
}

intermediate_conflict() {
  fixture intermediate
  jj new --quiet 'main@upstream' -m 'intermediate conflicting edit'
  printf 'series conflicting\n' >conflict.txt
  jj new --quiet -m 'revert conflicting edit'
  printf 'upstream original\n' >conflict.txt
  jj bookmark create --quiet patch/intermediate -r @
  tip="$(rev patch/intermediate)"
  jj new --quiet 'bookmarks(exact:"fork/main")'
  printf 'upstream conflicting\n' >"$d/upstream-src/conflict.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm 'upstream conflict'
  git -C "$d/upstream-src" push -q "$d/upstream.git" main
  # Tip has no net change to conflict.txt, but the first commit cannot replay cleanly.
  expect_code 20 invoke sync --push
  equal "$(rev patch/intermediate)" "$tip" 'intermediate conflict series moved'
  equal "$(refs)" "$before" 'intermediate conflict published earlier plans'
  equal "$(remote_refs)" "$remote_before" 'intermediate conflict pushed'
}

unpublished_checks() {
  fixture unpublished; advance_upstream
  # Record source-visible commits during checks. --ignore-working-copy is observational:
  # preparing/fetching/snapshotting may publish operations; candidate commits may not appear.
  cat >"$d/observe.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
id=\$(git rev-parse HEAD)
jj -R "$d/work" --ignore-working-copy log --no-graph -r 'all()' -T 'commit_id ++ "\\n"' >"$d/visible"
if grep -qx "\$id" "$d/visible"; then echo "\$id" >>"$d/leaked"; fi
printf '%s\n' "\$id" >>"$d/observed"
EOF
  config 'bash "'$d'/observe.sh"' 'bash "'$d'/observe.sh"'
  expect_code 10 invoke check
  [[ -s "$d/observed" ]] || fail 'check observer did not run'
  [[ ! -e "$d/leaked" ]] || fail 'candidate was visible in source jj repository during checks'
  equal "$(refs)" "$before" 'check command published bookmarks'
  equal "$(remote_refs)" "$remote_before" 'check command pushed'
  jj --ignore-working-copy log --no-graph -r 'all()' -T 'commit_id ++ "\n"' >"$d/visible-after"
  while read -r id; do
    ! grep -qx "$id" "$d/visible-after" || fail 'check command published a candidate after checks returned'
  done <"$d/observed"
  # sync must also keep its fork-merge candidate unpublished while fork checks execute.
  rm -f "$d/observed"
  expect_code 0 invoke sync --push
  [[ -s "$d/observed" ]] || fail 'sync observer did not run'
  [[ ! -e "$d/leaked" ]] || fail 'sync candidate was visible in source jj repository during checks'
}

integrity() {
  local mutation="$1"
  fixture "integrity-$mutation"; advance_upstream
  case "$mutation" in
    head) command='git checkout --detach HEAD^' ;;
    staged) command='printf bad > base.txt; git add base.txt' ;;
    unstaged) command='printf bad > base.txt' ;;
    artifact) command='printf harmless > build-artifact' ;;
  esac
  config "$command"
  if [[ $mutation == artifact ]]; then
    expect_code 0 invoke sync --push
    equal "$(git -C "$d/fork.git" show patch/a:base.txt)" base 'artifact changed published tracked file'
    ! git -C "$d/fork.git" cat-file -e patch/a:build-artifact 2>/dev/null || fail 'build artifact was snapshotted'
  else
    expect_code 20 invoke sync --push
    grep -q 'candidate integrity' "$d/stdout" || fail 'missing integrity failure diagnostic'
    equal "$(refs)" "$before" 'mutating check published refs'
    equal "$(remote_refs)" "$remote_before" 'mutating check pushed refs'
  fi
}

post_publication_failure() {
  local kind="$1"
  fixture "partial-$kind"; advance_upstream
  lock=""
  case "$kind" in
    head) lock="$d/work/.git/HEAD.lock" ;;
    export) lock="$d/work/.git/refs/heads/patch/a.lock" ;;
    checkout)
      printf 'new.txt\n' >>.git/info/exclude
      config true 'printf "user obstruction\n" > "'$d'/work/new.txt"'
      ;;
  esac
  if [[ -n "$lock" ]]; then config true 'touch "'$lock'"'; fi
  expect_code 1 invoke sync --push
  grep -q 'published operation' "$d/stderr" || fail 'partial publication not reported'
  grep -q 'nothing was restored or pushed' "$d/stderr" || fail 'partial failure outcome unclear'
  [[ "$(rev patch/a)" != "$(git -C "$d/fork.git" rev-parse patch/a)" ]] ||
    fail 'published series was restored after synchronization failure'
  [[ "$(rev fork/main)" != "$(git -C "$d/fork.git" rev-parse fork/main)" ]] ||
    fail 'published fork was restored after synchronization failure'
  equal "$(remote_refs)" "$remote_before" 'partial synchronization failure still pushed'
  if [[ $kind == checkout ]]; then
    equal "$(cat new.txt)" 'user obstruction' 'checkout overwrote ignored user file'
    grep -q 'skipped' "$d/stderr" || fail 'skipped checkout update not reported'
  else
    rm -f "$lock"
  fi
}

glue_repair() {
  fixture glue-repair
  for label in c d; do
    jj new --quiet 'main@upstream' -m "patch $label"
    printf '%s\n' "$label" >conflict.txt
    jj bookmark create --quiet "patch/$label" -r @
  done
  jj new --quiet 'bookmarks(exact:"patch/c")' 'bookmarks(exact:"patch/d")' -m glue
  printf 'c and d\n' >conflict.txt
  jj bookmark create --quiet glue/c+d -r @
  expect_code 0 invoke assemble --push
  old_glue="$(rev glue/c+d)"
  jj new --quiet 'bookmarks(exact:"patch/c")' -m 'c changed'
  printf 'changed c\n' >conflict.txt
  jj bookmark set --quiet patch/c -r @
  jj new --quiet 'bookmarks(exact:"fork/main")'
  old_wc="$(jj log --no-graph -r @ -T commit_id)"
  before="$(refs)"; remote_before="$(remote_refs)"
  expect_code 20 invoke assemble --push
  equal "$(refs)" "$before" 'glue repair moved series or fork'
  equal "$(jj --ignore-working-copy log --no-graph -r @ -T commit_id)" "$old_wc" 'glue repair changed checkout'
  [[ "$(rev glue/c+d)" != "$old_glue" ]] || fail 'conflicting restacked glue was discarded'
  equal "$(jj --ignore-working-copy log --no-graph -r 'bookmarks(exact:"glue/c+d")' -T conflict)" true 'repair glue is not conflicted'
  equal "$(remote_refs)" "$remote_before" 'glue repair was pushed'
}

unexplained_repair() {
  fixture unexplained-repair
  # With a single conflicted series there is no pair to name. Deliberate tracked
  # deletions keep the remote-membership safeguard valid when submitting its repair.
  jj bookmark delete --quiet patch/a patch/b
  for label in left right; do
    jj new --quiet 'main@upstream' -m "$label"
    printf '%s\n' "$label" >conflict.txt
    jj bookmark create --quiet "input/$label" -r @
  done
  jj new --quiet 'bookmarks(exact:"input/left")' 'bookmarks(exact:"input/right")' -m 'conflicted series'
  jj bookmark create --quiet patch/unresolved -r @
  jj new --quiet 'bookmarks(exact:"fork/main")'
  old_wc="$(jj log --no-graph -r @ -T commit_id)"
  before="$(refs)"; remote_before="$(remote_refs)"
  expect_code 20 invoke assemble --push
  grep -q 'no single pair conflicts' "$d/stdout" || fail 'unexplained conflict has no actionable repair'
  equal "$(refs)" "$before" 'unexplained repair moved fork'
  [[ "$(jj --ignore-working-copy log --no-graph -r @ -T commit_id)" != "$old_wc" ]] || fail 'repair head not checked out'
  equal "$(jj --ignore-working-copy log --no-graph -r "$old_wc" -T commit_id)" "$old_wc" 'old working copy abandoned'
  equal "$(remote_refs)" "$remote_before" 'unexplained repair pushed'
  repair="$(jj --ignore-working-copy log --no-graph -r @ -T change_id)"
  printf 'resolved\n' >conflict.txt
  expect_code 0 invoke assemble --candidate "$repair"
  equal "$(git show "$(rev fork/main):conflict.txt")" resolved 'resolved candidate not assembled'
  equal "$(jj --ignore-working-copy log --no-graph -r @ -T empty)" true 'successful working copy is not empty'
  equal "$(git rev-parse HEAD)" "$(rev fork/main)" 'Git HEAD did not follow fork'
}

nested_glues() {
  fixture nested-glues; add_glue
  jj new --quiet 'main@upstream' -m 'patch c'
  printf 'c\n' >c.txt
  jj bookmark create --quiet patch/c -r @
  jj new --quiet 'bookmarks(exact:"glue/a+b")' 'bookmarks(exact:"patch/c")' -m 'outer glue'
  printf 'outer\n' >outer.txt
  jj bookmark create --quiet glue/a+b+c -r @
  expect_code 0 invoke assemble --push
  old_inner="$(rev glue/a+b)"; old_outer="$(rev glue/a+b+c)"
  jj new --quiet 'bookmarks(exact:"patch/a")' -m 'a new tip'
  printf 'a3\n' >a3.txt
  jj bookmark set --quiet patch/a -r @
  jj new --quiet 'bookmarks(exact:"fork/main")'
  expect_code 0 invoke assemble --push
  [[ "$(rev glue/a+b)" != "$old_inner" && "$(rev glue/a+b+c)" != "$old_outer" ]] || fail 'nested glues were not both restacked'
  actual="$(git show -s --format=%P "$(rev glue/a+b+c)" | tr ' ' '\n' | sort)"
  expected="$(printf '%s\n' "$(rev glue/a+b)" "$(rev patch/c)" | sort)"
  equal "$actual" "$expected" 'outer glue does not merge new inner glue and series c'
  for pair in 'glue.txt:resolution' 'outer.txt:outer' 'a3.txt:a3' 'c.txt:c'; do
    equal "$(git -C "$d/fork.git" show "fork/main:${pair%%:*}")" "${pair#*:}" 'nested glue content lost'
  done
}

initialized_native_commands() {
  fixture native-commands; advance_upstream
  mkdir "$d/bin"
  printf '#!/usr/bin/env bash\necho "jj CLI must not be invoked for initialized maintenance: $*" >&2\nexit 99\n' >"$d/bin/jj"
  chmod +x "$d/bin/jj"
  expect_code 10 env PATH="$d/bin:$PATH" "$bin" --config "$d/config.toml" check
  equal "$(refs)" "$before" 'native check published bookmarks'
  expect_code 0 env PATH="$d/bin:$PATH" "$bin" --config "$d/config.toml" sync --push
  equal "$(git -C "$d/fork.git" show fork/main:new.txt)" 'new upstream' 'native transport did not publish upstream content'
}

unchanged_candidate_stale() {
  local kind="$1"
  fixture "unchanged-stale-$kind"
  # This existing candidate needs checks but no bookmark move or new checkout.
  jj new --quiet 'bookmarks(exact:"patch/a")' 'bookmarks(exact:"patch/b")' -m 'local candidate'
  jj bookmark set --quiet fork/main -r @ --allow-backwards
  jj new --quiet
  before="$(refs)"
  case "$kind" in
    operation) config true 'jj -R "'$d'/work" bookmark create user/concurrent -r "bookmarks(exact:patch/a)"' ;;
    source) config true 'printf "source edit\n" > "'$d'/work/base.txt"' ;;
  esac
  expect_code 20 invoke assemble --push
  equal "$(refs)" "$before" 'stale existing candidate changed bookmarks'
  equal "$(remote_refs)" "$remote_before" 'stale existing candidate was pushed'
  if [[ $kind == operation ]]; then
    equal "$(rev user/concurrent)" "$(rev patch/a)" 'concurrent bookmark lost'
  else
    equal "$(cat base.txt)" 'source edit' 'concurrent source edit lost'
  fi
}

passed=0; failed=0
run_case() {
  local name="$1"; shift
  # Keep errexit active inside each scenario (an `if function` would disable it).
  set +e
  (set -e; "$@") >"$root/$name.log" 2>&1
  local code=$?
  set -e
  if [[ $code == 0 ]]; then
    passed=$((passed + 1)); echo "ok: $name"
  else
    failed=$((failed + 1)); echo "not ok: $name"; cat "$root/$name.log"
  fi
}
run_case exact-candidate exact_candidate
run_case duplicate-history duplicate_history
run_case later-series-failure later_failure
run_case sync-fork-failure fork_failure sync
run_case assemble-fork-failure fork_failure assemble
run_case local-jj-change local_change
run_case unsnapshotted-source-edit working_copy_edit
run_case concurrent-mirror-move mirror_move
run_case intermediate-conflict intermediate_conflict
run_case unpublished-check-candidates unpublished_checks
for mutation in head staged unstaged artifact; do run_case "integrity-$mutation" integrity "$mutation"; done
for kind in head export checkout; do run_case "post-publication-$kind-failure" post_publication_failure "$kind"; done
run_case conflicted-glue-repair glue_repair
run_case unexplained-merge-repair unexplained_repair
run_case nested-glue-ordering nested_glues
run_case initialized-native-commands initialized_native_commands
for kind in operation source; do run_case "unchanged-candidate-stale-$kind" unchanged_candidate_stale "$kind"; done
echo "$passed passed; $failed failed"
[[ $failed == 0 ]]
