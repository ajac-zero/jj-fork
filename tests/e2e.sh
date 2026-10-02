#!/usr/bin/env bash
# End-to-end scenarios against throwaway local repositories. Usage: tests/e2e.sh [path/to/jj-fork]
set -euo pipefail

bin="$(realpath "${1:-target/debug/jj-fork}")"
root="$(mktemp -d /tmp/jj-fork-e2e.XXXXXX)"
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
export JJ_CONFIG="$root/jjconfig.toml"
printf '[user]\nname = "t"\nemail = "t@example.com"\n' >"$JJ_CONFIG"

pass=0
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { pass=$((pass + 1)); echo "ok: $*"; }
# expect_code CODE CMD... runs CMD and checks its exit code, keeping stdout in $out.
expect_code() {
  local want="$1"; shift
  set +e; out="$("$@" 2>"$root/stderr")"; local got=$?; set -e
  [[ $got == "$want" ]] || { cat "$root/stderr" >&2; echo "$out" >&2; fail "$* exited $got, want $want"; }
}
contains() { grep -qF -- "$2" <<<"$1" || { echo "$1" >&2; fail "missing: $2"; }; }

# Upstream with a few files.
git init -q -b main "$root/upstream-src"
cd "$root/upstream-src"
printf 'line1\nline2\nline3\n' >a.txt
printf 'b\n' >b.txt
git add -A && git commit -qm "upstream: initial"
git clone -q --bare "$root/upstream-src" "$root/upstream.git"

# The fork: mirror main plus patches built on upstream main.
git clone -q "$root/upstream.git" "$root/fork-src"
cd "$root/fork-src"
patch() { # patch NAME FILE CONTENT
  git checkout -q -B "patch/$1" origin/main
  printf '%s\n' "$3" >"$2"
  git add -A && git commit -qm "patch $1"
}
patch clean c.txt "clean patch"
patch conflict a.txt "patched line1"
patch broken d.txt "BROKEN"
git checkout -q -B fork/main origin/main
git merge -q --no-edit patch/clean patch/conflict patch/broken
git clone -q --bare "$root/fork-src" "$root/fork.git"
git -C "$root/fork.git" branch -f main "$(git rev-parse origin/main)"

cat >"$root/config.toml" <<EOF
[upstream]
url = "$root/upstream.git"

[fork]
mirror_branch = "main"
series_prefixes = ["patch/", "tooling/"]

[checks]
patch = [
  { name = "build", run = "! grep -qs BROKEN d.txt", tier = "medium", low_if_errors_at_most = 2 },
]
fork = [
  { name = "build", run = "test -f a.txt" },
]
EOF

# Upstream moves: line1 changes (conflicts with patch/conflict), and a new file appears.
cd "$root/upstream-src"
printf 'upstream line1\nline2\nline3\n' >a.txt
printf 'e\n' >e.txt
git add -A && git commit -qm "upstream: change line1"
git push -q "$root/upstream.git" main

# Scenario 1: check reports each patch.
git clone -q "$root/fork.git" "$root/work"
cd "$root/work"
git checkout -q fork/main
expect_code 0 "$bin" --config "$root/config.toml" init
expect_code 20 "$bin" --config "$root/config.toml" check
contains "$out" "patch/clean clean"
contains "$out" "patch/conflict conflict [tier=low] — first conflict at"
contains "$out" "1 hunks, 4 lines in 1 non-generated of 1 files (a.txt); 1 commits left to replay"
contains "$out" "patch/broken broken [tier=low] — replays cleanly but fails build checks"
ok "check reports clean, conflict, and broken patches with tiers"

expect_code 20 "$bin" --config "$root/config.toml" sync --push
contains "$out" "not applying: some series need an agent"
[[ "$(git -C "$root/fork.git" rev-parse patch/clean)" == "$(git -C "$root/fork-src" rev-parse patch/clean)" ]] ||
  fail "sync moved a patch although others need an agent"
ok "sync changes nothing while a patch needs an agent"

# Scenario 2: with only the clean patch left, sync rebases it, assembles, and pushes.
# Retire the two patches, locally and on the fork remote.
jj bookmark forget --quiet patch/conflict patch/broken --include-remotes
git -C "$root/fork.git" branch -q -D patch/conflict patch/broken
expect_code 0 "$bin" --config "$root/config.toml" sync --push
contains "$out" "patch/clean -> "
contains "$out" "main -> "
contains "$out" "fork/main -> "
upstream_main="$(git -C "$root/upstream.git" rev-parse main)"
[[ "$(git -C "$root/fork.git" merge-base main patch/clean)" == "$upstream_main" ]] || fail "patch/clean not on upstream"
[[ "$(git -C "$root/fork.git" rev-parse main)" == "$upstream_main" ]] || fail "mirror branch not fast-forwarded"
[[ "$(git -C "$root/fork.git" rev-parse fork/main^1)" == "$(git -C "$root/fork.git" rev-parse patch/clean)" ]] ||
  fail "fork/main does not merge patch/clean"
ok "sync rebases, fast-forwards the mirror, assembles, and pushes"

expect_code 0 "$bin" --config "$root/config.toml" sync
contains "$out" "patch/clean up-to-date"
contains "$out" "fork/main already merges upstream and every series"
ok "a second sync is a no-op"

# Scenario 3: two series that conflict with each other get a glue.
for x in x y; do
  jj new --quiet "main@upstream" -m "patch $x"
  printf 'value %s\n' "$x" >z.txt
  jj bookmark create --quiet "patch/z$x" -r @
done
jj new --quiet "main@upstream" -m "tooling: notes"
printf 'notes\n' >NOTES.txt
jj bookmark create --quiet "tooling/notes" -r @
expect_code 20 "$bin" --config "$root/config.toml" sync --push
contains "$out" "fork/main conflict [tier=medium] — merge"
contains "$out" "conflicting pair: patch/zx + patch/zy: add glue/zx+zy with: jj new"
[[ -z "$(jj log --no-graph -r 'conflicts()' -T commit_id)" ]] || fail "conflicted merge left behind although a pair was named"
jj new --quiet 'bookmarks(exact:"patch/zx")' 'bookmarks(exact:"patch/zy")' -m "glue: zx + zy"
printf 'value x and y\n' >z.txt
jj bookmark create --quiet 'glue/zx+zy' -r @
expect_code 0 "$bin" --config "$root/config.toml" assemble --push
[[ "$(git -C "$root/fork.git" show fork/main:z.txt)" == "value x and y" ]] || fail "glue resolution not in fork/main"
[[ "$(git -C "$root/fork.git" show fork/main:NOTES.txt)" == "notes" ]] || fail "tooling series not in fork/main"
ok "a conflicting pair is named; its glue resolves it and tooling/* is merged"

# Moving a series restacks the glue onto the new tip, carrying the resolution.
jj new --quiet 'bookmarks(exact:"patch/zx")' -m "patch x: more"
printf 'more\n' >zx-extra.txt
jj bookmark set --quiet patch/zx -r @
expect_code 0 "$bin" --config "$root/config.toml" assemble --push
contains "$out" "glue/zx+zy -> "
contains "$out" "(restacked)"
[[ "$(git -C "$root/fork.git" show fork/main:z.txt)" == "value x and y" ]] || fail "restacked glue lost its resolution"
git -C "$root/fork.git" cat-file -e fork/main:zx-extra.txt || fail "moved series not in fork/main"
ok "a stale glue is restacked onto the moved series"

# A series inside another series is a stale bookmark; assemble refuses.
jj bookmark create --quiet patch/inner -r 'bookmarks(exact:"patch/zx")-'
expect_code 20 "$bin" --config "$root/config.toml" assemble
contains "$out" "patch/inner is contained in patch/zx"
jj bookmark forget --quiet patch/inner
ok "a nested series is rejected"

# Scenario 4: a shallow, single-branch clone with jj already initialized.
git clone -q --depth 1 --branch fork/main "file://$root/fork.git" "$root/shallow"
cd "$root/shallow"
jj git init --colocate >/dev/null 2>&1
expect_code 0 "$bin" --config "$root/config.toml" assemble --no-checks
contains "$out" "fork/main already merges upstream and every series"
[[ "$(git rev-parse --is-shallow-repository)" == false ]] || fail "still shallow"
ok "a shallow clone is unshallowed and sees the real fork/main parents"

echo "all $pass scenarios passed"
