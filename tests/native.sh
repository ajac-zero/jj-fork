#!/usr/bin/env bash
# Native preparation/transport regressions. Usage: tests/native.sh [path/to/jj-fork]
set -euo pipefail
bin="$(realpath "${1:-target/debug/jj-fork}")"
root="$(mktemp -d /tmp/jj-fork-native.XXXXXX)"
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
export JJ_USER=test JJ_EMAIL=test@example.invalid
export JJ_CONFIG="$root/jjconfig.toml"
export REAL_GIT="$(command -v git)"
printf '[user]\nname="test"\nemail="test@example.invalid"\n' >"$JJ_CONFIG"
fail() {
  if [[ -n "${d:-}" ]]; then
    cat "$d/stdout" "$d/stderr" >&2 2>/dev/null || true
  fi
  echo "FAIL: $*" >&2; exit 1
}
expect_code() {
  local want="$1"; shift
  set +e; "$@" >"$d/stdout" 2>"$d/stderr"; local got=$?; set -e
  if [[ $got != "$want" ]]; then cat "$d/stdout" "$d/stderr" >&2; fail "$* exited $got, want $want"; fi
}
fixture() {
  d="$root/$1"; mkdir -p "$d"
  git init -q -b main "$d/upstream-src"
  printf 'base\n' >"$d/upstream-src/base.txt"
  git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm base
  for n in 1 2; do
    printf '%s\n' "$n" >"$d/upstream-src/history.txt"
    git -C "$d/upstream-src" add .; git -C "$d/upstream-src" commit -qm "history $n"
  done
  git clone -q --bare "$d/upstream-src" "$d/upstream.git"
  if [[ $1 == unshallow ]]; then
    git clone -q --depth 1 "file://$d/upstream.git" "$d/work"
  else
    git clone -q "$d/upstream.git" "$d/work"
  fi
  git -C "$d/work" checkout -qb fork/main
  git clone -q --bare "$d/upstream-src" "$d/fork.git"
  git -C "$d/fork.git" branch fork/main main
  git -C "$d/work" remote set-url origin "$d/fork.git"
  cat >"$d/config.toml" <<EOF2
[upstream]
url = "$d/upstream.git"
[fork]
mirror_branch = "main"
series_prefixes = ["patch/", "tooling/"]
[checks]
patch = [{ name = "patch", run = "true" }]
fork = [{ name = "fork", run = "true" }]
EOF2
  cd "$d/work"
}
invoke() { "$bin" --config "$d/config.toml" "$@"; }

unshallow_preserves_jj() {
  fixture unshallow
  jj git init --colocate >/dev/null 2>&1
  jj config set --repo user.name 'repo-specific name'
  repo_config="$(jj config path --repo 2>/dev/null || true)"
  [[ -n "$repo_config" && -f "$repo_config" ]] || repo_config=".jj/repo/config.toml"
  jj new --quiet main -m 'jj-only ancestor'
  printf 'private\n' >private.txt
  jj bookmark create --quiet jj-only -r @
  jj new --quiet 'bookmarks(exact:"jj-only")' -m 'second jj-only commit'
  printf 'second\n' >second.txt
  jj bookmark create --quiet jj-tip -r @
  jj new --quiet 'bookmarks(exact:"jj-tip")' -m 'saved descendant'
  jj bookmark create --quiet saved -r @
  before_ops="$(jj op log --no-graph -T 'id ++ "\n"')"
  saved_id="$(jj log --no-graph -r 'bookmarks(exact:"saved")' -T commit_id)"
  [[ "$(git rev-parse --is-shallow-repository)" == true ]] || fail 'fixture is not shallow'
  expect_code 0 invoke init
  [[ "$(git rev-parse --is-shallow-repository)" == false ]] || fail 'init did not unshallow'
  [[ -d .jj ]] || fail 'init removed .jj'
  [[ "$(jj log --no-graph -r 'bookmarks(exact:"saved")' -T commit_id)" == "$saved_id" ]] || fail 'jj-only descendant was lost'
  [[ "$(git cat-file -e "$saved_id^{commit}" 2>/dev/null; echo $?)" == 0 ]] || fail 'saved commit absent from Git object database'
  after_ops="$(jj op log --no-graph -T 'id ++ "\n"')"
  grep -Fq "$(head -n1 <<<"$before_ops")" <<<"$after_ops" || fail 'prior operation history lost'
  git cat-file -e "$saved_id:private.txt" || fail 'jj-only first commit content lost'
  git cat-file -e "$saved_id:second.txt" || fail 'jj-only second commit content lost'
  [[ -f "$repo_config" ]] || fail 'repository config removed'
  grep -Fq 'repo-specific name' "$repo_config" || fail 'repository identity configuration lost'
}

transport_partial_and_wrapper() {
  fixture partial
  # Two independent refs with asymmetric tree contents.
  expect_code 0 invoke init
  jj new --quiet main -m 'patch alpha'
  printf 'alpha\n' >alpha.txt; jj bookmark create --quiet patch/alpha -r @
  jj new --quiet main -m 'patch beta'
  printf 'beta\n' >beta.txt; jj bookmark create --quiet patch/beta -r @
  cat >"$d/fork.git/hooks/update" <<'HOOK'
#!/usr/bin/env bash
[[ "$1" != refs/heads/patch/beta ]]
HOOK
  chmod +x "$d/fork.git/hooks/update"
  # jj deliberately disables local hooks for its Git subprocesses. This explicit
  # receive-pack command models server-side hooks, which must still reject beta.
  git config remote.origin.receivepack 'git -c core.hooksPath=hooks receive-pack'
  mkdir "$d/bin"
  cat >"$d/bin/git-wrapper" <<'WRAP'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$WRAPPER_LOG"
printf '%s\n%s\n' "$SSH_AUTH_SOCK" "$GIT_SSH_COMMAND" >>"$WRAPPER_ENV"
exec "$REAL_GIT" "$@"
WRAP
  chmod +x "$d/bin/git-wrapper"
  jj config set --repo git.executable-path "$d/bin/git-wrapper"
  export WRAPPER_LOG="$d/wrapper.log" WRAPPER_ENV="$d/wrapper.env" SSH_AUTH_SOCK="$d/dummy-agent.sock" GIT_SSH_COMMAND="ssh -o IdentityFile=$d/dummy-key"
  expect_code 1 invoke assemble --push
  grep -q 'push\|partial\|reject' "$d/stdout" "$d/stderr" || fail 'partial push failure not reported'
  [[ -s "$WRAPPER_LOG" ]] || fail 'configured Git wrapper was not invoked'
  grep -Fxq "$d/dummy-agent.sock" "$WRAPPER_ENV" || fail 'Git child did not inherit SSH_AUTH_SOCK'
  grep -Fxq "$GIT_SSH_COMMAND" "$WRAPPER_ENV" || fail 'Git child did not inherit GIT_SSH_COMMAND'
  git --git-dir="$d/fork.git" cat-file -e refs/heads/patch/alpha 2>/dev/null || fail 'accepted ref missing'
  git --git-dir="$d/fork.git" cat-file -e refs/heads/patch/beta 2>/dev/null && fail 'rejected ref was published'
  grep -qi 'patch/alpha' "$d/stdout" "$d/stderr" || fail 'output does not identify accepted ref'
  grep -qi 'patch/beta' "$d/stdout" "$d/stderr" || fail 'output does not identify rejected ref'
}

lease_after_fetch() {
  fixture lease
  expect_code 0 invoke init
  jj new --quiet main -m 'patch move'
  printf 'candidate\n' >candidate.txt; jj bookmark create --quiet patch/move -r @
  mkdir "$d/bin"
  cat >"$d/bin/git-wrapper" <<'WRAP'
#!/usr/bin/env bash
if [[ " $* " == *" push "* && ! -e "$INJECTED" ]]; then
  touch "$INJECTED"
  "$REAL_GIT" -C "$RACE" update-ref refs/heads/patch/move "$MOVED"
fi
exec "$REAL_GIT" "$@"
WRAP
  chmod +x "$d/bin/git-wrapper"
  git clone -q "$d/fork.git" "$d/race"
  git -C "$d/race" checkout -qb patch/move origin/main
  printf 'remote movement\n' >"$d/race/remote-only.txt"
  git -C "$d/race" add .; git -C "$d/race" commit -qm moved
  export RACE="$d/fork.git" INJECTED="$d/injected" MOVED="$(git -C "$d/race" rev-parse HEAD)"
  # Transfer the object without creating the guarded series until push begins.
  git -C "$d/race" push -q origin HEAD:refs/heads/race-seed
  jj config set --repo git.executable-path "$d/bin/git-wrapper"
  export WRAPPER_LOG="$d/wrapper.log"
  expect_code 1 invoke assemble --push
  [[ "$(git --git-dir="$d/fork.git" rev-parse refs/heads/patch/move)" == "$MOVED" ]] || fail 'lease overwrote concurrent remote movement'
  git --git-dir="$d/fork.git" cat-file -e refs/heads/patch/move:remote-only.txt || fail 'concurrent remote content lost'
  grep -qi 'partial\|rejected\|changed\|lease\|stale\|unexpectedly moved' "$d/stdout" "$d/stderr" || fail 'lease failure not described'
}

config_layers_and_bad_override() {
  fixture config
  cat >"$d/extra.toml" <<EOF2
[user]
name = "layered user"
email = "layered@example.invalid"
[revset-aliases]
'target()' = 'main@upstream'
[jj-fork.checks]
fork = [{ name = 'layered check', run = 'touch "$d/layered-check"' }]
EOF2
  JJ_CONFIG="$d/extra.toml" JJ_USER='environment user' JJ_EMAIL='environment@example.invalid' expect_code 0 invoke init
  jj new --quiet main -m 'config series'
  printf 'policy\n' >policy.txt; jj bookmark create --quiet patch/policy -r @
  JJ_CONFIG="$d/extra.toml" JJ_USER='environment user' JJ_EMAIL='environment@example.invalid' expect_code 0 invoke assemble --target 'target()'
  [[ -f "$d/layered-check" ]] || fail 'native engine did not apply layered check override'
  fork="$(jj --ignore-working-copy log --no-graph -r 'bookmarks(exact:"fork/main")' -T commit_id)"
  [[ "$(git show -s --format=%cn "$fork")" == 'environment user' ]] || fail 'native commit ignored JJ_USER'
  [[ "$(git show -s --format=%ce "$fork")" == 'environment@example.invalid' ]] || fail 'native commit ignored JJ_EMAIL'
  printf 'not valid = [' >"$d/bad.toml"
  set +e; JJ_CONFIG="$d/bad.toml" invoke check >"$d/stdout" 2>"$d/stderr"; got=$?; set -e
  [[ $got == 1 ]] || fail 'malformed override was not a configuration error'
  grep -qi 'config\|parse\|toml' "$d/stdout" "$d/stderr" || fail 'bad override lacks explicit configuration error'
}

passed=0; failed=0
run_case() {
  local name="$1"; shift
  set +e; (set -e; "$@") >"$root/$name.log" 2>&1; local code=$?; set -e
  if [[ $code == 0 ]]; then passed=$((passed+1)); echo "ok: $name"; else failed=$((failed+1)); echo "not ok: $name"; cat "$root/$name.log"; fi
}
run_case unshallow-preserves-jj unshallow_preserves_jj
run_case layered-config-and-error config_layers_and_bad_override
run_case partial-push-and-configured-wrapper transport_partial_and_wrapper
run_case push-lease-after-fetch lease_after_fetch
echo "$passed passed; $failed failed"
[[ $failed == 0 ]]
