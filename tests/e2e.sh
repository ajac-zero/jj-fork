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

# With three conflicting series, pairs are listed in name order, not commit-id order, so "the
# first pair" an agent is told to glue is the same on every run.
jj new --quiet "main@upstream" -m "patch w"
printf 'value w\n' >z.txt
jj bookmark create --quiet patch/zw -r @
expect_code 20 "$bin" --config "$root/config.toml" assemble --no-fetch
pairs="$(grep -o 'conflicting pair: [^:]*' <<<"$out")"
[[ "$pairs" == "conflicting pair: patch/zw + patch/zx
conflicting pair: patch/zw + patch/zy
conflicting pair: patch/zx + patch/zy" ]] || { echo "$out" >&2; fail "conflicting pairs not in name order"; }
jj abandon --quiet 'bookmarks(exact:"patch/zw")'

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

# A series inside another series is a stale bookmark; assemble refuses. (Unpublished commits, so
# reconcile does not mistake it for a series deleted after being published.)
zx_tip="$(jj log --no-graph -r 'bookmarks(exact:"patch/zx")' -T commit_id)"
jj new --quiet "$zx_tip" -m "patch x: unpublished 1"
jj new --quiet -m "patch x: unpublished 2"
jj bookmark create --quiet patch/inner -r @-
jj bookmark set --quiet patch/zx -r @
expect_code 20 "$bin" --config "$root/config.toml" assemble
contains "$out" "patch/inner is contained in patch/zx"
jj bookmark forget --quiet patch/inner
jj bookmark set --quiet patch/zx -r "$zx_tip" --allow-backwards
ok "a nested series is rejected"

# Scenario 4: a shallow, single-branch clone with jj already initialized.
git clone -q --depth 1 --branch fork/main "file://$root/fork.git" "$root/shallow"
cd "$root/shallow"
jj git init --colocate >/dev/null 2>&1
expect_code 0 "$bin" --config "$root/config.toml" assemble --no-checks
contains "$out" "fork/main already merges upstream and every series"
[[ "$(git rev-parse --is-shallow-repository)" == false ]] || fail "still shallow"
ok "a shallow clone is unshallowed and sees the real fork/main parents"

# Scenario 5: someone pushes a series while assemble --push is checking its candidate. The fork
# check stands in for that concurrent push; it runs after jj-fork's first fetch and before its
# push, and pushing the same commit again later changes nothing.
git clone -q "$root/fork.git" "$root/other"
cd "$root/other"
git checkout -q -b patch/late origin/main
printf 'late\n' >late.txt
git add -A && git commit -qm "patch late"
sed "s|^fork = \[|&\n  { name = \"concurrent push\", run = \"git -C $root/other push -q origin patch/late\" },|" \
  "$root/config.toml" >"$root/config-concurrent.toml"
grep -qF 'concurrent push' "$root/config-concurrent.toml" || fail "concurrent-push check not added"

cd "$root/work"
jj new --quiet 'bookmarks(exact:"patch/zy")' -m "patch y: more"
printf 'more\n' >zy-extra.txt
jj bookmark set --quiet patch/zy -r @
before="$(git -C "$root/fork.git" for-each-ref)"
expect_code 20 "$bin" --config "$root/config-concurrent.toml" assemble --push
contains "$out" "origin changed while jj-fork ran; nothing pushed. Changed on origin:"
contains "$out" "  patch/late"
contains "$out" "rerun: jj fork assemble --push"
[[ "$(git -C "$root/fork.git" for-each-ref | grep -v 'refs/heads/patch/late$')" == "$before" ]] ||
  fail "a ref other than patch/late changed on the fork remote although the push was refused"
ok "assemble --push refuses when the remote changed during the run"

expect_code 0 "$bin" --config "$root/config-concurrent.toml" assemble --push
for file in late.txt zy-extra.txt; do
  git -C "$root/fork.git" cat-file -e "fork/main:$file" || fail "rerun did not merge $file into fork/main"
done
[[ "$(git -C "$root/fork.git" show fork/main:z.txt)" == "value x and y" ]] || fail "glue resolution lost on rerun"
ok "a rerun merges the concurrently pushed series and pushes"

# Scenario 6: a clone whose jj state predates changes on the fork remote, the way an Amp orb
# starts from an old snapshot and then runs plain `git fetch`. Local bookmarks that the remote
# moved, restacked, or deleted must not win; local-only work must survive and be pushed.
git clone -q "$root/fork.git" "$root/editor"
git -C "$root/editor" checkout -q -b patch/ahead origin/main
printf 'ahead\n' >"$root/editor/ahead.txt"
git -C "$root/editor" add -A && git -C "$root/editor" commit -qm "patch ahead"
git -C "$root/editor" push -q origin patch/ahead

git clone -q "$root/fork.git" "$root/orb"
cd "$root/orb"
git checkout -q fork/main
expect_code 0 "$bin" --config "$root/config.toml" init
expect_code 0 "$bin" --config "$root/config.toml" assemble --push
# The snapshot knows these bookmarks but does not follow the remote's moves.
jj bookmark untrack --quiet 'glob:*' --remote origin
published_notes="$(git -C "$root/fork.git" rev-parse tooling/notes)"

# The fork remote changes: a series is deleted, a glue is restacked onto a moved series, and
# another series advances.
cd "$root/editor"
git fetch -q origin
git push -q origin --delete tooling/notes
git checkout -q -B patch/clean origin/patch/clean
printf 'clean 2\n' >clean2.txt
git add -A && git commit -qm "patch clean: more"
git push -q origin patch/clean
git checkout -q -B patch/zx origin/patch/zx
printf 'zx 2\n' >zx2.txt
git add -A && git commit -qm "patch zx: again"
git push -q origin patch/zx
git checkout -q -B 'glue/zx+zy' 'origin/glue/zx+zy'
git reset -q --hard patch/zx
git merge -q --no-commit --no-ff origin/patch/zy >/dev/null || true
printf 'value x and y\n' >z.txt
git add -A && git commit -qm "glue: zx + zy (restacked)"
git push -q -f origin 'glue/zx+zy'
want_glue="$(git rev-parse HEAD)"
want_clean="$(git rev-parse patch/clean)"
want_zx="$(git rev-parse patch/zx)"

# In the clone: local work, then plain git fetch, as Amp does.
cd "$root/orb"
jj new --quiet 'bookmarks(exact:"patch/ahead")' -m "patch ahead: local"
printf 'local\n' >ahead-local.txt
jj bookmark set --quiet patch/ahead -r @
jj new --quiet "main@upstream" -m "tooling: new local"
printf 'new\n' >newlocal.txt
jj bookmark create --quiet tooling/newlocal -r @
git fetch -q --prune origin
want_ahead="$(jj log --no-graph -r 'bookmarks(exact:"patch/ahead")' -T commit_id)"

expect_code 0 "$bin" --config "$root/config.toml" assemble --no-fetch --push
contains "$out" "reconciled: tooling/notes"
contains "$out" "reconciled: patch/clean"
contains "$out" "reconciled: glue/zx+zy"
contains "$out" "reconciled: patch/ahead kept"
fk="$root/fork.git"
git -C "$fk" rev-parse -q --verify tooling/notes >/dev/null && fail "deleted series was pushed back"
[[ -z "$(jj bookmark list --color=never 'tooling/notes' 2>/dev/null)" ]] || fail "stale local bookmark tooling/notes remains"
git -C "$fk" cat-file -e fork/main:NOTES.txt 2>/dev/null && fail "deleted series still merged into fork/main"
! git -C "$fk" rev-list fork/main^@ | grep -qx "$published_notes" || fail "fork/main merges the deleted series"
ok "a series deleted on the remote is neither merged nor pushed back"

[[ "$(git -C "$fk" rev-parse 'glue/zx+zy')" == "$want_glue" ]] || fail "glue is not the remote's version"
[[ "$(git -C "$fk" rev-parse patch/zx)" == "$want_zx" ]] || fail "patch/zx moved off the remote's commit"
[[ "$(git -C "$fk" rev-parse patch/clean)" == "$want_clean" ]] || fail "patch/clean moved off the remote's commit"
for file in clean2.txt zx2.txt; do
  git -C "$fk" cat-file -e "fork/main:$file" || fail "fork/main lacks $file from the remote's series"
done
[[ "$(git -C "$fk" show fork/main:z.txt)" == "value x and y" ]] || fail "fork/main lost the glue resolution"
[[ -z "$(jj log --no-graph -r 'conflicts()' -T commit_id)" ]] || fail "conflicted commits left behind"
[[ -z "$(jj bookmark list --color=never --conflicted)" ]] || fail "conflicted bookmark left behind"
ok "restacked glue and advanced series use the remote's commits"

[[ "$(git -C "$fk" rev-parse patch/ahead)" == "$want_ahead" ]] || fail "ahead series not kept and pushed"
git -C "$fk" cat-file -e fork/main:ahead-local.txt || fail "ahead series not merged"
git -C "$fk" rev-parse -q --verify tooling/newlocal >/dev/null || fail "new local series not pushed"
git -C "$fk" cat-file -e fork/main:newlocal.txt || fail "new local series not merged"
ok "new and ahead local series are kept and pushed"

# Scenario 7: a stale clone must not drop series it does not know about. Someone else pushes

# Scenario 7: a stale clone must not drop series it does not know about. Someone else pushes
# patch/theirs. jj-fork normally tracks it before assembling; when this clone cannot (simulated
# by a jj wrapper that fails `bookmark track`), the remote series is untracked and has no local
# bookmark, so assemble and assemble --push refuse and name it. Deleting a series on purpose
# in this clone still works.
git clone -q "$root/fork.git" "$root/stale"
cd "$root/stale"
git checkout -q fork/main
expect_code 0 "$bin" --config "$root/config.toml" init
expect_code 0 "$bin" --config "$root/config.toml" assemble --push
git -C "$root/editor" fetch -q origin
git -C "$root/editor" checkout -q -B patch/theirs origin/main
printf 'theirs\n' >"$root/editor/theirs.txt"
git -C "$root/editor" add -A && git -C "$root/editor" commit -qm "patch theirs"
git -C "$root/editor" push -q origin patch/theirs
before="$(git -C "$root/fork.git" for-each-ref)"
git fetch -q origin
jj bookmark delete --quiet patch/theirs 2>/dev/null || true
jj bookmark untrack --quiet patch/theirs --remote origin
mkdir "$root/nobin"
printf '#!/usr/bin/env bash\n[[ "${1:-}" == bookmark && "${2:-}" == track ]] && exit 1\nexec %s "$@"\n' "$(command -v jj)" >"$root/nobin/jj"
chmod +x "$root/nobin/jj"
untracking="env PATH=$root/nobin:$PATH"
expect_code 20 $untracking "$bin" --config "$root/config.toml" assemble --no-fetch --no-checks --push
contains "$out" "nothing pushed"
contains "$out" "  patch/theirs"
[[ "$(git -C "$root/fork.git" for-each-ref)" == "$before" ]] || fail "refused push still changed the remote"
jj new --quiet "main@upstream" -m "tooling: stale local"
printf 'stale\n' >stale.txt
jj bookmark create --quiet tooling/stale -r @
expect_code 20 $untracking "$bin" --config "$root/config.toml" assemble --no-fetch --no-checks
contains "$out" "fork/main not moved"
contains "$out" "  patch/theirs"
jj bookmark forget --quiet tooling/stale
ok "assemble and assemble --push refuse a remote series that is neither merged nor deleted, and name it"

expect_code 0 "$bin" --config "$root/config.toml" assemble --no-fetch --push
git -C "$root/fork.git" cat-file -e fork/main:theirs.txt || fail "tracked series not merged"
jj bookmark delete --quiet patch/theirs
expect_code 0 "$bin" --config "$root/config.toml" assemble --no-fetch --push
[[ -z "$(jj bookmark list --color=never patch/theirs -T 'if(!remote && present, name)')" ]] || fail "deleted series came back locally"
git -C "$root/fork.git" cat-file -e fork/main:theirs.txt 2>/dev/null && fail "deleted series still merged"
ok "deleting a series locally still works"

# Scenario 8: a failing go test is retried up to 3 times (any pass is flaky and does not block),
# then run 5 times on bare upstream: it blocks only if every retry fails and all 5 upstream runs
# pass. A fake `go` makes this deterministic: the full run always fails TestFlaky, retries in the
# candidate pass on the FAKE_PASS_ON-th try (never when unset), and the baseline worktree's
# `-count=5` run passes or fails per FAKE_UPSTREAM. FAKE_COUNTER counts the retries.
git clone -q "$root/fork.git" "$root/gotest"
cd "$root/gotest"
git checkout -q fork/main
expect_code 0 "$bin" --config "$root/config.toml" init
expect_code 0 "$bin" --config "$root/config.toml" assemble --push
jj new --quiet "main@upstream" -m "tooling: go"
printf 'go\n' >go.txt
jj bookmark create --quiet tooling/go -r @
mkdir "$root/gobin"
cat >"$root/gobin/go" <<'GO'
#!/usr/bin/env bash
if [[ "$*" != *" -run "* ]]; then
  printf -- '--- FAIL: TestFlaky (0.00s)\nFAIL\nFAIL\texample.com/m\t0.1s\n'
  exit 1
fi
if [[ "$PWD" == */baseline ]]; then
  [[ "$*" == *"-count=5 -run ^TestFlaky\$ example.com/m"* ]] || exit 3
  [[ "${FAKE_UPSTREAM:-}" == pass ]]
  exit
fi
n=$(($(cat "$FAKE_COUNTER" 2>/dev/null || echo 0) + 1))
echo "$n" >"$FAKE_COUNTER"
[[ -n "${FAKE_PASS_ON:-}" && $n -ge $FAKE_PASS_ON ]]
GO
chmod +x "$root/gobin/go"
sed 's|^fork = \[|&\n  { name = "go", run = "go test ./...", kind = "go-test" },|' "$root/config.toml" >"$root/config-go.toml"
grep -qF 'kind = "go-test"' "$root/config-go.toml" || fail "go-test check not added"
export FAKE_COUNTER="$root/go-counter"
go_assemble() { # go_assemble CODE; env FAKE_* set by the caller
  rm -f "$FAKE_COUNTER"; rm -rf "$root/tmp"; mkdir "$root/tmp"
  expect_code "$1" env TMPDIR="$root/tmp" PATH="$root/gobin:$PATH" "$bin" --config "$root/config-go.toml" assemble --no-fetch
  golog="$(cat "$root"/tmp/jj-fork-logs/*/fork-branch.log)"
}

FAKE_PASS_ON= FAKE_UPSTREAM=pass go_assemble 20
[[ "$(cat "$FAKE_COUNTER")" == 3 ]] || fail "expected exactly 3 retries"
contains "$golog" "fails every retry here but passes 5 runs on upstream"
contains "$out" "fails go checks"
ok "a go test that fails every retry and passes upstream blocks"

FAKE_PASS_ON= FAKE_UPSTREAM=fail go_assemble 0
contains "$golog" "example.com/m TestFlaky also fails on upstream; ignoring"
contains "$out" "note: also fails on upstream, ignored: example.com/m TestFlaky"
ok "a go test that also fails on upstream is ignored"

FAKE_PASS_ON=2 FAKE_UPSTREAM=pass go_assemble 0
[[ "$(cat "$FAKE_COUNTER")" == 2 ]] || fail "retrying should stop at the first pass"
contains "$golog" "note: flaky: example.com/m TestFlaky failed, then passed on retry"
ok "a go test that passes on a retry is flaky and does not block"

echo "all $pass scenarios passed"
